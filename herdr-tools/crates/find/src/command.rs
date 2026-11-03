//! Runs a line picker's source command on the load worker. The command is argv
//! (no shell), runs in the source pane's directory with stdin closed, and has a
//! whole-run deadline and output caps, so a hung or chatty program can delay
//! only its own list, never typing or Escape.
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub(crate) const DEADLINE: Duration = Duration::from_secs(5);
const MAX_BYTES: u64 = 4 * 1024 * 1024;
const MAX_LINES: usize = 50_000;
const MAX_STDERR: u64 = 64 * 1024;

/// Usable lines in output order, plus what had to be left out.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Output {
    pub lines: Vec<String>,
    /// Lines that are not UTF-8 or contain control characters (tabs included).
    /// They could not be shown faithfully, and typing them into a shell could
    /// run keys (a CR) or end a bracketed paste (ESC), so they never become items.
    pub skipped: usize,
    pub truncated: bool,
}

/// Errors are user-facing sentences: the popup shows them as they are.
pub(crate) fn run(argv: &[String], cwd: &Path, deadline: Duration) -> Result<Output, String> {
    let program = argv.first().map(String::as_str).unwrap_or_default();
    let name = Path::new(program)
        .file_name()
        .map_or(program.into(), |n| n.to_string_lossy());
    let mut child = Command::new(program)
        .args(&argv[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => {
                format!("{name} isn't installed or isn't on Herdr's PATH")
            }
            _ => format!("Couldn't run {name}: {error}"),
        })?;
    // Readers own the pipes so a full pipe never blocks the child, and the
    // caps bound memory. Dropping a capped pipe ends a chatty child (EPIPE).
    let (sender, receiver) = mpsc::channel();
    let stdout = child
        .stdout
        .take()
        .map(|pipe| (Box::new(pipe) as Box<dyn Read + Send>, MAX_BYTES + 1, true));
    let stderr = child
        .stderr
        .take()
        .map(|pipe| (Box::new(pipe) as Box<dyn Read + Send>, MAX_STDERR, false));
    for (pipe, cap, is_stdout) in [stdout, stderr].into_iter().flatten() {
        let sender = sender.clone();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.take(cap).read_to_end(&mut bytes);
            let _ = sender.send((is_stdout, bytes));
        });
    }
    drop(sender);
    let end = Instant::now() + deadline;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{name} took longer than {}s",
                    deadline.as_secs_f32()
                ));
            }
            Err(error) => return Err(format!("Couldn't wait for {name}: {error}")),
        }
    };
    // A grandchild that inherited a pipe could keep it open after the child
    // exits; give the readers the remaining deadline, then use what arrived.
    let (mut out, mut err) = (Vec::new(), Vec::new());
    for _ in 0..2 {
        let remaining = end.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining.max(Duration::from_millis(50))) {
            Ok((true, bytes)) => out = bytes,
            Ok((false, bytes)) => err = bytes,
            Err(_) => break,
        }
    }
    let capped = out.len() as u64 > MAX_BYTES;
    if capped {
        // The cap cut the last line mid-way; never offer a partial path.
        let whole = out.iter().rposition(|b| *b == b'\n').unwrap_or(0);
        out.truncate(whole);
    }
    let mut output = split(&out);
    output.truncated |= capped;
    if !status.success() && output.lines.is_empty() {
        let detail = String::from_utf8_lossy(&err)
            .lines()
            .map(crate::catalog::clean)
            .find(|line| !line.is_empty())
            .unwrap_or_default();
        let code = status
            .code()
            .map_or_else(|| "a signal".to_owned(), |code| format!("exit {code}"));
        return Err(if detail.is_empty() {
            format!("{name} failed ({code})")
        } else {
            format!("{name} failed ({code}): {detail}")
        });
    }
    Ok(output)
}

fn split(bytes: &[u8]) -> Output {
    let mut output = Output::default();
    let mut seen = std::collections::HashSet::new();
    for raw in bytes.split(|b| *b == b'\n') {
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        let Ok(line) = std::str::from_utf8(raw) else {
            output.skipped += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        if line.chars().any(|c| {
            c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        }) {
            output.skipped += 1;
            continue;
        }
        if output.lines.len() == MAX_LINES {
            output.truncated = true;
            break;
        }
        if seen.insert(line) {
            output.lines.push(line.to_owned());
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Result<Output, String> {
        let argv = ["sh", "-c", script].map(String::from);
        run(&argv, Path::new("/"), Duration::from_secs(2))
    }

    #[test]
    fn lines_keep_order_drop_blanks_duplicates_and_trailing_cr() {
        let output = sh(r"printf '/b\r\n\n  \n/a\n/b\n/c with space'").unwrap();
        assert_eq!(output.lines, ["/b", "/a", "/c with space"]);
        assert_eq!(output.skipped, 0);
        assert!(!output.truncated);
    }

    #[test]
    fn control_characters_and_invalid_utf8_are_skipped_and_counted() {
        let output =
            sh(r"printf '/ok\n/tab\there\n/esc\033[31m\n/bad\377\n/bidi\342\200\256x\n'").unwrap();
        assert_eq!(output.lines, ["/ok"]);
        assert_eq!(output.skipped, 4);
    }

    #[test]
    fn command_runs_in_the_given_directory_with_stdin_closed() {
        let dir = tempfile::tempdir().unwrap();
        let argv = ["sh", "-c", "pwd -P; cat"].map(String::from);
        let output = run(&argv, dir.path(), Duration::from_secs(2)).unwrap();
        let expected = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(output.lines, [expected.display().to_string()]);
    }

    #[test]
    fn missing_programs_failures_and_timeouts_explain_themselves() {
        let missing = run(
            &["definitely-not-zoxide-xyz".to_owned()],
            Path::new("/"),
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(
            missing,
            "definitely-not-zoxide-xyz isn't installed or isn't on Herdr's PATH"
        );
        assert_eq!(
            sh("echo 'database is locked' >&2; exit 3").unwrap_err(),
            "sh failed (exit 3): database is locked"
        );
        assert_eq!(sh("exit 4").unwrap_err(), "sh failed (exit 4)");
        // Output wins over a non-zero exit: show what the program listed.
        assert_eq!(sh("echo /x; exit 1").unwrap().lines, ["/x"]);
        let started = Instant::now();
        let argv = ["sh", "-c", "sleep 5"].map(String::from);
        let slow = run(&argv, Path::new("/"), Duration::from_millis(200)).unwrap_err();
        assert!(slow.contains("took longer"), "{slow}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn oversized_output_is_truncated_at_a_whole_line() {
        let output = sh("yes /some/long/path/segment | head -c 5000000").unwrap();
        assert!(output.truncated);
        // Duplicates collapse, and the cut-off partial line is never offered.
        assert_eq!(output.lines, ["/some/long/path/segment"]);
        let many: Vec<String> = (0..MAX_LINES + 5).map(|i| format!("/p{i}")).collect();
        let split = split(many.join("\n").as_bytes());
        assert_eq!(split.lines.len(), MAX_LINES);
        assert!(split.truncated);
    }
}
