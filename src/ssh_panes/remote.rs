//! Remote payload entry points and the hex JSON handoff across the SSH command boundary.
//! Each watcher owns one forward; a per-server symlink routes persistent scratch
//! clients through any surviving link without a remote Herdr server.

use std::{collections::BTreeMap, io, path::Path};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Versioned data handoff; shell-sensitive cwd/env values never become SSH syntax.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RemoteSpec {
    pub v: u32,
    pub link: String,
    pub socket: String,
    pub server_key: String,
    pub host: String,
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub cwd: Option<String>,
    pub payload_digest: String,
    pub env: BTreeMap<String, String>,
}

impl RemoteSpec {
    pub fn encode(&self) -> io::Result<String> {
        let bytes = serde_json::to_vec(self).map_err(io::Error::other)?;
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 15) as usize] as char);
        }
        Ok(encoded)
    }

    pub fn decode(hex: &str) -> io::Result<Self> {
        if !hex.len().is_multiple_of(2) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote spec must be hex-encoded JSON",
            ));
        }
        let bytes = hex
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let digit = |byte: u8| (byte as char).to_digit(16).map(|value| value as u8);
                match (digit(pair[0]), digit(pair[1])) {
                    (Some(high), Some(low)) => Ok(high * 16 + low),
                    _ => Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "remote spec contains non-hex bytes",
                    )),
                }
            })
            .collect::<io::Result<Vec<_>>>()?;
        let spec: Self = serde_json::from_slice(&bytes)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
        if spec.v != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported remote spec version",
            ));
        }
        Ok(spec)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct WatchArgs {
    pub link: String,
    pub shell_pid: u32,
    pub server_key: String,
    pub socket: String,
}

#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(not(unix))]
pub fn remote_attach(_spec: &str) -> io::Result<i32> {
    unsupported()
}
#[cfg(not(unix))]
pub fn remote_watch(_args: WatchArgs) -> io::Result<i32> {
    unsupported()
}
#[cfg(not(unix))]
pub fn remote_run(_spec: &str, _argv: &[String]) -> io::Result<i32> {
    unsupported()
}
#[cfg(not(unix))]
fn unsupported() -> io::Result<i32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "ssh-panes: not supported on this platform",
    ))
}

pub(crate) fn server_key(server_id: &str, socket: &Path) -> String {
    let mut digest = Sha256::new();
    digest.update(server_id.as_bytes());
    digest.update(b"\0");
    digest.update(socket.as_os_str().as_encoded_bytes());
    format!("{:x}", digest.finalize())[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_panes_remote_spec_round_trips_and_rejects_bad_input() {
        let spec = RemoteSpec {
            v: 1,
            link: "link".into(),
            socket: "/tmp/herdr-sp-link.sock".into(),
            server_key: "key".into(),
            host: "studio".into(),
            pane_id: "w1:p2".into(),
            workspace_id: "w1".into(),
            tab_id: "w1:t1".into(),
            cwd: Some("/remote/a ' λ".into()),
            payload_digest: "digest".into(),
            env: BTreeMap::from([("NVIM_APPNAME".into(), "herdr-sp-nvim".into())]),
        };
        let encoded = spec.encode().unwrap();
        assert_eq!(RemoteSpec::decode(&encoded).unwrap(), spec);
        assert_eq!(RemoteSpec::decode(&encoded.to_uppercase()).unwrap(), spec);
        for invalid in ["f", "xx", "ff", "7b7d"] {
            assert!(RemoteSpec::decode(invalid).is_err());
        }
        let future = RemoteSpec { v: 2, ..spec };
        assert!(RemoteSpec::decode(&future.encode().unwrap()).is_err());
        assert_ne!(
            server_key("a", Path::new("/sock")),
            server_key("b", Path::new("/sock"))
        );
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use crate::ssh_panes::{
        api::{RemoteForeground, ReportParams, SshPanesRequest},
        connector,
    };
    use std::{
        fs,
        os::{
            fd::AsRawFd,
            unix::{
                fs::{symlink, FileTypeExt, MetadataExt, PermissionsExt},
                net::UnixStream,
                process::CommandExt,
            },
        },
        path::PathBuf,
        process::{Command, Stdio},
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    #[derive(Serialize, Deserialize)]
    struct LinkRecord {
        socket: String,
        shell_pid: u32,
        started_at: u64,
    }

    pub(crate) fn home() -> io::Result<PathBuf> {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is unset"))
    }
    pub(crate) fn expand_home(value: &str) -> io::Result<PathBuf> {
        Ok(if let Some(rest) = value.strip_prefix("~/") {
            home()?.join(rest)
        } else {
            PathBuf::from(value)
        })
    }
    fn root() -> io::Result<PathBuf> {
        std::env::current_exe()?
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .ok_or_else(|| io::Error::other("payload must live at <root>/bin/herdr"))
    }
    fn validate(spec: &RemoteSpec, attach: bool) -> io::Result<()> {
        let hex = |s: &str, len| s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit());
        if !hex(&spec.server_key, 16)
            || (attach
                && (!hex(&spec.link, 24)
                    || spec.socket != format!("/tmp/herdr-sp-{}.sock", spec.link)))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid remote link identity",
            ));
        }
        Ok(())
    }
    pub(crate) fn private_dir(path: &Path) -> io::Result<()> {
        fs::create_dir_all(path)?;
        let meta = fs::symlink_metadata(path)?;
        if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "SSH panes state directory is not owned by this user",
            ));
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
    }
    // Derive state from the payload root so a -dev payload can never share
    // sockets or persistent scratch data with the production SSH-pane payload.
    fn state_root(root: &Path, home: &Path) -> PathBuf {
        home.join(".local/state").join(
            root.file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new("herdr-ssh-panes")),
        )
    }
    fn server_dir(key: &str) -> io::Result<PathBuf> {
        let base = state_root(&root()?, &home()?);
        private_dir(&base)?;
        private_dir(&base.join("s"))?;
        let dir = base.join("s").join(key);
        private_dir(&dir)?;
        private_dir(&dir.join("links"))?;
        Ok(dir)
    }

    // A short filesystem lock serializes symlink selection with simultaneous
    // attach/exit in another pane. Selection may make bounded API probes, but no
    // lock spans a shell lifetime.
    struct LinkLock(fs::File);
    impl LinkLock {
        fn acquire(dir: &Path) -> io::Result<Self> {
            let file = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(dir.join("links.lock"))?;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(file))
        }
    }
    impl Drop for LinkLock {
        fn drop(&mut self) {
            unsafe {
                libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
    fn alive(pid: u32) -> bool {
        pid > 0
            && (unsafe { libc::kill(pid as i32, 0) } == 0
                || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
    }

    fn remove_file(path: &Path) -> io::Result<()> {
        match fs::remove_file(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }
    fn point_at(dir: &Path, socket: &Path) -> io::Result<()> {
        let temp = dir.join(format!(".herdr-{}.sock", connector::new_link()?));
        symlink(socket, &temp)?;
        let result = fs::rename(&temp, dir.join("herdr.sock"));
        let _ = remove_file(&temp);
        result
    }
    fn records(dir: &Path) -> io::Result<Vec<(PathBuf, LinkRecord)>> {
        let mut records = Vec::new();
        for entry in fs::read_dir(dir.join("links"))? {
            let entry = entry?;
            if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if let Ok(record) = fs::read(entry.path()).and_then(|bytes| {
                serde_json::from_slice::<LinkRecord>(&bytes).map_err(io::Error::other)
            }) {
                records.push((entry.path(), record));
            }
        }
        Ok(records)
    }
    fn repoint(dir: &Path) -> io::Result<()> {
        let mut links = records(dir)?;
        links.sort_by(|a, b| {
            b.1.started_at
                .cmp(&a.1.started_at)
                .then_with(|| b.0.cmp(&a.0))
        });
        for (_, record) in links {
            // A sleeping/disconnected sshd may still accept connect(). Require
            // an answer from the local API before routing persistent clients.
            if alive(record.shell_pid)
                && connector::rpc(
                    Path::new(&record.socket),
                    SshPanesRequest::List(Default::default()),
                )
                .is_ok()
            {
                return point_at(dir, Path::new(&record.socket));
            }
        }
        remove_file(&dir.join("herdr.sock"))
    }

    fn sweep_records(dir: &Path) -> io::Result<()> {
        for (path, record) in records(dir)? {
            if !alive(record.shell_pid) {
                // A single ECONNREFUSED is also a full backlog/bind-listen race.
                // Only our record's dead shell authorizes removing its socket;
                // unrecorded sockets in /tmp belong to someone else or are new.
                let socket = Path::new(&record.socket);
                if socket.parent() == Some(Path::new("/tmp"))
                    && socket
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("herdr-sp-") && n.ends_with(".sock"))
                    && fs::symlink_metadata(socket).is_ok_and(|m| {
                        m.uid() == unsafe { libc::geteuid() } && m.file_type().is_socket()
                    })
                {
                    remove_file(socket)?;
                }
                remove_file(&path)?;
            }
        }
        Ok(())
    }

    /// Build the entire realm explicitly. No inherited session/client/tmux handle
    /// may route a remote tool to the Studio's own server or another tmux pane.
    pub(crate) fn remote_env(
        spec: &RemoteSpec,
        root: &Path,
        remote_home: &Path,
    ) -> BTreeMap<String, String> {
        let state = state_root(root, remote_home);
        let mut env = BTreeMap::from([
            ("HERDR_ENV".into(), "1".into()),
            (
                "HERDR_SOCKET_PATH".into(),
                state
                    .join("s")
                    .join(&spec.server_key)
                    .join("herdr.sock")
                    .display()
                    .to_string(),
            ),
            ("HERDR_PANE_ID".into(), spec.pane_id.clone()),
            ("HERDR_WORKSPACE_ID".into(), spec.workspace_id.clone()),
            ("HERDR_TAB_ID".into(), spec.tab_id.clone()),
            (
                "HERDR_BIN_PATH".into(),
                root.join("bin/herdr").display().to_string(),
            ),
            ("HERDR_SSH_PANES".into(), "1".into()),
            ("HERDR_SSH_PANES_HOST".into(), spec.host.clone()),
            ("HERDR_SSH_PANES_LINK".into(), spec.link.clone()),
            ("HERDR_SSH_PANES_SERVER_ID".into(), spec.server_key.clone()),
            ("HERDR_SSH_PANES_HOME".into(), root.display().to_string()),
        ]);
        for (name, path) in [
            ("HERDR_SCRATCH_NVIM_DIR", "nvim-plugins/herdr-scratch.nvim"),
            ("HERDR_SIDEKICK_DIR", "nvim-plugins/sidekick.nvim"),
        ] {
            if root.join(path).is_dir() {
                env.insert(name.into(), root.join(path).display().to_string());
            }
        }
        for (name, value) in &spec.env {
            if [
                "__HERDR_SP_ARGV",
                "HERDR_SESSION",
                "HERDR_CLIENT_SOCKET_PATH",
                "TMUX",
                "TMUX_PANE",
                "HERDR_SCRATCH_NVIM_DIR",
                "HERDR_SIDEKICK_DIR",
                "HERDR_ENV",
                "HERDR_SSH_PANES",
                "HERDR_BIN_PATH",
                "HERDR_SOCKET_PATH",
                "HERDR_SSH_PANES_HOME",
                "HERDR_SSH_PANES_HOST",
                "HERDR_SSH_PANES_LINK",
                "HERDR_SSH_PANES_SERVER_ID",
            ]
            .contains(&name.as_str())
                || (name == "NVIM_APPNAME" && value.is_empty())
            {
                continue;
            }
            env.insert(
                name.clone(),
                value
                    .replace("@root", &root.display().to_string())
                    .replace("@state", &state.join("plugins").display().to_string()),
            );
        }
        env
    }
    pub(crate) fn apply_env(
        command: &mut Command,
        spec: &RemoteSpec,
        root: &Path,
    ) -> io::Result<()> {
        for key in [
            "HERDR_SESSION",
            "HERDR_CLIENT_SOCKET_PATH",
            "TMUX",
            "TMUX_PANE",
            "HERDR_SCRATCH_NVIM_DIR",
            "HERDR_SIDEKICK_DIR",
            "NVIM_APPNAME",
        ] {
            command.env_remove(key);
        }
        command.envs(remote_env(spec, root, &home()?));
        command.env(
            "PATH",
            format!(
                "{}/bin:{}",
                root.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );
        let cwd = spec
            .cwd
            .as_deref()
            .map(expand_home)
            .transpose()?
            .unwrap_or(home()?);
        command.current_dir(if cwd.is_dir() { cwd } else { home()? });
        Ok(())
    }

    fn emit_status(link: &str, event: &str) -> io::Result<()> {
        use std::io::Write;
        let mut output = std::io::stdout().lock();
        write!(output, "\x1b]777;herdr-sp-{link};{event}\x07")?;
        output.flush()
    }

    pub fn remote_attach(hex: &str) -> io::Result<i32> {
        let spec = RemoteSpec::decode(hex)?;
        validate(&spec, true)?;
        let dir = server_dir(&spec.server_key)?;
        let root = root()?;
        // The last-published manifest is the commit marker. A partial upload must
        // never start an interactive shell with a mixed payload.
        let digest = fs::read(root.join("manifest.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|v| v["digest"].as_str().map(str::to_string));
        if digest.as_deref() != Some(&spec.payload_digest) {
            emit_status(&spec.link, "stale")?;
            return Ok(3);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while UnixStream::connect(&spec.socket).is_err() {
            if Instant::now() >= deadline {
                eprintln!("ssh-panes: remote socket {} did not appear; is AllowStreamLocalForwarding disabled on {}?", spec.socket, spec.host);
                return Ok(1);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        {
            let _lock = LinkLock::acquire(&dir)?;
            sweep_records(&dir)?;
            let record = LinkRecord {
                socket: spec.socket.clone(),
                shell_pid: std::process::id(),
                started_at: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
            };
            fs::write(
                dir.join("links").join(format!("{}.json", spec.link)),
                serde_json::to_vec(&record).map_err(io::Error::other)?,
            )?;
            point_at(&dir, Path::new(&spec.socket))?;
        }
        // Accept a shell-start observation before exec. Even an immediate
        // typo+exit must be distinguishable from pre-exec payload exit 3/127;
        // the detached watcher may not get scheduled before that shell exits.
        let _ = connector::rpc(
            Path::new(&spec.socket),
            SshPanesRequest::Report(ReportParams {
                link_id: spec.link.clone(),
                seq: 1,
                shell_pid: std::process::id(),
                shell_name: std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()),
                foreground: RemoteForeground {
                    process_group_id: None,
                    foreground_is_pane_shell: true,
                    agent: None,
                    process_name: None,
                },
                cwd: spec.cwd.clone(),
                foreground_cwd: None,
            }),
        );
        let mut watch = Command::new(root.join("bin/herdr"));
        watch
            .args([
                "ssh",
                "remote-watch",
                "--link",
                &spec.link,
                "--shell-pid",
                &std::process::id().to_string(),
                "--server-key",
                &spec.server_key,
                "--socket",
                &spec.socket,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // setsid keeps the cleanup observer alive after sshd hangs up the shell.
        unsafe {
            watch.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        if let Err(error) = watch.spawn() {
            cleanup(&dir, &spec.link, &spec.socket)?;
            return Err(error);
        }
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut command = Command::new(&shell);
        command.arg("-l");
        if Path::new(&shell).file_name().is_some_and(|s| s == "fish") {
            command.args([
                "-C",
                &format!(
                    "fish_add_path -gpm {}",
                    connector::quote(&root.join("bin").display().to_string())
                ),
            ]);
        }
        apply_env(&mut command, &spec, &root)?;
        emit_status(&spec.link, "ready")?;
        let error = command.exec();
        cleanup(&dir, &spec.link, &spec.socket)?;
        Err(error)
    }

    fn cleanup(dir: &Path, link: &str, socket: &str) -> io::Result<()> {
        let _lock = LinkLock::acquire(dir)?;
        remove_file(&dir.join("links").join(format!("{link}.json")))?;
        remove_file(Path::new(socket))?;
        repoint(dir)
    }
    pub fn remote_watch(args: WatchArgs) -> io::Result<i32> {
        let spec = RemoteSpec {
            v: 1,
            link: args.link.clone(),
            socket: args.socket.clone(),
            server_key: args.server_key.clone(),
            host: String::new(),
            pane_id: String::new(),
            workspace_id: String::new(),
            tab_id: String::new(),
            cwd: None,
            payload_digest: String::new(),
            env: BTreeMap::new(),
        };
        validate(&spec, true)?;
        let dir = server_dir(&args.server_key)?;
        let shell_name = std::env::var("SHELL")
            .ok()
            .and_then(|s| {
                Path::new(&s)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "sh".into());
        let mut previous = None;
        let mut sent = Instant::now() - Duration::from_secs(5);
        let mut seq = 1;
        while alive(args.shell_pid) {
            let foreground = crate::pane::ssh_probe::remote_side_probe(args.shell_pid);
            let (cwd, foreground_cwd) = crate::pane::ssh_probe::remote_side_cwds(
                args.shell_pid,
                foreground.process_group_id,
            );
            let observation = (foreground.clone(), cwd.clone(), foreground_cwd.clone());
            if previous.as_ref() != Some(&observation) || sent.elapsed() >= Duration::from_secs(5) {
                seq += 1;
                let report = ReportParams {
                    link_id: args.link.clone(),
                    seq,
                    shell_pid: args.shell_pid,
                    shell_name: shell_name.clone(),
                    foreground,
                    cwd,
                    foreground_cwd,
                };
                let accepted =
                    connector::rpc(Path::new(&args.socket), SshPanesRequest::Report(report))
                        .is_ok_and(|r| r["accepted"] == true);
                if accepted && fs::symlink_metadata(dir.join("herdr.sock")).is_err() {
                    // A handoff/stall can make every repoint probe fail. Once
                    // reports work again, repair routing for persistent clients.
                    let _lock = LinkLock::acquire(&dir)?;
                    repoint(&dir)?;
                }
                previous = Some(observation);
                sent = Instant::now();
                if !accepted {
                    previous = None;
                    // Retry within four seconds (including handoff/UnknownLink),
                    // and still observe shell death every 500 ms while backed off.
                    for _ in 0..8 {
                        if !alive(args.shell_pid) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(500));
                    }
                    continue;
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        cleanup(&dir, &args.link, &args.socket)?;
        Ok(0)
    }
    pub fn remote_run(hex: &str, argv: &[String]) -> io::Result<i32> {
        let mut spec = RemoteSpec::decode(hex)?;
        validate(&spec, false)?;
        let encoded: Vec<String> = spec
            .env
            .remove("__HERDR_SP_ARGV")
            .map(|s| serde_json::from_str(&s).map_err(io::Error::other))
            .transpose()?
            .unwrap_or_default();
        let argv = if argv.is_empty() { &encoded } else { argv };
        let Some(program) = argv.first() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote command is empty",
            ));
        };
        let root = root()?;
        let candidate = root.join("bin").join(program);
        let mut command = Command::new(if !program.contains('/') && candidate.is_file() {
            candidate
        } else {
            PathBuf::from(program)
        });
        command.args(&argv[1..]);
        apply_env(&mut command, &spec, &root)?;
        Err(command.exec())
    }
    #[cfg(test)]
    mod lifecycle_tests {
        use super::*;
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        fn api_peer(path: &Path) -> std::thread::JoinHandle<()> {
            let listener = UnixListener::bind(path).unwrap();
            std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["method"], "ssh_panes");
                writeln!(stream, "{}", serde_json::json!({"id":request["id"],"result":{"type":"ssh_panes","ssh_panes":{"enabled":true,"links":[]}}})).unwrap();
            })
        }

        #[test]
        fn ssh_panes_repoint_prefers_newest_api_peer_and_preserves_refused_live_socket() {
            let dir = std::env::temp_dir()
                .join(format!("herdr-sp-test-{}", connector::new_link().unwrap()));
            private_dir(&dir.join("links")).unwrap();
            let old = dir.join("old.sock");
            let new = dir.join("new.sock");
            let refused = PathBuf::from(format!(
                "/tmp/herdr-sp-{}.sock",
                connector::new_link().unwrap()
            ));
            drop(UnixListener::bind(&refused).unwrap());
            let old_listener = UnixListener::bind(&old).unwrap();
            let peer = api_peer(&new);
            for (id, socket, started_at) in [
                ("old", &old, 10),
                ("new", &new, 20),
                ("refused", &refused, 30),
            ] {
                let record = LinkRecord {
                    socket: socket.display().to_string(),
                    shell_pid: std::process::id(),
                    started_at,
                };
                fs::write(
                    dir.join("links").join(format!("{id}.json")),
                    serde_json::to_vec(&record).unwrap(),
                )
                .unwrap();
            }
            sweep_records(&dir).unwrap();
            assert!(refused.exists(), "a live record must survive ECONNREFUSED");
            repoint(&dir).unwrap();
            peer.join().unwrap();
            assert_eq!(fs::read_link(dir.join("herdr.sock")).unwrap(), new);
            // Make only the refused record dead; dead-shell authority permits cleanup.
            let record = LinkRecord {
                socket: refused.display().to_string(),
                shell_pid: 0,
                started_at: 30,
            };
            fs::write(
                dir.join("links/refused.json"),
                serde_json::to_vec(&record).unwrap(),
            )
            .unwrap();
            sweep_records(&dir).unwrap();
            assert!(!refused.exists());
            drop(old_listener);
            fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn ssh_panes_bare_accept_without_api_reply_is_not_a_live_forward() {
            let dir = std::env::temp_dir()
                .join(format!("herdr-sp-test-{}", connector::new_link().unwrap()));
            private_dir(&dir.join("links")).unwrap();
            let socket = dir.join("zombie.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let peer = std::thread::spawn(move || {
                let _ = listener.accept().unwrap();
            });
            let record = LinkRecord {
                socket: socket.display().to_string(),
                shell_pid: std::process::id(),
                started_at: 1,
            };
            fs::write(
                dir.join("links/zombie.json"),
                serde_json::to_vec(&record).unwrap(),
            )
            .unwrap();
            point_at(&dir, &socket).unwrap();
            repoint(&dir).unwrap();
            peer.join().unwrap();
            assert!(fs::symlink_metadata(dir.join("herdr.sock")).is_err());
            assert!(socket.exists());
            fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn ssh_panes_exiting_link_repoints_only_its_server_socket() {
            let dir = std::env::temp_dir()
                .join(format!("herdr-sp-test-{}", connector::new_link().unwrap()));
            private_dir(&dir).unwrap();
            private_dir(&dir.join("links")).unwrap();
            let first = dir.join("a.sock");
            let second = dir.join("b.sock");
            let _a = UnixListener::bind(&first).unwrap();
            let peer = api_peer(&second);
            for (id, socket) in [("a", &first), ("b", &second)] {
                let record = LinkRecord {
                    socket: socket.display().to_string(),
                    shell_pid: std::process::id(),
                    started_at: 1,
                };
                fs::write(
                    dir.join("links").join(format!("{id}.json")),
                    serde_json::to_vec(&record).unwrap(),
                )
                .unwrap();
            }
            point_at(&dir, &first).unwrap();
            cleanup(&dir, "a", first.to_str().unwrap()).unwrap();
            assert_eq!(fs::read_link(dir.join("herdr.sock")).unwrap(), second);
            peer.join().unwrap();
            assert_eq!(
                fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            cleanup(&dir, "b", second.to_str().unwrap()).unwrap();
            assert!(fs::symlink_metadata(dir.join("herdr.sock")).is_err());
            fs::remove_dir_all(dir).unwrap();
        }
    }
}

#[cfg(all(test, unix))]
mod env_tests {
    use super::*;
    use std::fs;

    #[test]
    fn ssh_panes_development_state_is_disjoint_and_state_expands_to_plugins() {
        let spec = RemoteSpec {
            v: 1,
            link: "l".into(),
            socket: "s".into(),
            server_key: "1234567890abcdef".into(),
            host: "studio".into(),
            pane_id: "w1:p1".into(),
            workspace_id: "w1".into(),
            tab_id: "w1:t1".into(),
            cwd: None,
            payload_digest: "d".into(),
            env: BTreeMap::from([
                ("STATE".into(), "@state/shadowfax.scratch".into()),
                ("HERDR_SSH_PANES".into(), "0".into()),
                ("HERDR_SESSION".into(), "default".into()),
            ]),
        };
        let env = remote_env(
            &spec,
            Path::new("/home/u/.local/share/herdr-ssh-panes-dev"),
            Path::new("/home/u"),
        );
        assert_eq!(
            env["HERDR_SOCKET_PATH"],
            "/home/u/.local/state/herdr-ssh-panes-dev/s/1234567890abcdef/herdr.sock"
        );
        assert_eq!(
            env["STATE"],
            "/home/u/.local/state/herdr-ssh-panes-dev/plugins/shadowfax.scratch"
        );
        assert_eq!(env["HERDR_SSH_PANES"], "1");
        assert!(!env.contains_key("HERDR_SESSION"));
    }

    #[test]
    fn ssh_panes_remote_env_expands_state_and_exports_only_present_plugins() {
        let root = std::env::temp_dir().join(format!(
            "herdr-sp-test-{}",
            crate::ssh_panes::connector::new_link().unwrap()
        ));
        fs::create_dir_all(root.join("nvim-plugins/sidekick.nvim")).unwrap();
        let spec = RemoteSpec {
            v: 1,
            link: "1234567890abcdef12345678".into(),
            socket: "/tmp/link.sock".into(),
            server_key: "1234567890abcdef".into(),
            host: "studio".into(),
            pane_id: "w1:p2".into(),
            workspace_id: "w1".into(),
            tab_id: "w1:t1".into(),
            cwd: None,
            payload_digest: "hash".into(),
            env: BTreeMap::from([
                ("NVIM_APPNAME".into(), "herdr-sp-nvim".into()),
                ("CUSTOM".into(), "@root/a:@state/b".into()),
                ("__HERDR_SP_ARGV".into(), "[]".into()),
            ]),
        };
        let env = remote_env(&spec, &root, Path::new("/home/remote"));
        assert_eq!(
            env["HERDR_SOCKET_PATH"],
            format!(
                "/home/remote/.local/state/{}/s/1234567890abcdef/herdr.sock",
                root.file_name().unwrap().to_string_lossy()
            )
        );
        assert_eq!(env["HERDR_PANE_ID"], "w1:p2");
        assert_eq!(env["HERDR_ENV"], "1");
        assert_eq!(env["HERDR_SSH_PANES_HOST"], "studio");
        assert_eq!(env["NVIM_APPNAME"], "herdr-sp-nvim");
        assert_eq!(
            env["CUSTOM"],
            format!(
                "{}/a:/home/remote/.local/state/{}/plugins/b",
                root.display(),
                root.file_name().unwrap().to_string_lossy()
            )
        );
        assert!(env.contains_key("HERDR_SIDEKICK_DIR"));
        assert!(!env.contains_key("HERDR_SCRATCH_NVIM_DIR"));
        assert!(!env.contains_key("__HERDR_SP_ARGV"));
        fs::remove_dir_all(root).unwrap();
    }
}
