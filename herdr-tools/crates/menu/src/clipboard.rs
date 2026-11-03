//! Write clipboard text through the platform's native provider. No shell or OSC52
//! is involved, and success means the provider accepted all input and exited zero.
use std::io::{Read, Seek, Write};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

pub fn copy(text: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        write(&mut Command::new("pbcopy"), text).context("could not copy pane context with pbcopy")
    }
    #[cfg(target_os = "linux")]
    {
        // Prefer Wayland only when it is active. Falling through is safe only
        // for a missing executable, not a provider that ran and rejected input.
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        let mut providers = vec![
            ("xclip", vec!["-selection", "clipboard", "-in"]),
            ("xsel", vec!["--clipboard", "--input"]),
        ];
        if wayland {
            providers.insert(0, ("wl-copy", vec![]));
        } else {
            providers.push(("wl-copy", vec![]));
        }
        for (provider, args) in providers {
            match write(Command::new(provider).args(args), text) {
                Ok(()) => return Ok(()),
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
                Err(error) => return Err(error).with_context(|| format!("{provider} failed")),
            }
        }
        bail!("no clipboard provider found; install wl-copy, xclip, or xsel")
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = text;
        bail!("clipboard copying is supported on macOS and Linux")
    }
}

fn write(command: &mut Command, text: &str) -> Result<()> {
    // X11 providers can fork a long-lived selection owner that inherits stderr.
    // A pipe would keep wait_with_output blocked after the provider exits. An
    // anonymous file captures diagnostics while letting us wait only for the
    // provider that accepted stdin; no clipboard-owner lifecycle is ours to stop.
    let mut diagnostics =
        tempfile::tempfile().context("cannot create clipboard diagnostic file")?;
    let provider_stderr = diagnostics
        .try_clone()
        .context("cannot open clipboard provider diagnostics")?;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::from(provider_stderr))
        .spawn()
        .context("clipboard provider is unavailable")?;
    // Close stdin before waiting: providers finalize ownership only after EOF.
    // Always reap the child, including when it closes stdin before all bytes.
    let written = child
        .stdin
        .take()
        .context("clipboard provider has no stdin")?
        .write_all(text.as_bytes());
    let status = child
        .wait()
        .context("failed to wait for clipboard provider")?;
    if !status.success() {
        diagnostics
            .rewind()
            .context("cannot rewind clipboard provider diagnostics")?;
        let mut stderr = Vec::new();
        // Error text is UI-bound. A noisy provider must not produce an unbounded
        // allocation or an enormous menu error, even if it wrote more to disk.
        diagnostics
            .take(4096)
            .read_to_end(&mut stderr)
            .context("cannot read clipboard provider diagnostics")?;
        bail!(
            "clipboard provider exited with {}: {}",
            status,
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    written.context("clipboard provider did not accept all text")
}

#[cfg(test)]
mod tests {
    use super::write;
    use std::fs;
    use std::process::Command;

    #[test]
    fn provider_receives_literal_text_and_eof() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("clipboard");
        let text = "hello ' \" $(false) `false`\nwith another line\n";
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "cat > \"$1\"", "clipboard-test"]);
        command.arg(&destination);
        write(&mut command, text).unwrap();
        assert_eq!(fs::read_to_string(destination).unwrap(), text);
    }

    #[test]
    fn provider_failure_is_not_reported_as_success() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "cat >/dev/null; echo clipboard-denied >&2; exit 7"]);
        let error = format!("{:#}", write(&mut command, "context").unwrap_err());
        assert!(error.contains("7"));
        assert!(error.contains("clipboard-denied"));
    }

    #[test]
    fn provider_exit_does_not_wait_for_background_clipboard_owner() {
        use std::sync::mpsc;
        use std::thread;
        use std::time::{Duration, Instant};

        let directory = tempfile::tempdir().unwrap();
        let release = directory.path().join("release-owner");
        let provider_release = release.clone();
        let finished = directory.path().join("owner-finished");
        let provider_finished = finished.clone();
        let (send, receive) = mpsc::channel();
        let provider = thread::spawn(move || {
            let mut command = Command::new("/bin/sh");
            // Like xclip, the parent accepts stdin and exits while a clipboard
            // owner keeps its inherited stderr open until ownership is released.
            command.args([
                "-c",
                "cat >/dev/null; (i=0; while [ ! -e \"$1\" ] && [ \"$i\" -lt 250 ]; do sleep 0.02; i=$((i+1)); done; : > \"$2\") & exit 0",
                "clipboard-test",
            ]);
            command.arg(provider_release).arg(provider_finished);
            send.send(write(&mut command, "context")).unwrap();
        });
        let result = receive.recv_timeout(Duration::from_secs(2));
        // Release the descendant even on regression so this test cannot leave a
        // clipboard owner running or a blocked worker after the assertion fails.
        fs::write(release, "release").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !finished.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        provider.join().unwrap();
        assert!(finished.exists(), "background clipboard owner did not exit");
        result
            .expect("copy waited for inherited stderr instead of provider exit")
            .unwrap();
    }

    #[test]
    fn missing_provider_is_actionable() {
        let error = format!(
            "{:#}",
            write(&mut Command::new("/nonexistent/herdr-clipboard"), "context").unwrap_err()
        );
        assert!(error.contains("clipboard provider is unavailable"));
    }
}
