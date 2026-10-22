//! Digest-gated publication of the current executable and configured payload paths.
//! Rsync replaces files by rename; manifest.json is published last as the commit
//! marker, so an interrupted transfer cannot authorize a mixed remote payload.
//! Process-boundary regression harness: `python3 tests/ssh_panes_sync.py target/debug/herdr`.

use super::config::SshPanesConfig;
use serde::{Deserialize, Serialize};
use std::io;

/// Published payload identity, including permissions and followed symlink targets.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub version: u32,
    pub digest: String,
    pub files: Vec<ManifestFile>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ManifestFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub kind: String,
    pub mode: u32,
}
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct SyncReport {
    pub changed: bool,
    pub dry_run: bool,
    pub digest: String,
    pub transferred: Vec<String>,
}
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct DoctorReport {
    pub ok: bool,
    pub diagnostics: Vec<String>,
}

#[cfg(unix)]
pub(crate) use unix::*;
#[cfg(not(unix))]
pub fn local_manifest(_cfg: &SshPanesConfig) -> io::Result<Manifest> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "SSH panes require Unix",
    ))
}

#[cfg(unix)]
mod unix {
    use super::*;
    use crate::ssh_panes::{config::SyncPath, connector, remote};
    use sha2::{Digest, Sha256};
    use std::{
        collections::BTreeMap,
        fs,
        io::{Read, Write},
        os::{
            fd::AsRawFd,
            unix::fs::{MetadataExt, PermissionsExt},
        },
        path::{Component, Path, PathBuf},
        process::{Command, Stdio},
    };

    #[derive(Clone)]
    struct Mapping {
        src: PathBuf,
        dest: String,
        exclude: Vec<String>,
    }
    fn mappings(cfg: &SshPanesConfig, executable: &Path) -> io::Result<Vec<Mapping>> {
        validate_remote_root(&cfg.remote_root)?;
        let mut result = Vec::new();
        for SyncPath { src, dest, exclude } in &cfg.sync.paths {
            validate_dest(dest)?;
            result.push(Mapping {
                src: {
                    let source = remote::expand_home(src)?;
                    let source = if source.is_absolute() {
                        source
                    } else {
                        std::env::current_dir()?.join(source)
                    };
                    // A trailing-slash directory transfer follows a source-root
                    // symlink. Nested links follow targets as rsync --copy-links does.
                    if source.is_dir() {
                        fs::canonicalize(source)?
                    } else {
                        source
                    }
                },
                dest: dest.trim_end_matches('/').into(),
                exclude: exclude.clone(),
            });
        }
        result.push(Mapping {
            src: executable.into(),
            dest: "bin/herdr".into(),
            exclude: Vec::new(),
        });
        // Overlapping roots would let one mapping's --delete erase another one's
        // files, including the payload binary. Refuse before any remote mutation.
        for (i, a) in result.iter().enumerate() {
            for b in &result[i + 1..] {
                let a = destination(cfg, &a.dest);
                let b = destination(cfg, &b.dest);
                if a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/")) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "overlapping SSH payload destinations",
                    ));
                }
            }
        }
        Ok(result)
    }
    fn validate_dest(dest: &str) -> io::Result<()> {
        let relative = dest.strip_prefix("~/").unwrap_or(dest);
        if relative.is_empty()
            || Path::new(relative).is_absolute()
            || Path::new(relative)
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
            || relative.contains(['\0', '\n', '\r'])
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sync destination must be root-relative or ~/home-relative without ..",
            ));
        }
        Ok(())
    }
    pub(crate) fn validate_remote_root(root: &str) -> io::Result<()> {
        let path = Path::new(root.strip_prefix("~/").unwrap_or(root));
        let valid_prefix = root.starts_with("~/") || path.is_absolute();
        if !valid_prefix
            || root.contains(['\0', '\n', '\r'])
            || !path.components().any(|c| matches!(c, Component::Normal(_)))
            || path
                .components()
                .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote_root must be ~/<non-empty path> or an absolute non-root path without ..",
            ));
        }
        Ok(())
    }

    // Home mappings share a user-owned tree. Without an explicit opt-in field in
    // SyncPath, only a top-level herdr-* directory may be destructively mirrored.
    fn may_delete(dest: &str) -> bool {
        dest.strip_prefix("~/").is_none_or(|rest| {
            rest.split('/')
                .next()
                .is_some_and(|top| top.starts_with("herdr-"))
        })
    }

    fn cache_key(src: &Path, meta: &fs::Metadata) -> String {
        // ctime/inode detect replacements and edits preserving size + mtime.
        format!(
            "{}\0{}:{}:{}:{}:{}:{}:{}",
            src.display(),
            meta.dev(),
            meta.ino(),
            meta.len(),
            meta.mtime(),
            meta.mtime_nsec(),
            meta.ctime(),
            meta.ctime_nsec()
        )
    }

    fn hex_hash(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }
    type HashCache = BTreeMap<String, String>;

    // Rsync's common exclusion grammar: basename patterns apply to each path
    // component; slash/anchored patterns apply to the mapping-relative path.
    fn excluded(relative: &str, is_dir: bool, patterns: &[String]) -> io::Result<bool> {
        for pattern in patterns {
            if pattern.ends_with('/') && !is_dir {
                continue;
            }
            let pattern = pattern.trim_end_matches('/');
            let anchored = pattern.starts_with('/');
            let pattern = pattern.trim_start_matches('/');
            let mut re = String::from("^");
            let mut chars = pattern.chars().peekable();
            while let Some(c) = chars.next() {
                match c {
                    '*' if chars.peek() == Some(&'*') => {
                        chars.next();
                        re.push_str(".*");
                    }
                    '*' => re.push_str("[^/]*"),
                    '?' => re.push_str("[^/]"),
                    _ => re.push_str(&regex::escape(&c.to_string())),
                }
            }
            re.push('$');
            let re = regex::Regex::new(&re).map_err(io::Error::other)?;
            if if anchored || pattern.contains('/') {
                re.is_match(relative)
            } else {
                relative.split('/').any(|part| re.is_match(part))
            } {
                return Ok(true);
            }
        }
        Ok(false)
    }
    /// A scan borrows the last cache while collecting only currently visited keys.
    struct HashScan<'a> {
        previous: HashCache,
        current: &'a mut HashCache,
    }
    fn walk(
        src: &Path,
        dest: &str,
        relative: &str,
        patterns: &[String],
        cache: &mut HashScan<'_>,
        ancestors: &mut Vec<(u64, u64)>,
        files: &mut Vec<ManifestFile>,
    ) -> io::Result<()> {
        // Hash the dereferenced tree that rsync sends. A cyclic directory link
        // is an error instead of recursion until stack exhaustion.
        // Basename exclusions must work even for dangling links, like rsync.
        let entry = fs::symlink_metadata(src)?;
        if !relative.is_empty() && excluded(relative, entry.is_dir(), patterns)? {
            return Ok(());
        }
        let meta = fs::metadata(src)?;
        if !relative.is_empty() && excluded(relative, meta.is_dir(), patterns)? {
            return Ok(());
        }
        let kind = if meta.is_dir() {
            "dir"
        } else if meta.is_file() {
            "file"
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported payload file type",
            ));
        };
        let path = if relative.is_empty() {
            dest.into()
        } else {
            format!("{dest}/{relative}")
        };
        let sha256 = match kind {
            "dir" => String::new(),
            _ => {
                let key = cache_key(src, &meta);
                if let Some(hash) = cache.previous.get(&key) {
                    cache.current.insert(key, hash.clone());
                    hash.clone()
                } else {
                    let mut input = fs::File::open(src)?;
                    let mut hash = Sha256::new();
                    let mut buffer = [0u8; 65536];
                    loop {
                        let count = input.read(&mut buffer)?;
                        if count == 0 {
                            break;
                        }
                        hash.update(&buffer[..count]);
                    }
                    let after = input.metadata()?;
                    if cache_key(src, &after) != key || cache_key(src, &fs::metadata(src)?) != key {
                        return Err(io::Error::other(format!(
                            "payload changed while hashing: {}",
                            src.display()
                        )));
                    }
                    let hash = format!("{:x}", hash.finalize());
                    cache.current.insert(key, hash.clone());
                    hash
                }
            }
        };
        files.push(ManifestFile {
            path,
            size: if kind == "dir" { 0 } else { meta.len() },
            sha256,
            kind: kind.into(),
            mode: meta.permissions().mode() & 0o7777,
        });
        if meta.is_dir() {
            let identity = (meta.dev(), meta.ino());
            if ancestors.contains(&identity) {
                return Err(io::Error::other(format!(
                    "cyclic payload symlink: {}",
                    src.display()
                )));
            }
            ancestors.push(identity);
            let mut children = fs::read_dir(src)?.collect::<io::Result<Vec<_>>>()?;
            children.sort_by_key(|e| e.file_name());
            for child in children {
                let name = child.file_name().into_string().map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "payload filenames must be UTF-8",
                    )
                })?;
                let relative = if relative.is_empty() {
                    name
                } else {
                    format!("{relative}/{name}")
                };
                walk(
                    &child.path(),
                    dest,
                    &relative,
                    patterns,
                    cache,
                    ancestors,
                    files,
                )?;
            }
            ancestors.pop();
        }
        Ok(())
    }
    fn manifest_for(mappings: &[Mapping], cache: &mut HashCache) -> io::Result<Manifest> {
        // Publish only keys visited in this scan. Removed files and old stat
        // signatures must not accumulate on every connect.
        let mut cache = HashScan {
            previous: std::mem::take(cache),
            current: cache,
        };
        let mut files = Vec::new();
        for mapping in mappings {
            walk(
                &mapping.src,
                &mapping.dest,
                "",
                &mapping.exclude,
                &mut cache,
                &mut Vec::new(),
                &mut files,
            )?;
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        #[derive(Serialize)]
        struct Canonical<'a> {
            version: u32,
            files: &'a [ManifestFile],
        }
        let digest = hex_hash(
            &serde_json::to_vec(&Canonical {
                version: 1,
                files: &files,
            })
            .map_err(io::Error::other)?,
        );
        Ok(Manifest {
            version: 1,
            digest,
            files,
        })
    }
    fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
        let temp = path.with_extension(format!("{}.tmp", connector::new_link()?));
        fs::write(&temp, bytes)?;
        let result = fs::rename(&temp, path);
        let _ = fs::remove_file(&temp);
        result
    }
    pub fn local_manifest(cfg: &SshPanesConfig) -> io::Result<Manifest> {
        let mappings = mappings(cfg, &crate::platform::launch_executable()?)?;
        let dir = crate::config::config_dir().join("ssh-panes");
        remote::private_dir(&dir)?;
        let cache_path = dir.join("hash-cache.json");
        let mut cache = fs::read(&cache_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<HashCache>(&bytes).ok())
            .unwrap_or_default();
        let manifest = manifest_for(&mappings, &mut cache)?;
        atomic_write(
            &cache_path,
            &serde_json::to_vec(&cache).map_err(io::Error::other)?,
        )?;
        Ok(manifest)
    }
    fn error_tail(output: &std::process::Output) -> io::Error {
        let error = String::from_utf8_lossy(&output.stderr);
        let tail = error
            .lines()
            .rev()
            .take(8)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        io::Error::other(if tail.is_empty() {
            format!("command exited {}", output.status)
        } else {
            tail
        })
    }
    /// Post steps use bash stdin, never fish's parsing rules. All data inserted
    /// here uses POSIX quoting, while SSH's outer command remains just bash -s.
    fn bash(host: &str, cfg: &SshPanesConfig, script: &str) -> io::Result<std::process::Output> {
        let mut child = connector::side_command(host, cfg)?
            .arg("bash -s")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("missing script stdin"))?
            .write_all(script.as_bytes())?;
        let output = child.wait_with_output()?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(error_tail(&output))
        }
    }
    fn bash_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
    fn bash_path(value: &str) -> String {
        if let Some(rest) = value.strip_prefix("~/") {
            format!("\"$HOME\"/{}", bash_quote(rest))
        } else {
            bash_quote(value)
        }
    }
    fn destination(cfg: &SshPanesConfig, dest: &str) -> String {
        if dest.starts_with("~/") {
            dest.into()
        } else {
            format!("{}/{dest}", cfg.remote_root.trim_end_matches('/'))
        }
    }
    fn read_manifest(host: &str, cfg: &SshPanesConfig) -> io::Result<(String, Option<Manifest>)> {
        let output = bash(
            host,
            cfg,
            &format!(
                "set -e\nprintf '%s\\0' \"$HOME\"\np={}/manifest.json\nif test -f \"$p\"; then cat \"$p\"; fi\n",
                bash_path(&cfg.remote_root)
            ),
        )?;
        let separator = output
            .stdout
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| io::Error::other("remote HOME response is missing"))?;
        let home =
            String::from_utf8(output.stdout[..separator].to_vec()).map_err(io::Error::other)?;
        // A malformed marker is stale, rather than a reason to skip publication.
        Ok((
            home,
            serde_json::from_slice::<Manifest>(&output.stdout[separator + 1..])
                .ok()
                .filter(|m| m.version == 1),
        ))
    }
    /// All local sessions sharing a config may publish the same remote root.
    /// Hold this lock across every mapping and marker publication so simultaneous
    /// auto-sync retries cannot interleave payload versions.
    struct SyncLock(fs::File);
    impl SyncLock {
        fn acquire(host: &str, cfg: &SshPanesConfig) -> io::Result<Self> {
            let dir = crate::config::config_dir().join("ssh-panes");
            remote::private_dir(&dir)?;
            let key = hex_hash(format!("{host}\0{}", cfg.remote_root).as_bytes());
            let file = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(dir.join(format!("sync-{}.lock", &key[..16])))?;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(file))
        }
    }
    impl Drop for SyncLock {
        fn drop(&mut self) {
            unsafe {
                libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }

    pub fn ensure_synced(host: &str, cfg: &SshPanesConfig, force: bool) -> io::Result<SyncReport> {
        sync_payload(host, cfg, force, false)
    }
    pub fn sync_payload(
        host: &str,
        cfg: &SshPanesConfig,
        force: bool,
        dry_run: bool,
    ) -> io::Result<SyncReport> {
        connector::check_host(host, cfg)?;
        let _lock = SyncLock::acquire(host, cfg)?;
        let manifest = local_manifest(cfg)?;
        let (remote_home, previous) = read_manifest(host, cfg)?;
        let changed = force
            || previous
                .as_ref()
                .is_none_or(|m| m.digest != manifest.digest);
        let mut report = SyncReport {
            changed,
            dry_run,
            digest: manifest.digest.clone(),
            transferred: Vec::new(),
        };
        if !changed || dry_run {
            return Ok(report);
        }
        let mappings = mappings(cfg, &crate::platform::launch_executable()?)?;
        // A home-relative mapping can alias an absolute remote_root too. Resolve
        // both with the remote HOME before allowing --delete on either tree.
        let resolved = mappings
            .iter()
            .map(|m| {
                let dest = destination(cfg, &m.dest);
                if let Some(rest) = dest.strip_prefix("~/") {
                    format!("{remote_home}/{rest}")
                } else if Path::new(&dest).is_absolute() {
                    dest
                } else {
                    format!("{remote_home}/{dest}")
                }
            })
            .collect::<Vec<_>>();
        for (i, a) in resolved.iter().enumerate() {
            for b in &resolved[i + 1..] {
                if a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/")) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "overlapping SSH payload destinations",
                    ));
                }
            }
        }
        let mut prepare = format!(
            "set -e\nmkdir -p {root}\nrm -f {root}/manifest.json\n",
            root = bash_path(&cfg.remote_root)
        );
        for m in &mappings {
            let dest = destination(cfg, &m.dest);
            // Directory mappings copy contents; file mappings name the file.
            let dir = if m.src.is_dir() {
                dest
            } else {
                dest.rsplit_once('/')
                    .map(|(p, _)| p.to_string())
                    .ok_or_else(|| io::Error::other("invalid payload destination"))?
            };
            prepare.push_str(&format!("mkdir -p {}\n", bash_path(&dir)));
        }
        bash(host, cfg, &prepare)?;
        let transport = std::iter::once("ssh".to_string())
            .chain(connector::side_channel_opts(cfg)?)
            .map(|s| format!("'{}'", s.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(" ");
        for mapping in mappings {
            // Do not even invoke rsync for a mapping whose manifest is identical.
            let belongs = |f: &&ManifestFile| {
                f.path == mapping.dest || f.path.starts_with(&format!("{}/", mapping.dest))
            };
            if !force
                && previous.as_ref().is_some_and(|old| {
                    old.files
                        .iter()
                        .filter(belongs)
                        .eq(manifest.files.iter().filter(belongs))
                })
            {
                continue;
            }
            let mut rsync = Command::new("rsync");
            rsync.current_dir(crate::config::config_dir());
            // We supply shell quoting ourselves for fish and old macOS rsync.
            // Disable modern rsync's second escaping pass; these env controls
            // are also understood/ignored safely by pre-3.0 implementations.
            rsync
                .env("RSYNC_OLD_ARGS", "1")
                .env("RSYNC_PROTECT_ARGS", "0");
            connector::reset_child_signals(&mut rsync);
            if may_delete(&mapping.dest) {
                rsync.arg("--delete");
            } else {
                eprintln!("ssh-panes: copying {} without --delete; home destination must start with herdr- to mirror deletions", mapping.dest);
            }
            rsync.args([
                "-a",
                "--copy-links",
                "--checksum",
                "--itemize-changes",
                "-e",
                &transport,
            ]);
            for exclude in mapping.exclude {
                rsync.arg("--exclude").arg(exclude);
            }
            let directory = mapping.src.is_dir();
            let source = format!(
                "{}{}",
                mapping.src.display(),
                if directory { "/" } else { "" }
            );
            let dest = destination(cfg, &mapping.dest);
            // SSH invokes the remote user's fish. Quote the remote argument, not
            // the host; rsync's transport string is independently POSIX-quoted.
            let target = format!(
                "{host}:{}{}",
                connector::remote_path(&dest)?,
                if directory { "/" } else { "" }
            );
            let output = rsync.arg("--").arg(source).arg(target).output()?;
            if !output.status.success() {
                return Err(error_tail(&output));
            }
            report.transferred.extend(
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter(|line| !line.is_empty())
                    .map(str::to_string),
            );
        }
        // The payload binary uses separate integration config/state if enabled.
        // Default integrations=[] never touches the remote user's agent settings.
        let mut post = format!("set -e\nroot={}\n", bash_path(&cfg.remote_root));
        for integration in &cfg.sync.integrations {
            post.push_str(&format!(
                "\"$root/bin/herdr\" integration install {}\n",
                bash_quote(integration)
            ));
        }
        // Rehash after transfer so a changing local checkout cannot be blessed by
        // a manifest describing bytes different from those rsync actually saw.
        if local_manifest(cfg)?.digest != manifest.digest {
            return Err(io::Error::other("payload changed during sync; retry sync"));
        }
        let json = serde_json::to_string(&manifest).map_err(io::Error::other)?;
        post.push_str(&format!("tmp=\"$root/.manifest-{}.tmp\"\ntrap 'rm -f \"$tmp\"' EXIT\nprintf '%s\\n' {} > \"$tmp\"\nmv -f \"$tmp\" \"$root/manifest.json\"\n", connector::new_link()?, bash_quote(&json)));
        bash(host, cfg, &post)?;
        Ok(report)
    }

    pub fn doctor(host: &str, cfg: &SshPanesConfig) -> io::Result<DoctorReport> {
        connector::check_host(host, cfg)?;
        let local = local_manifest(cfg)?;
        let digest_ok = read_manifest(host, cfg)?
            .1
            .as_ref()
            .is_some_and(|m| m.digest == local.digest);
        let mut spec = connector::spec(host, cfg, None, connector::new_link()?, local.digest)?;
        let script = format!(
            r#"failed=0
if test "$(basename "$SHELL")" = fish; then
  resolved=$($SHELL -l -C 'fish_add_path -gpm {}' -c 'command -v herdr')
else
  resolved=$($SHELL -l -c 'command -v herdr')
fi
if test "$resolved" = "$HERDR_BIN_PATH"; then echo "payload PATH: $resolved"; else echo "payload PATH: unexpected $resolved"; failed=1; fi
dir=$(dirname "$HERDR_SOCKET_PATH")
if test "$(uname)" = Darwin; then perms=$(stat -f %Lp "$dir" 2>/dev/null); else perms=$(stat -c %a "$dir" 2>/dev/null); fi
if ! test -e "$dir"; then echo 'socket directory: no link yet (informational)'; elif test "$perms" = 700; then echo 'socket directory: 0700'; else echo "socket directory: missing or unsafe ($perms)"; failed=1; fi
if ! test -L "$HERDR_SOCKET_PATH" && ! test -e "$HERDR_SOCKET_PATH"; then echo 'stable symlink: no link yet (informational)'; elif test -L "$HERDR_SOCKET_PATH" && "$HERDR_BIN_PATH" pane list >/dev/null 2>&1; then echo 'stable symlink: live'; else echo 'stable symlink: unavailable'; failed=1; fi
if HERDR_SOCKET_PATH={} "$HERDR_BIN_PATH" pane list >/dev/null 2>&1; then echo 'stream-local forwarding: live API'; else echo 'stream-local forwarding: unavailable'; failed=1; fi
exit "$failed"
"#,
            connector::quote(&format!("{}/bin", cfg.remote_root)),
            bash_quote(&spec.socket)
        );
        // Let fish expand the actual remote root rather than the local config's
        // tilde. The script itself is carried as hex data, with no ssh quoting.
        let script = script.replace(
            &connector::quote(&format!("{}/bin", cfg.remote_root)),
            "\"$HERDR_SSH_PANES_HOME/bin\"",
        );
        spec.env.insert(
            "__HERDR_SP_ARGV".into(),
            serde_json::to_string(&vec!["/bin/bash", "-c", &script]).map_err(io::Error::other)?,
        );
        let mut command = Command::new("ssh");
        command.args([
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
            "-o",
            "ExitOnForwardFailure=yes",
            "-R",
            &format!("{}:{}", spec.socket, crate::api::socket_path().display()),
        ]);
        command
            .args(&cfg.ssh_options)
            .arg(host)
            .arg(connector::remote_command(cfg, "remote-run", &spec)?);
        connector::reset_child_signals(&mut command);
        let output = command.output()?;
        let _ = bash(host, cfg, &format!("rm -f {}\n", bash_quote(&spec.socket)));
        let mut diagnostics = vec![format!(
            "payload digest: {}",
            if digest_ok { "current" } else { "stale" }
        )];
        diagnostics.extend(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::to_string),
        );
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            diagnostics.push(if stderr.trim().is_empty() {
                format!("remote checks failed ({})", output.status)
            } else {
                format!("remote checks failed: {}", stderr.trim())
            });
        }
        Ok(DoctorReport {
            ok: digest_ok && output.status.success(),
            diagnostics,
        })
    }
    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn ssh_panes_sync_root_and_home_delete_policy() {
            for root in [
                "", "~", "~/", "/", "/../", "relative", "~/x/../y", "/x/../y",
            ] {
                assert!(validate_remote_root(root).is_err(), "{root}");
            }
            for root in [
                "~/herdr-dev",
                "~/.local/share/herdr-ssh-panes",
                "/tmp/herdr-dev/",
            ] {
                assert!(validate_remote_root(root).is_ok(), "{root}");
            }
            assert!(may_delete("nvim-plugins/sidekick.nvim"));
            assert!(may_delete("~/herdr-nvim/config"));
            assert!(!may_delete("~/.config/nvim"));
            assert!(!may_delete("~/.config/herdr-sp-nvim"));
        }

        #[test]
        fn ssh_panes_hash_cache_tracks_preserved_mtime_and_prunes_removed_files() {
            let dir = std::env::temp_dir()
                .join(format!("herdr-sp-test-{}", connector::new_link().unwrap()));
            fs::create_dir_all(&dir).unwrap();
            let file = dir.join("a");
            fs::write(&file, "one").unwrap();
            let mapping = Mapping {
                src: dir.clone(),
                dest: "config".into(),
                exclude: vec![],
            };
            let mut cache = HashCache::new();
            let first = manifest_for(std::slice::from_ref(&mapping), &mut cache).unwrap();
            let mtime = fs::metadata(&file).unwrap().modified().unwrap();
            fs::write(&file, "two").unwrap();
            fs::File::open(&file).unwrap().set_modified(mtime).unwrap();
            let second = manifest_for(std::slice::from_ref(&mapping), &mut cache).unwrap();
            assert_ne!(first.digest, second.digest);
            assert_eq!(cache.len(), 1);
            fs::remove_file(file).unwrap();
            manifest_for(&[mapping], &mut cache).unwrap();
            assert!(cache.is_empty());
            fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn ssh_panes_nested_links_hash_targets_and_directory_cycles_fail() {
            let dir = std::env::temp_dir()
                .join(format!("herdr-sp-test-{}", connector::new_link().unwrap()));
            fs::create_dir_all(dir.join("source")).unwrap();
            fs::create_dir_all(dir.join("outside")).unwrap();
            fs::write(dir.join("outside/file"), "bytes").unwrap();
            std::os::unix::fs::symlink("../outside", dir.join("source/linked-dir")).unwrap();
            std::os::unix::fs::symlink("../outside/file", dir.join("source/linked-file")).unwrap();
            let mapping = Mapping {
                src: dir.join("source"),
                dest: "config".into(),
                exclude: vec![],
            };
            let manifest =
                manifest_for(std::slice::from_ref(&mapping), &mut HashCache::new()).unwrap();
            assert!(manifest
                .files
                .iter()
                .any(|f| f.path == "config/linked-dir/file" && f.kind == "file"));
            assert!(manifest
                .files
                .iter()
                .any(|f| f.path == "config/linked-file" && f.sha256 == hex_hash(b"bytes")));
            std::os::unix::fs::symlink(".", dir.join("source/cycle")).unwrap();
            assert!(manifest_for(&[mapping], &mut HashCache::new())
                .unwrap_err()
                .to_string()
                .contains("cyclic payload symlink"));
            fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn ssh_panes_manifest_tracks_bytes_deletes_modes_links_and_exclusions() {
            let dir = std::env::temp_dir()
                .join(format!("herdr-sp-test-{}", connector::new_link().unwrap()));
            fs::create_dir_all(dir.join(".git")).unwrap();
            fs::write(dir.join(".git/ignored"), "not payload").unwrap();
            std::os::unix::fs::symlink("missing", dir.join("ignored-link")).unwrap();
            fs::write(dir.join("a"), "first").unwrap();
            std::os::unix::fs::symlink("a", dir.join("link")).unwrap();
            let mapping = Mapping {
                src: dir.clone(),
                dest: "config".into(),
                exclude: vec![".git".into(), "ignored-link".into()],
            };
            let mut cache = HashCache::new();
            let one = manifest_for(std::slice::from_ref(&mapping), &mut cache).unwrap();
            assert_eq!(
                one.files
                    .iter()
                    .map(|f| f.path.as_str())
                    .collect::<Vec<_>>(),
                ["config", "config/a", "config/link"]
            );
            let two = manifest_for(std::slice::from_ref(&mapping), &mut cache).unwrap();
            assert_eq!(one.digest, two.digest);
            // Changing mtime alone changes the cache key, never the payload digest.
            fs::File::open(dir.join("a"))
                .unwrap()
                .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(2))
                .unwrap();
            assert_eq!(
                manifest_for(std::slice::from_ref(&mapping), &mut cache)
                    .unwrap()
                    .digest,
                one.digest
            );
            fs::write(dir.join("a"), "second").unwrap();
            let changed = manifest_for(std::slice::from_ref(&mapping), &mut cache).unwrap();
            assert_ne!(changed.digest, one.digest);
            fs::remove_file(dir.join("link")).unwrap();
            fs::remove_file(dir.join("a")).unwrap();
            assert_ne!(
                manifest_for(std::slice::from_ref(&mapping), &mut cache)
                    .unwrap()
                    .digest,
                changed.digest
            );
            fs::remove_dir_all(dir).unwrap();
        }
        #[test]
        fn ssh_panes_directory_source_symlink_hashes_the_transferred_tree() {
            let dir = std::env::temp_dir()
                .join(format!("herdr-sp-test-{}", connector::new_link().unwrap()));
            fs::create_dir_all(dir.join("source")).unwrap();
            fs::write(dir.join("source/a"), "one").unwrap();
            std::os::unix::fs::symlink(dir.join("source"), dir.join("alias")).unwrap();
            let cfg = SshPanesConfig {
                sync: crate::ssh_panes::config::SyncConfig {
                    paths: vec![SyncPath {
                        src: dir.join("alias").display().to_string(),
                        dest: "nvim".into(),
                        exclude: vec![],
                    }],
                    ..Default::default()
                },
                ..Default::default()
            };
            let mappings = mappings(&cfg, &dir.join("source/a")).unwrap();
            let mut cache = HashCache::new();
            let first = manifest_for(&mappings, &mut cache).unwrap();
            assert!(first.files.iter().any(|f| f.path == "nvim/a"));
            fs::write(dir.join("source/a"), "second").unwrap();
            assert_ne!(
                first.digest,
                manifest_for(&mappings, &mut cache).unwrap().digest
            );
            fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn ssh_panes_sync_rejects_deletion_overlap_and_destination_escape() {
            for bad in ["", "/etc", "../escape", "~/../escape", "a/../b"] {
                assert!(validate_dest(bad).is_err(), "{bad}");
            }
            let cfg = SshPanesConfig {
                sync: crate::ssh_panes::config::SyncConfig {
                    paths: vec![SyncPath {
                        src: "/src".into(),
                        dest: "bin".into(),
                        exclude: vec![],
                    }],
                    ..Default::default()
                },
                ..Default::default()
            };
            assert!(mappings(&cfg, Path::new("/exe")).is_err());
            let mut alias = cfg.clone();
            alias.sync.paths = vec![
                SyncPath {
                    src: "/src".into(),
                    dest: "nvim-plugins/x".into(),
                    exclude: vec![],
                },
                SyncPath {
                    src: "/src".into(),
                    dest: "~/.local/share/herdr-ssh-panes/nvim-plugins".into(),
                    exclude: vec![],
                },
            ];
            assert!(mappings(&alias, Path::new("/exe")).is_err());
            assert!(excluded("a/cache.tmp", false, &["*.tmp".into()]).unwrap());
            assert!(!excluded("a/cache.txt", false, &["*.tmp".into()]).unwrap());
        }
    }
}
