use super::*;
use crate::{events::AppEvent, pane::PaneRuntime};
use tokio::sync::mpsc;

async fn wait_for(events: &mut mpsc::Receiver<AppEvent>, expected: impl Fn(&AppEvent) -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(event) = events.recv().await {
            if expected(&event) {
                return;
            }
        }
        panic!("detection task closed before expected event");
    })
    .await
    .expect("detection loop must publish the remote observation");
}

async fn detects_then_exits(basic: bool) {
    let (mut runtime, mut events, guard) = if basic {
        let (mut runtime, _) = PaneRuntime::test_with_channel(80, 24);
        runtime.child_pid.store(2_000_000_001, Ordering::Release);
        let events = runtime.test_start_basic_detection();
        (runtime, events, None)
    } else {
        let (runtime, events, guard) = test_support::normal_runtime(PaneId::alloc());
        (runtime, events, Some(guard))
    };
    let pane = runtime.ssh_probe_key();
    link(pane, 42);
    test_support::foreground(pane, Some(42));
    store(
        pane,
        &RemoteForeground {
            process_group_id: Some(90),
            foreground_is_pane_shell: false,
            agent: Some("claude".into()),
            process_name: Some("claude".into()),
        },
        Instant::now(),
    );
    wait_for(&mut events, |event| matches!(event, AppEvent::AgentProcessDetected { pane_id, agent: Agent::Claude, .. } if *pane_id == pane)).await;
    assert_eq!(remote_agent(pane), Some(Some(Agent::Claude)));
    store(
        pane,
        &RemoteForeground {
            process_group_id: Some(80),
            foreground_is_pane_shell: true,
            agent: None,
            process_name: Some("fish".into()),
        },
        Instant::now(),
    );
    wait_for(&mut events, |event| matches!(event, AppEvent::StateChanged { pane_id, process_exited: true, .. } if *pane_id == pane)).await;
    assert_eq!(remote_agent(pane), Some(None));
    // A pgid mismatch invalidates the override and prompt hook on this tick.
    test_support::foreground(pane, Some(43));
    assert!(remote_agent(pane).is_none());
    unlink(pane);
    if let Some(handle) = runtime.detect_handle.take() {
        handle.abort();
    }
    drop(runtime);
    drop(guard);
}

#[tokio::test]
async fn ssh_panes_basic_detection_emits_remote_agent_then_process_exited() {
    let _table = crate::pane::ssh_probe::TEST_TABLE.lock().await;
    detects_then_exits(true).await;
}

#[tokio::test]
async fn ssh_panes_normal_detection_emits_remote_agent_then_process_exited() {
    let _table = crate::pane::ssh_probe::TEST_TABLE.lock().await;
    detects_then_exits(false).await;
}

async fn detects_then_watcher_silent(basic: bool) {
    let (mut runtime, mut events, guard) = if basic {
        let (mut runtime, _) = PaneRuntime::test_with_channel(80, 24);
        runtime
            .child_pid
            .store(test_support::TEST_PID, Ordering::Release);
        let events = runtime.test_start_basic_detection();
        (runtime, events, None)
    } else {
        let (runtime, events, guard) = test_support::normal_runtime(PaneId::alloc());
        (runtime, events, Some(guard))
    };
    let pane = runtime.ssh_probe_key();
    link(pane, 42);
    test_support::foreground(pane, Some(42));
    let foreground = RemoteForeground {
        process_group_id: Some(90),
        foreground_is_pane_shell: false,
        agent: Some("claude".into()),
        process_name: Some("claude".into()),
    };
    store(pane, &foreground, Instant::now());
    wait_for(&mut events, |event| matches!(event, AppEvent::AgentProcessDetected { pane_id, agent: Agent::Claude, .. } if *pane_id == pane)).await;
    store(
        pane,
        &foreground,
        Instant::now() - crate::ssh_panes::SOFT_LEASE,
    );
    heartbeat(pane, Instant::now());
    assert_eq!(remote_agent(pane), Some(None));
    wait_for(&mut events, |event| matches!(event, AppEvent::StateChanged { pane_id, agent: None, .. } if *pane_id == pane)).await;
    unlink(pane);
    if let Some(handle) = runtime.detect_handle.take() {
        handle.abort();
    }
    drop(runtime);
    drop(guard);
}

#[tokio::test]
async fn ssh_panes_basic_detection_clears_identity_when_watcher_silent_despite_heartbeat() {
    let _table = TEST_TABLE.lock().await;
    detects_then_watcher_silent(true).await;
}

#[tokio::test]
async fn ssh_panes_normal_detection_clears_identity_when_watcher_silent_despite_heartbeat() {
    let _table = TEST_TABLE.lock().await;
    detects_then_watcher_silent(false).await;
}
