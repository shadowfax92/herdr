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
        api::{AttachParams, DetachParams, SshPanesRequest},
        remote::{self, RemoteSpec},
        sync,
    };
    use std::{
        fs,
        io::Read,
        os::unix::{
            fs::PermissionsExt,
            process::{CommandExt, ExitStatusExt},
        },
        path::Path,
        process::{Command, ExitStatus},
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
    ) -> RemoteSpec {
        RemoteSpec {
            v: 1,
            socket: format!("/tmp/herdr-sp-{link}.sock"),
            link,
            server_key: remote::server_key(
                &crate::platform::hostname().unwrap_or_default(),
                &crate::api::socket_path(),
            ),
            host: host.into(),
            pane_id: std::env::var("HERDR_PANE_ID").unwrap_or_default(),
            workspace_id: std::env::var("HERDR_WORKSPACE_ID").unwrap_or_default(),
            tab_id: std::env::var("HERDR_TAB_ID").unwrap_or_default(),
            cwd,
            payload_digest: digest,
            env: BTreeMap::from([("NVIM_APPNAME".into(), cfg.sync.nvim_appname.clone())]),
        }
    }

    /// Only the executable path crosses the remote login shell; the JSON spec and
    /// run argv travel as hex, so fish never interprets user cwd/env/argument bytes.
    pub(crate) fn remote_command(
        cfg: &SshPanesConfig,
        verb: &str,
        spec: &RemoteSpec,
    ) -> io::Result<String> {
        Ok(format!(
            "{}/bin/herdr ssh {verb} --spec {}",
            remote_path(&cfg.remote_root)?,
            spec.encode()?
        ))
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
        );
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
        let signals = SignalGuard::install()?;
        let mut retried_sync = false;
        loop {
            let digest = sync::local_manifest(cfg)?.digest;
            let spec = spec(&opts.host, cfg, opts.cwd.clone(), new_link()?, digest);
            let attach = SshPanesRequest::Attach(AttachParams {
                pane_id: pane.clone(),
                link_id: spec.link.clone(),
                host: opts.host.clone(),
                connector_pid: std::process::id(),
                connector_pgid: unsafe { libc::getpgrp() as u32 },
                cwd_hint: opts.cwd.clone(),
                protocol: 1,
            });
            rpc(&socket, attach.clone())?;
            let mut command = Command::new("ssh");
            command.args(ssh_argv(&opts, cfg, &spec, &socket)?);
            // Parent catches termination for detach; child restores normal SSH
            // signal handling. No setpgid: SIGWINCH/job control reach both.
            unsafe {
                command.pre_exec(|| {
                    for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP] {
                        libc::signal(signal, libc::SIG_DFL);
                    }
                    Ok(())
                });
            }
            let result: io::Result<i32> = (|| {
                let mut child = command.spawn()?;
                let mut heartbeat = Instant::now();
                loop {
                    let signal = SIGNAL.swap(0, Ordering::Relaxed);
                    if signal != 0 {
                        unsafe {
                            libc::kill(child.id() as i32, signal);
                        }
                    }
                    if let Some(status) = child.try_wait()? {
                        return Ok(status_code(status));
                    }
                    if heartbeat.elapsed() >= Duration::from_secs(10) {
                        let _ = rpc(&socket, attach.clone());
                        heartbeat = Instant::now();
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            })();
            let code = result.as_ref().ok().copied();
            let _ = rpc(
                &socket,
                SshPanesRequest::Detach(DetachParams {
                    link_id: spec.link,
                    exit_code: code,
                }),
            );
            let code = result?;
            if matches!(code, 3 | 127) {
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
            if code == 255 {
                eprintln!("ssh-panes: lost connection to {} (ssh 255)", opts.host);
                if std::env::var("HERDR_SSH_PANES_SPAWNED").as_deref() == Ok("1") {
                    signals.restore_tty();
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
    impl Drop for SignalGuard {
        fn drop(&mut self) {
            self.restore_tty();
            for (signal, old) in &self.old {
                unsafe {
                    libc::sigaction(*signal, old, std::ptr::null_mut());
                }
            }
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
        assert_eq!(
            RemoteSpec::decode(command.split_whitespace().last().unwrap()).unwrap(),
            spec
        );
        let a = new_link().unwrap();
        let b = new_link().unwrap();
        assert_eq!(a.len(), 24);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a, b);
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
