//! What Enter does with a line picker's selection (the marks, or the cursor
//! row). Only the popup's foreground
//! loop calls `deliver`, after the user pressed Enter; Escape never reaches it,
//! so cancelling can neither type nor copy anything.
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::host::{Host, HostError};
use crate::pickers::{Deliver, Kind};

/// Overrides the clipboard program (run with `sh -c`, text on stdin). The
/// acceptance test uses it to observe copies without touching the real
/// clipboard; it also rescues systems without pbcopy/wl-copy/xclip/xsel.
pub(crate) const COPY_COMMAND_ENV: &str = "HERDR_JUMP_COPY_COMMAND";
const COPY_DEADLINE: Duration = Duration::from_secs(2);

/// The text Enter types into the pane. Paths are single-quoted only when a
/// shell would split or expand them; `'\''` closes, escapes and reopens a
/// quote, which fish, bash and zsh all read back as the original path. Agent
/// prompts read a quoted path just as well, so one form serves every pane.
pub(crate) fn typed(value: &str, kind: Kind) -> String {
    match kind {
        Kind::Text => value.to_owned(),
        Kind::Path => {
            let plain = !value.is_empty()
                && !value.starts_with('~')
                && value
                    .chars()
                    .all(|c| c.is_alphanumeric() || "/._-+,:@%=~".contains(c));
            if plain {
                value.to_owned()
            } else {
                format!("'{}'", value.replace('\'', r"'\''"))
            }
        }
    }
}

/// What Enter types for `values`: each one `typed`, separated by spaces, so a
/// shell reads one argument per value.
pub(crate) fn typed_all(values: &[impl AsRef<str>], kind: Kind) -> String {
    values
        .iter()
        .map(|value| typed(value.as_ref(), kind))
        .collect::<Vec<_>>()
        .join(" ")
}

/// What Enter copies. One value stays raw (what `m_path` copied), ready for
/// Finder or a browser; several are exactly the typed text, because raw paths
/// joined by spaces could not be told apart again.
pub(crate) fn copied(values: &[impl AsRef<str>], kind: Kind) -> String {
    match values {
        [one] => one.as_ref().to_owned(),
        _ => typed_all(values, kind),
    }
}

/// Some Enter effects failed. `done` lists the ones that succeeded, so a retry
/// can skip them; `message` names both, so the user knows what is on the
/// clipboard and what reached the pane.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Failure {
    pub done: Vec<Deliver>,
    pub message: String,
}

/// Runs each Enter effect in the picker's order, continuing past failures.
/// The pane gets `typed_all` as one input, the clipboard gets `copied`.
pub(crate) fn deliver(
    host: &Host,
    source_pane: &str,
    values: &[String],
    kind: Kind,
    effects: &[Deliver],
) -> Result<(), Failure> {
    let mut done = Vec::new();
    let mut said = Vec::new();
    let mut failed = Vec::new();
    for effect in effects {
        let (result, success, failure) = match effect {
            Deliver::Copy => (copy(&copied(values, kind)), "Copied", "couldn't copy"),
            Deliver::Paste => (
                paste(host, source_pane, &typed_all(values, kind)),
                "Typed into the pane",
                "couldn't type into the pane",
            ),
        };
        match result {
            Ok(()) => {
                done.push(*effect);
                said.push(success);
            }
            Err(error) => failed.push(format!("{failure}: {error}")),
        }
    }
    if failed.is_empty() {
        return Ok(());
    }
    let failures = failed.join(" · ");
    let message = if said.is_empty() {
        let mut chars = failures.chars();
        chars
            .next()
            .map(|first| first.to_uppercase().chain(chars).collect())
            .unwrap_or_default()
    } else {
        format!("{}, but {failures}", said.join(" and "))
    };
    Err(Failure { done, message })
}

fn paste(host: &Host, pane: &str, text: &str) -> Result<(), String> {
    // Items never contain control characters (see `command`); refuse anyway,
    // because a CR here would run a command in the user's shell.
    if text.chars().any(char::is_control) {
        return Err("it contains control characters".into());
    }
    host.send_input(pane, text)
        .map_err(|error| match error.downcast_ref::<HostError>() {
            Some(e) if e.code == "pane_not_found" => "the pane you came from has closed".into(),
            _ => format!("{error:#}"),
        })
}

fn copy(text: &str) -> Result<(), String> {
    let mut command = match std::env::var(COPY_COMMAND_ENV) {
        Ok(script) if !script.is_empty() => {
            let mut command = Command::new("sh");
            command.args(["-c", &script]);
            command
        }
        _ => platform_clipboard()?,
    };
    let name = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => format!("{name} isn't installed"),
            _ => format!("{name}: {error}"),
        })?;
    // The clipboard owner reads stdin to its end; dropping the pipe after the
    // write is the end-of-text signal. A helper that never reads would block a
    // large write, so it happens on a thread and the whole copy (write and
    // exit) shares one deadline. Killing the helper closes its end of the
    // pipe, which ends a blocked write with EPIPE.
    let (sender, written) = mpsc::channel();
    let stdin = child.stdin.take();
    let bytes = text.as_bytes().to_vec();
    std::thread::spawn(move || {
        let result = stdin.map(|mut stdin| stdin.write_all(&bytes));
        let _ = sender.send(result);
    });
    let end = Instant::now() + COPY_DEADLINE;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{name} did not finish"));
            }
        }
    };
    // A grandchild could still hold the pipe; never wait past the deadline.
    let remaining = end.saturating_duration_since(Instant::now());
    match written.recv_timeout(remaining.max(Duration::from_millis(50))) {
        Ok(Some(Ok(()))) if status.success() => Ok(()),
        Ok(Some(Err(error))) => Err(format!("{name}: {error}")),
        Ok(_) => Err(format!("{name} failed ({status})")),
        Err(_) => Err(format!("{name} did not read the text")),
    }
}

fn platform_clipboard() -> Result<Command, String> {
    if cfg!(target_os = "macos") {
        let mut command = Command::new("pbcopy");
        // pbcopy decodes stdin by locale; without UTF-8 it mangles non-ASCII.
        command.env("LC_CTYPE", "UTF-8");
        return Ok(command);
    }
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|d| !d.is_empty());
    let candidates: &[&[&str]] = if wayland {
        &[&["wl-copy"], &["xclip", "-selection", "clipboard"]]
    } else {
        &[
            &["xclip", "-selection", "clipboard"],
            &["xsel", "--clipboard", "--input"],
        ]
    };
    let found = candidates.iter().find(|argv| {
        std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(argv[0]).is_file()))
    });
    let argv = found.ok_or("no clipboard program (wl-copy, xclip or xsel) is installed")?;
    let mut command = Command::new(argv[0]);
    command.args(&argv[1..]);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_paths_are_typed_bare_and_others_are_quoted() {
        for path in [
            "/Users/me/code/herdr-jump",
            "/Users/me/.config/nvim",
            "/a/b_c-d.e+f,g:h@i%j=k~l",
            "/Users/me/café/日本",
        ] {
            assert_eq!(typed(path, Kind::Path), path);
        }
        for (path, expected) in [
            ("/Users/me/My Drive", "'/Users/me/My Drive'"),
            ("/tmp/it's", r"'/tmp/it'\''s'"),
            ("/tmp/$HOME", "'/tmp/$HOME'"),
            ("/tmp/*", "'/tmp/*'"),
            ("/tmp/a#b", "'/tmp/a#b'"),
            ("/tmp/(x)", "'/tmp/(x)'"),
            ("/tmp/a!b", "'/tmp/a!b'"),
            ("~/x", "'~/x'"),
        ] {
            assert_eq!(typed(path, Kind::Path), expected);
        }
        assert_eq!(typed("feat x", Kind::Text), "feat x");
    }

    #[test]
    fn several_values_are_typed_and_copied_as_one_space_separated_text() {
        let both = ["/Users/me/My Drive", "/Users/me/code"];
        assert_eq!(
            typed_all(&both, Kind::Path),
            "'/Users/me/My Drive' /Users/me/code"
        );
        assert_eq!(copied(&both, Kind::Path), typed_all(&both, Kind::Path));
        // One value: the clipboard keeps the raw path, as before multi-select.
        assert_eq!(copied(&both[..1], Kind::Path), "/Users/me/My Drive");
        assert_eq!(typed_all(&["a b", "c"], Kind::Text), "a b c");
    }

    #[test]
    fn quoted_paths_round_trip_through_a_posix_shell() {
        for path in ["/Users/me/My Drive", "/tmp/it's here", "/tmp/$x `y` \"z\""] {
            let output = Command::new("sh")
                .args(["-c", &format!("printf %s {}", typed(path, Kind::Path))])
                .output()
                .unwrap();
            assert_eq!(String::from_utf8(output.stdout).unwrap(), path);
        }
    }

    // Everything that sets the process-wide clipboard override lives in this
    // one test, so parallel tests never observe each other's command.
    #[test]
    fn copy_and_paste_report_exactly_what_happened() {
        use serde_json::{json, Value};
        use std::sync::{Arc, Mutex};
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("clip");
        let copy_to_file = format!("cat > '{}'", clip.display());
        std::env::set_var(COPY_COMMAND_ENV, &copy_to_file);
        copy("/Users/me/My Drive").unwrap();
        assert_eq!(
            std::fs::read_to_string(&clip).unwrap(),
            "/Users/me/My Drive"
        );

        let typed_text = Arc::new(Mutex::new(Vec::new()));
        let seen = typed_text.clone();
        let (_sock, host) = crate::host::fake_host(move |req: Value| {
            seen.lock().unwrap().push(req["params"].clone());
            json!({"id":req["id"],"result":{}})
        });
        let both = [Deliver::Paste, Deliver::Copy];
        std::fs::remove_file(&clip).unwrap();
        let one = ["/Users/me/My Drive".to_owned()];
        deliver(&host, "w1:p1", &one, Kind::Path, &both).unwrap();
        // The pane gets the shell-safe form, the clipboard the raw path.
        assert_eq!(
            *typed_text.lock().unwrap(),
            [json!({"pane_id":"w1:p1","text":"'/Users/me/My Drive'"})]
        );
        assert_eq!(
            std::fs::read_to_string(&clip).unwrap(),
            "/Users/me/My Drive"
        );
        // Marked paths reach the pane as one input, in order, and the
        // clipboard holds the same text.
        let two = ["/Users/me/My Drive".to_owned(), "/Users/me/code".to_owned()];
        deliver(&host, "w1:p2", &two, Kind::Path, &both).unwrap();
        assert_eq!(
            typed_text.lock().unwrap()[1],
            json!({"pane_id":"w1:p2","text":"'/Users/me/My Drive' /Users/me/code"})
        );
        assert_eq!(
            std::fs::read_to_string(&clip).unwrap(),
            "'/Users/me/My Drive' /Users/me/code"
        );

        let (_sock, gone) = crate::host::fake_host(
            |req: Value| json!({"id":req["id"],"error":{"code":"pane_not_found","message":"gone"}}),
        );
        assert_eq!(
            deliver(&gone, "w1:p1", &["/a".to_owned()], Kind::Path, &both).unwrap_err(),
            Failure {
                done: vec![Deliver::Copy],
                message:
                    "Copied, but couldn't type into the pane: the pane you came from has closed"
                        .into(),
            }
        );
        std::env::set_var(COPY_COMMAND_ENV, "cat >/dev/null; exit 7");
        let failure = deliver(&host, "w1:p1", &["/a".to_owned()], Kind::Path, &both).unwrap_err();
        assert_eq!(failure.done, [Deliver::Paste]);
        let message = failure.message;
        assert!(
            message.starts_with("Typed into the pane, but couldn't copy: sh failed"),
            "{message}"
        );
        let failure = deliver(&gone, "w1:p1", &["/a".to_owned()], Kind::Path, &both).unwrap_err();
        assert!(failure.done.is_empty());
        let message = failure.message;
        assert!(
            message.starts_with(
                "Couldn't type into the pane: the pane you came from has closed · couldn't copy"
            ),
            "{message}"
        );
        // A helper that never reads stdin cannot hold a large copy (or the
        // popup) past the deadline.
        std::env::set_var(COPY_COMMAND_ENV, "exec sleep 10");
        let started = Instant::now();
        let error = copy(&"x".repeat(1 << 20)).unwrap_err();
        assert!(error.contains("did not finish"), "{error}");
        assert!(started.elapsed() < COPY_DEADLINE + Duration::from_secs(1));
        std::env::remove_var(COPY_COMMAND_ENV);
    }
}
