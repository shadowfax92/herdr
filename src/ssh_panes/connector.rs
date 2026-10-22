//! Local owner of an interactive SSH link and its private, unforwarded side channel.
//! SSH shares the pane's tty/process group; only SSH reads input until it exits.

use std::{collections::BTreeMap, io};

use super::config::SshPanesConfig;

#[derive(Debug, Clone, Default)]
pub(crate) struct ConnectOptions {
    pub host: String,
    pub cwd: Option<String>,
    pub no_sync: bool,
    pub ssh_args: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RunOptions {
    pub host: String,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub no_tty: bool,
    pub argv: Vec<String>,
}

pub(crate) fn check_host(host: &str, cfg: &SshPanesConfig) -> io::Result<()> {
    if host.is_empty()
        || host.starts_with('-')
        || host.chars().any(char::is_whitespace)
        || host.contains('\0')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid SSH host",
        ));
    }
    if !cfg.allowed_hosts.is_empty() && !cfg.allowed_hosts.iter().any(|h| h == host) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("herdr ssh: host {host} is not in [ssh_panes] allowed_hosts"),
        ));
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(not(unix))]
pub fn connect(_opts: ConnectOptions, _cfg: &SshPanesConfig) -> io::Result<i32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "ssh-panes: not supported on this platform",
    ))
}
#[cfg(not(unix))]
pub fn run_remote(_opts: RunOptions, _cfg: &SshPanesConfig) -> io::Result<i32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "ssh-panes: not supported on this platform",
    ))
}

#[cfg(unix)]
mod unix {
    use super::*;
    use crate::api::{
        client::{ApiClient, ConnectionTarget},
        schema::{Method, Request},
    };
    use crate::ssh_panes::{
        api::{AttachParams, DetachParams, ListParams, SshPanesRequest},
        remote::{self, RemoteSpec},
        sync,
    };
    use std::{
        fs,
        io::{Read, Write},
        os::unix::{
            fs::PermissionsExt,
            process::{CommandExt, ExitStatusExt},
        },
        path::Path,
        process::{Command, ExitStatus, Stdio},
        sync::atomic::{AtomicI32, Ordering},
        time::{Duration, Instant},
    };

    pub(crate) fn new_link() -> io::Result<String> {
        let mut bytes = [0u8; 12];
        fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// All control traffic has a deadline and an explicit socket. In particular the
    /// remote watcher must never fall back to a remote machine's default server.
    pub(crate) fn rpc(socket: &Path, params: SshPanesRequest) -> io::Result<serde_json::Value> {
        let value = ApiClient::for_target(ConnectionTarget::SocketPath(socket.into()))
            .request_value_with_timeout(
                &Request {
                    id: "ssh-panes".into(),
                    method: Method::SshPanes(params),
                },
                Duration::from_millis(1500),
            )
            .map_err(io::Error::other)?;
        if let Some(error) = value.get("error") {
            return Err(io::Error::other(
                error["message"]
                    .as_str()
                    .unwrap_or("SSH panes request rejected"),
            ));
        }
        Ok(value["result"]["ssh_panes"].clone())
    }

    pub(crate) fn spec(
        host: &str,
        cfg: &SshPanesConfig,
        cwd: Option<String>,
        link: String,
        digest: String,
    ) -> io::Result<RemoteSpec> {
        Ok(RemoteSpec {
            v: 1,
            socket: format!("/tmp/herdr-sp-{link}.sock"),
            link,
            server_key: remote::server_key(
                &server_id(&crate::config::config_dir().join("ssh-panes"))?,
                &crate::api::socket_path(),
            ),
            host: host.into(),
            pane_id: std::env::var("HERDR_PANE_ID").unwrap_or_default(),
            workspace_id: std::env::var("HERDR_WORKSPACE_ID").unwrap_or_default(),
            tab_id: std::env::var("HERDR_TAB_ID").unwrap_or_default(),
            cwd,
            payload_digest: digest,
            env: BTreeMap::from([("NVIM_APPNAME".into(), cfg.sync.nvim_appname.clone())]),
        })
    }

    /// The seed belongs to this local installation, not its DHCP hostname. The
    /// socket is hashed separately so named servers still own distinct scratch.
    pub(crate) fn server_id(dir: &Path) -> io::Result<String> {
        use std::os::fd::AsRawFd;
        remote::private_dir(dir)?;
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("server-id.lock"))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let path = dir.join("server-id");
        match fs::read_to_string(&path) {
            Ok(id) if id.len() == 24 && id.bytes().all(|b| b.is_ascii_hexdigit()) => Ok(id),
            Ok(_) => Err(io::Error::other("invalid ssh-panes/server-id")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let id = new_link()?;
                let temp = dir.join(format!(".server-id-{id}"));
                fs::write(&temp, &id)?;
                fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))?;
                let result = fs::rename(&temp, path);
                let _ = fs::remove_file(temp);
                result?;
                Ok(id)
            }
            Err(e) => Err(e),
        }
        // Closing the file releases flock on every return/error path.
    }

    /// Side-channel SSH/rsync must remain interruptible even if their caller has
    /// inherited ignored terminal signals. The interactive owner is scoped below.
    pub(crate) fn reset_child_signals(command: &mut Command) {
        unsafe {
            command.pre_exec(|| {
                for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP] {
                    libc::signal(signal, libc::SIG_DFL);
                }
                Ok(())
            });
        }
    }

    fn initial_attach(socket: &Path, attach: &SshPanesRequest, spawned: bool) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut delay = Duration::from_millis(100);
        loop {
            match rpc(socket, attach.clone()) {
                Ok(_) => return Ok(()),
                Err(error) if !spawned || Instant::now() >= deadline => return Err(error),
                Err(_) => {
                    // Restored connectors start inside App::try_new, before the
                    // API loop can answer. Keep launch_argv alive through startup.
                    std::thread::sleep(
                        delay.min(deadline.saturating_duration_since(Instant::now())),
                    );
                    delay = (delay * 2).min(Duration::from_secs(1));
                }
            }
        }
    }

    fn link_reported(socket: &Path, link: &str) -> Option<bool> {
        let result = rpc(socket, SshPanesRequest::List(ListParams::default())).ok()?;
        let link = result["links"]
            .as_array()?
            .iter()
            .find(|v| v["link_id"] == link)?;
        Some(link["last_report_ms_ago"].as_u64().is_some())
    }

    /// Only these per-link control records are consumed from SSH stdout. Every
    /// other byte is forwarded immediately, including prompts without newlines.
    /// Completion is emitted by a remote /bin/sh supervisor after the login shell
    /// exits; a broken SSH transport cannot emit that completion record.
    #[derive(Default)]
    struct ShellEvidence {
        ready: bool,
        stale: bool,
        completed: Option<i32>,
    }
    fn relay_output(
        mut input: impl Read,
        link: &str,
        mut output: impl Write,
    ) -> io::Result<ShellEvidence> {
        let prefix = format!("\x1b]777;herdr-sp-{link};").into_bytes();
        let mut pending = Vec::new();
        let mut evidence = ShellEvidence::default();
        let mut buffer = [0u8; 8192];
        loop {
            let count = input.read(&mut buffer)?;
            if count == 0 {
                output.write_all(&pending)?;
                output.flush()?;
                return Ok(evidence);
            }
            pending.extend_from_slice(&buffer[..count]);
            loop {
                if let Some(index) = pending.windows(prefix.len()).position(|w| w == prefix) {
                    output.write_all(&pending[..index])?;
                    pending.drain(..index);
                    if let Some(end) = pending.iter().position(|b| *b == 7) {
                        let event = &pending[prefix.len()..end];
                        match event {
                            b"ready" => evidence.ready = true,
                            b"stale" | b"missing" => evidence.stale = true,
                            _ => {
                                let code = std::str::from_utf8(event)
                                    .ok()
                                    .and_then(|s| s.strip_prefix("done;"))
                                    .and_then(|s| s.parse().ok());
                                if code.is_some() {
                                    evidence.completed = code;
                                } else {
                                    output.write_all(&pending[..=end])?;
                                }
                            }
                        }
                        pending.drain(..=end);
                    } else if pending.len() <= prefix.len() + 32 {
                        break;
                    } else {
                        output.write_all(&pending[..1])?;
                        pending.drain(..1);
                    }
                } else {
                    let keep = (1..prefix.len())
                        .rev()
                        .find(|n| pending.ends_with(&prefix[..*n]))
                        .unwrap_or(0);
                    output.write_all(&pending[..pending.len() - keep])?;
                    pending.drain(..pending.len() - keep);
                    break;
                }
            }
            output.flush()?;
        }
    }

    /// Only the executable path crosses the remote login shell; the JSON spec and
    /// run argv travel as hex, so fish never interprets user cwd/env/argument bytes.
    pub(crate) fn remote_command(
        cfg: &SshPanesConfig,
        verb: &str,
        spec: &RemoteSpec,
    ) -> io::Result<String> {
        sync::validate_remote_root(&cfg.remote_root)?;
        let payload = format!("{}/bin/herdr", remote_path(&cfg.remote_root)?);
        let command = format!("{payload} ssh {verb} --spec {}", spec.encode()?);
        if verb != "remote-attach" {
            return Ok(command);
        }
        // POSIX sh owns only exit reporting; fish still owns the interactive tty
        // and watcher shell PID. Markers use the random link id, never user data.
        let prefix = format!("\\033]777;herdr-sp-{};", spec.link);
        Ok(format!("/bin/sh -c {}", quote(&format!(
            "if test -x {payload}; then {command}; status=$?; else printf '{prefix}missing\\007'; status=127; fi; printf '{prefix}done;%s\\007' \"$status\"; exit \"$status\""
        ))))
    }

    pub(crate) fn quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    /// Fish and POSIX shells both accept these quoted path segments. A leading
    /// tilde stays outside quotes so sshd's login shell expands remote HOME.
    pub(crate) fn remote_path(path: &str) -> io::Result<String> {
        if path.contains(['\0', '\n', '\r']) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote path contains a control character",
            ));
        }
        Ok(if let Some(rest) = path.strip_prefix("~/") {
            format!("~/{rest}", rest = quote(rest))
        } else {
            quote(path)
        })
    }

    pub(crate) fn ssh_argv(
        opts: &ConnectOptions,
        cfg: &SshPanesConfig,
        spec: &RemoteSpec,
        socket: &Path,
    ) -> io::Result<Vec<String>> {
        let mut args: Vec<String> = [
            "-t",
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=4",
            "-R",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        args.push(format!("{}:{}", spec.socket, socket.display()));
        args.extend(cfg.ssh_options.clone());
        args.extend(opts.ssh_args.clone());
        args.push(opts.host.clone());
        args.push(remote_command(cfg, "remote-attach", spec)?);
        Ok(args)
    }

    // Use a relative spelling from the config directory. It resolves to the
    // contract's <config>/ssh-panes/cm/%C, but leaves room in sockaddr_un for
    // OpenSSH's temporary suffix even when the config directory is long.
    pub(crate) fn side_channel_opts(cfg: &SshPanesConfig) -> io::Result<Vec<String>> {
        let dir = crate::config::config_dir().join("ssh-panes/cm");
        fs::create_dir_all(&dir)?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        let mut args = vec![
            "-o".into(),
            "ControlMaster=auto".into(),
            "-o".into(),
            "ControlPersist=600".into(),
            "-o".into(),
            "ControlPath=ssh-panes/cm/%C".into(),
        ];
        args.extend(cfg.ssh_options.clone());
        Ok(args)
    }

    pub(crate) fn side_command(host: &str, cfg: &SshPanesConfig) -> io::Result<Command> {
        check_host(host, cfg)?;
        let mut command = Command::new("ssh");
        command
            .current_dir(crate::config::config_dir())
            .args(side_channel_opts(cfg)?)
            .arg(host);
        reset_child_signals(&mut command);
        Ok(command)
    }

    pub(crate) fn status_code(status: ExitStatus) -> i32 {
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
    }

    pub fn run_remote(opts: RunOptions, cfg: &SshPanesConfig) -> io::Result<i32> {
        check_host(&opts.host, cfg)?;
        if opts.argv.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote command is empty",
            ));
        }
        let mut spec = spec(
            &opts.host,
            cfg,
            opts.cwd,
            std::env::var("HERDR_SSH_PANES_LINK").unwrap_or(new_link()?),
            String::new(),
        )?;
        spec.env.extend(opts.env);
        // Keep argv in the hex data rather than reconstructing shell command text.
        spec.env.insert(
            "__HERDR_SP_ARGV".into(),
            serde_json::to_string(&opts.argv).map_err(io::Error::other)?,
        );
        let mut command = Command::new("ssh");
        command
            .current_dir(crate::config::config_dir())
            .args(side_channel_opts(cfg)?);
        if !opts.no_tty {
            command.arg("-t");
        }
        command
            .arg(&opts.host)
            .arg(remote_command(cfg, "remote-run", &spec)?);
        reset_child_signals(&mut command);
        Ok(status_code(command.status()?))
    }

    pub fn connect(opts: ConnectOptions, cfg: &SshPanesConfig) -> io::Result<i32> {
        check_host(&opts.host, cfg)?;
        let pane = std::env::var("HERDR_PANE_ID").unwrap_or_default();
        if pane.is_empty() {
            eprintln!("herdr ssh: must run inside a herdr pane (HERDR_PANE_ID is unset)");
            return Ok(2);
        }
        eprintln!("connecting to {}…", opts.host);
        let socket = crate::api::socket_path();
        let mut retried_sync = false;
        loop {
            let digest = sync::local_manifest(cfg)?.digest;
            let spec = spec(&opts.host, cfg, opts.cwd.clone(), new_link()?, digest)?;
            let attach = SshPanesRequest::Attach(AttachParams {
                pane_id: pane.clone(),
                link_id: spec.link.clone(),
                host: opts.host.clone(),
                connector_pid: std::process::id(),
                connector_pgid: unsafe { libc::getpgrp() as u32 },
                cwd_hint: opts.cwd.clone(),
                protocol: 1,
            });
            initial_attach(
                &socket,
                &attach,
                std::env::var("HERDR_SSH_PANES_SPAWNED").as_deref() == Ok("1"),
            )?;
            let mut command = Command::new("ssh");
            command
                .args(ssh_argv(&opts, cfg, &spec, &socket)?)
                .stdout(Stdio::piped());
            reset_child_signals(&mut command);
            let result: io::Result<(i32, ShellEvidence)> = (|| {
                // Only interactive SSH owns Ctrl-C/Ctrl-\. Outside this scope,
                // connect retries, hashing, sync and authentication are cancellable.
                let _signals = SignalGuard::install()?;
                let mut child = command.spawn()?;
                let stdout = child
                    .stdout
                    .take()
                    .ok_or_else(|| io::Error::other("missing SSH stdout"))?;
                let link = spec.link.clone();
                let output =
                    std::thread::spawn(move || relay_output(stdout, &link, std::io::stdout()));
                let mut heartbeat = Instant::now();
                loop {
                    let signal = SIGNAL.swap(0, Ordering::Relaxed);
                    if signal != 0 {
                        unsafe {
                            libc::kill(child.id() as i32, signal);
                        }
                    }
                    if let Some(status) = child.try_wait()? {
                        let evidence = output
                            .join()
                            .map_err(|_| io::Error::other("SSH output relay panicked"))??;
                        return Ok((status_code(status), evidence));
                    }
                    if heartbeat.elapsed() >= Duration::from_secs(10) {
                        let _ = rpc(&socket, attach.clone());
                        heartbeat = Instant::now();
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            })();
            // Query before detach destroys the evidence. Once the remote shell
            // reported, its last command's status (including 3/127/255) is user
            // output, not a payload/setup signal. Old payloads still use exit 3.
            let reported = link_reported(&socket, &spec.link);
            let code = result.as_ref().ok().map(|(code, _)| *code);
            let _ = rpc(
                &socket,
                SshPanesRequest::Detach(DetachParams {
                    link_id: spec.link,
                    exit_code: code,
                }),
            );
            let (code, evidence) = result?;
            let setup_failed = !evidence.ready
                && reported != Some(true)
                && (evidence.stale || reported == Some(false));
            if setup_failed && matches!(code, 3 | 127) {
                if cfg.auto_sync && !opts.no_sync && !retried_sync {
                    eprintln!(
                        "ssh-panes: remote payload on {} is stale; syncing…",
                        opts.host
                    );
                    if let Err(error) = sync::ensure_synced(&opts.host, cfg, code == 127) {
                        eprintln!("ssh-panes: sync failed: {error}");
                        return Ok(1);
                    }
                    retried_sync = true;
                    continue;
                }
                eprintln!(
                    "ssh-panes: run `herdr ssh sync {}` to update the remote payload",
                    opts.host
                );
            }
            if code == 255 && evidence.completed != Some(255) {
                eprintln!("ssh-panes: lost connection to {} (ssh 255)", opts.host);
                if std::env::var("HERDR_SSH_PANES_SPAWNED").as_deref() == Ok("1") {
                    eprintln!("[r] reconnect  [enter] close");
                    let mut answer = String::new();
                    std::io::stdin().read_line(&mut answer)?;
                    if answer.trim() == "r" {
                        retried_sync = false;
                        continue;
                    }
                }
            }
            return Ok(code);
        }
    }

    static SIGNAL: AtomicI32 = AtomicI32::new(0);
    extern "C" fn terminate(signal: i32) {
        SIGNAL.store(signal, Ordering::Relaxed);
    }

    /// Scoped tty/signal ownership belongs to the connector, never the server.
    /// Restoring termios also covers an SSH child killed before its own cleanup.
    struct SignalGuard {
        old: Vec<(i32, libc::sigaction)>,
        tty: Option<libc::termios>,
    }
    impl SignalGuard {
        fn install() -> io::Result<Self> {
            let mut guard = Self {
                old: Vec::new(),
                tty: None,
            };
            unsafe {
                let mut tty = std::mem::zeroed();
                if libc::tcgetattr(0, &mut tty) == 0 {
                    guard.tty = Some(tty);
                }
                for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP] {
                    let mut action: libc::sigaction = std::mem::zeroed();
                    action.sa_sigaction = if matches!(signal, libc::SIGINT | libc::SIGQUIT) {
                        libc::SIG_IGN
                    } else {
                        terminate as *const () as usize
                    };
                    libc::sigemptyset(&mut action.sa_mask);
                    let mut old = std::mem::zeroed();
                    if libc::sigaction(signal, &action, &mut old) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    guard.old.push((signal, old));
                }
            }
            Ok(guard)
        }
        fn restore_tty(&self) {
            if let Some(tty) = &self.tty {
                unsafe {
                    libc::tcsetattr(0, libc::TCSANOW, tty);
                }
            }
        }
    }
    /// Return the tty to local output without rewinding the remote transcript.
    /// Save on the active screen first: on main this refreshes a stale cursor
    /// slot; on alternate it leaves main's 1049h return position intact. 1047l
    /// copies the alternate cursor onto main, so restore main's safe slot before
    /// CRLF separates remote output from the next connector message/local prompt.
    fn reset_terminal(mut output: impl Write) -> io::Result<()> {
        output.write_all(
            b"\x1b7\x1b[?1047l\x1b8\x1b[<u\x1b[?1000;1002;1003;1006l\x1b[?2004l\x1b[?25h\r\n",
        )?;
        output.flush()
    }

    impl Drop for SignalGuard {
        fn drop(&mut self) {
            self.restore_tty();
            // A dead SSH child cannot undo remote fullscreen/input modes. Reset
            // before any detach error, sync message, reconnect or local prompt.
            let _ = reset_terminal(std::io::stdout().lock());
            for (signal, old) in &self.old {
                unsafe {
                    libc::sigaction(*signal, old, std::ptr::null_mut());
                }
            }
        }
    }
    #[cfg(test)]
    mod output_tests {
        use super::*;

        #[test]
        fn ssh_panes_reset_avoids_1049_and_preserves_input_cleanup() {
            let mut output = Vec::new();
            reset_terminal(&mut output).unwrap();
            assert!(!output.windows(8).any(|bytes| bytes == b"\x1b[?1049l"));
            for mode in [
                &b"\x1b[?1047l"[..],
                &b"\x1b[<u"[..],
                &b"\x1b[?1000;1002;1003;1006l"[..],
                &b"\x1b[?2004l"[..],
                &b"\x1b[?25h"[..],
            ] {
                assert!(output.windows(mode.len()).any(|bytes| bytes == mode));
            }
            assert!(output.ends_with(b"\r\n"));
        }

        #[test]
        fn ssh_panes_reset_preserves_transcript_before_local_output() {
            let mut reset = Vec::new();
            reset_terminal(&mut reset).unwrap();
            for alternate in [false, true] {
                let mut terminal = crate::ghostty::Terminal::new(100, 12, 1024).unwrap();
                // A shell may save its cursor before any remote output. Leaving
                // an already inactive alt screen must not restore that stale row.
                terminal.write(b"connecting to studio...\r\n\x1b7");
                terminal.write(b"REMOTE-MARKER-1\r\nREMOTE-MARKER-2\r\nremote> ");
                if alternate {
                    terminal.write(b"\x1b[?1049h\x1b[Hfullscreen app\x1b[?2004h\x1b[?25l");
                }
                terminal.write(&reset);
                terminal.write(b"ssh-panes: lost connection to studio (ssh 255)\r\nlocal> ");
                let rows: Vec<String> = terminal
                    .screen_text_rows()
                    .unwrap()
                    .iter()
                    .map(|row| {
                        row.cells
                            .iter()
                            .flat_map(|cell| cell.graphemes.iter().copied())
                            .filter_map(char::from_u32)
                            .collect::<String>()
                            .trim_end()
                            .to_string()
                    })
                    .collect();
                assert_eq!(rows[1], "REMOTE-MARKER-1", "{rows:?}");
                assert_eq!(rows[2], "REMOTE-MARKER-2", "{rows:?}");
                assert_eq!(rows[3], "remote>", "{rows:?}");
                assert_eq!(rows[4], "ssh-panes: lost connection to studio (ssh 255)");
                assert_eq!(rows[5], "local>");
            }
        }

        struct ByteReader(std::io::Cursor<Vec<u8>>);
        impl Read for ByteReader {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                self.0.read(&mut bytes[..1])
            }
        }
        #[test]
        fn ssh_panes_relay_preserves_bytes_and_consumes_fragmented_shell_markers() {
            let link = "0123456789abcdef01234567";
            let input = format!("prompt> \x1b]777;herdr-sp-{link};ready\x07bytes\r\n\x1b]777;herdr-sp-{link};done;255\x07");
            let mut output = Vec::new();
            let evidence = relay_output(
                ByteReader(std::io::Cursor::new(input.into_bytes())),
                link,
                &mut output,
            )
            .unwrap();
            assert_eq!(output, b"prompt> bytes\r\n");
            assert!(evidence.ready);
            assert!(!evidence.stale);
            assert_eq!(evidence.completed, Some(255));
        }
        #[test]
        fn ssh_panes_relay_preserves_other_osc_and_partial_records_at_eof() {
            let input =
                b"\x1b]title;abc\x07prompt> \x1b]777;herdr-sp-x;unknown\x07end\x1b]777;herdr-sp-x;";
            let mut output = Vec::new();
            let evidence = relay_output(&input[..], "x", &mut output).unwrap();
            assert_eq!(output, input);
            assert!(!evidence.ready);
            assert_eq!(evidence.completed, None);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::ssh_panes::remote::RemoteSpec;
    use std::path::Path;

    #[test]
    fn ssh_panes_interactive_argv_is_private_and_spec_is_shell_safe() {
        let opts = ConnectOptions {
            host: "studio".into(),
            ssh_args: vec!["-p".into(), "2222".into()],
            ..Default::default()
        };
        let cfg = SshPanesConfig {
            ssh_options: vec!["-o".into(), "ConnectTimeout=10".into()],
            ..Default::default()
        };
        let spec = RemoteSpec {
            v: 1,
            link: "1234567890abcdef12345678".into(),
            socket: "/tmp/herdr-sp-1234567890abcdef12345678.sock".into(),
            server_key: "1234567890abcdef".into(),
            host: "studio".into(),
            pane_id: "w1:p1".into(),
            workspace_id: "w1".into(),
            tab_id: "w1:t1".into(),
            cwd: Some("/spaces ' $(do not run)".into()),
            payload_digest: "hash".into(),
            env: BTreeMap::new(),
        };
        let args = ssh_argv(&opts, &cfg, &spec, Path::new("/tmp/local.sock")).unwrap();
        assert_eq!(
            &args[..13],
            [
                "-t",
                "-o",
                "ControlMaster=no",
                "-o",
                "ControlPath=none",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=4",
                "-R",
                "/tmp/herdr-sp-1234567890abcdef12345678.sock:/tmp/local.sock"
            ]
        );
        assert_eq!(
            &args[13..18],
            ["-o", "ConnectTimeout=10", "-p", "2222", "studio"]
        );
        let command = args.last().unwrap();
        assert!(!command.contains("$("));
        assert!(command.contains(&spec.encode().unwrap()));
        let a = new_link().unwrap();
        let b = new_link().unwrap();
        assert_eq!(a.len(), 24);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn ssh_panes_server_seed_is_persistent_and_named_sessions_remain_distinct() {
        let dir = std::env::temp_dir().join(format!("herdr-sp-test-{}", new_link().unwrap()));
        let one = server_id(&dir).unwrap();
        assert_eq!(one, server_id(&dir).unwrap());
        assert_eq!(std::fs::read_to_string(dir.join("server-id")).unwrap(), one);
        let a = crate::ssh_panes::remote::server_key(&one, Path::new("/session-a.sock"));
        let b = crate::ssh_panes::remote::server_key(&one, Path::new("/session-b.sock"));
        assert_ne!(a, b);
        std::fs::write(dir.join("server-id"), "corrupt").unwrap();
        assert!(server_id(&dir).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ssh_panes_host_allowlist_and_unsafe_host_usage() {
        let cfg = SshPanesConfig {
            allowed_hosts: vec!["studio".into()],
            ..Default::default()
        };
        assert!(check_host("studio", &cfg).is_ok());
        assert_eq!(
            check_host("other", &cfg).unwrap_err().to_string(),
            "herdr ssh: host other is not in [ssh_panes] allowed_hosts"
        );
        for bad in ["", "-oProxyCommand=bad", "studio extra", "studio\n"] {
            assert_eq!(
                check_host(bad, &cfg).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }
}
