//! Socket-only backend used by SSH core regression tests. It exercises the real
//! normal/handoff detection loops without launching terminals or real children.

use crate::{
    events::AppEvent,
    layout::PaneId,
    pane::{AgentDetection, PaneLaunchEnv, PaneRuntime},
    render_signal::RenderSignal,
};
use portable_pty::{Child, ChildKiller, CommandBuilder, ExitStatus};
use std::{
    io,
    os::{fd::OwnedFd, unix::net::UnixStream},
    sync::{Arc, Condvar, Mutex},
};
use tokio::sync::{mpsc, Notify};

const TEST_COMMAND: &str = "/__herdr_ssh_panes_no_pty__";
pub(crate) const TEST_PID: u32 = 2_000_000_001;

#[derive(Debug)]
struct SocketChild {
    stopped: Arc<(Mutex<bool>, Condvar)>,
}
impl ChildKiller for SocketChild {
    fn kill(&mut self) -> io::Result<()> {
        let (lock, notify) = &*self.stopped;
        *lock.lock().unwrap() = true;
        notify.notify_all();
        Ok(())
    }
    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(Self {
            stopped: self.stopped.clone(),
        })
    }
}
impl Child for SocketChild {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        Ok((*self.stopped.0.lock().unwrap()).then(|| ExitStatus::with_exit_code(0)))
    }
    fn wait(&mut self) -> io::Result<ExitStatus> {
        let (lock, notify) = &*self.stopped;
        let mut stopped = lock.lock().unwrap();
        while !*stopped {
            stopped = notify.wait(stopped).unwrap();
        }
        Ok(ExitStatus::with_exit_code(0))
    }
    fn process_id(&self) -> Option<u32> {
        Some(TEST_PID)
    }
}

thread_local! {
    static CAPTURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static SOCKET: std::cell::RefCell<Option<(TestChildGuard, CommandBuilder)>> = const { std::cell::RefCell::new(None) };
}

pub(crate) struct TestChildGuard {
    child: SocketChild,
    _peer: UnixStream,
}
impl Drop for TestChildGuard {
    fn drop(&mut self) {
        self.child.kill().unwrap();
    }
}

pub(crate) fn spawn_with_portable_pty(
    rows: u16,
    cols: u16,
    cmd: CommandBuilder,
) -> io::Result<crate::pty::backend::SpawnedPty> {
    if !CAPTURE.with(|enabled| enabled.replace(false))
        && cmd.get_argv().first().is_none_or(|arg| arg != TEST_COMMAND)
    {
        return crate::pty::backend::spawn_with_portable_pty(rows, cols, cmd);
    }
    let (master, peer) = UnixStream::pair()?;
    let stopped = Arc::new((Mutex::new(false), Condvar::new()));
    SOCKET.with(|slot| {
        *slot.borrow_mut() = Some((
            TestChildGuard {
                child: SocketChild {
                    stopped: stopped.clone(),
                },
                _peer: peer,
            },
            cmd,
        ))
    });
    Ok(crate::pty::backend::SpawnedPty {
        master_fd: OwnedFd::from(master),
        child: Box::new(SocketChild { stopped }),
    })
}

/// Keep the peer and fake wait handle alive until the test has finished. Runtime
/// drops must preserve processes: the synthetic PID must never reach OS signals.
pub(crate) fn normal_runtime(
    pane: PaneId,
) -> (PaneRuntime, mpsc::Receiver<AppEvent>, TestChildGuard) {
    let (events, receiver) = mpsc::channel(64);
    let mut runtime = PaneRuntime::spawn_argv_command(
        pane,
        24,
        80,
        std::env::temp_dir(),
        &[TEST_COMMAND.into()],
        &PaneLaunchEnv::default(),
        AgentDetection::Enabled,
        0,
        crate::terminal_theme::TerminalTheme::default(),
        None,
        events,
        Arc::new(Notify::new()),
        Arc::new(RenderSignal::default()),
    )
    .unwrap();
    runtime.preserve_processes_on_drop = true;
    let guard = SOCKET.with(|slot| slot.borrow_mut().take().unwrap().0);
    (runtime, receiver, guard)
}

pub(crate) fn foreground(pane: PaneId, pgid: Option<u32>) {
    super::test_owner(pane, true, pgid);
}

pub(crate) fn normal_terminal_runtime(
    pane: PaneId,
) -> (
    crate::terminal::TerminalRuntime,
    mpsc::Receiver<AppEvent>,
    TestChildGuard,
) {
    let (events, receiver) = mpsc::channel(64);
    let runtime = crate::terminal::TerminalRuntime::spawn_argv_command(
        pane,
        24,
        80,
        std::env::temp_dir(),
        &[TEST_COMMAND.into()],
        &PaneLaunchEnv::default(),
        AgentDetection::Enabled,
        0,
        crate::terminal_theme::TerminalTheme::default(),
        None,
        events,
        Arc::new(Notify::new()),
        Arc::new(RenderSignal::default()),
    )
    .unwrap();
    let guard = SOCKET.with(|slot| slot.borrow_mut().take().unwrap().0);
    (runtime, receiver, guard)
}

/// Intercept exactly one synchronous Workspace spawn on this test thread.
/// The captured CommandBuilder lets assertions cover upstream env/argv plumbing.
pub(crate) fn capture_next_spawn() {
    CAPTURE.with(|enabled| enabled.set(true));
}
pub(crate) fn take_spawn() -> (TestChildGuard, CommandBuilder) {
    SOCKET.with(|slot| slot.borrow_mut().take().unwrap())
}
