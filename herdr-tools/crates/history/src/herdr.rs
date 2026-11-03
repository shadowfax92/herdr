//! The Herdr calls only `history` makes, on top of [`tools_core::Host`]: reading
//! a pane's rendered output through `herdr pane read`.
//!
//! Talon's own client (binary resolution, client width, popup open, notify,
//! context parsing) moved to tools-core: [`Host::client_width`],
//! [`Host::open_popup_sized`], [`Host::notify`] and [`tools_core::Invocation`].

use std::ffi::OsString;

use anyhow::Result;
use tools_core::Host;

pub fn read_visible(host: &Host, pane_id: &str, ansi: bool) -> Result<String> {
    read_pane(host, pane_id, "visible", ansi, None)
}

pub fn read_recent_unwrapped(host: &Host, pane_id: &str, ansi: bool, lines: u32) -> Result<String> {
    read_pane(host, pane_id, "recent-unwrapped", ansi, Some(lines))
}

fn read_pane(
    host: &Host,
    pane_id: &str,
    source: &str,
    ansi: bool,
    lines: Option<u32>,
) -> Result<String> {
    let format = if ansi { "ansi" } else { "text" };
    let mut args = vec![
        OsString::from("pane"),
        OsString::from("read"),
        OsString::from(pane_id),
        OsString::from("--source"),
        OsString::from(source),
    ];
    if let Some(lines) = lines {
        args.push(OsString::from("--lines"));
        args.push(OsString::from(lines.to_string()));
    }
    args.push(OsString::from("--format"));
    args.push(OsString::from(format));
    host.cli(args)
}

#[cfg(test)]
mod tests {
    use tools_core::testing::fake_cli;
    use tools_core::Invocation;

    use super::*;

    /// Talon's own context parser is now core's [`Invocation`]; `history open`
    /// still depends on it refusing a missing or blank focused pane.
    #[test]
    fn invocation_context_requires_a_nonempty_focused_pane() {
        let context = Invocation::from_context_json(
            r#"{"focused_pane_id":"w2:p1","focused_pane_cwd":"/tmp/project"}"#,
        )
        .unwrap();

        assert_eq!(context.source_pane().unwrap(), "w2:p1");
        assert!(Invocation::from_context_json("{}")
            .unwrap()
            .source_pane()
            .is_err());
        assert!(
            Invocation::from_context_json(r#"{"focused_pane_id":"   "}"#)
                .unwrap()
                .source_pane()
                .is_err()
        );
        assert!(Invocation::from_context_json("not-json").is_err());
    }

    #[test]
    fn pane_reads_pass_source_lines_and_format_through() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("args");
        let bin = fake_cli(
            dir.path(),
            &format!("printf '%s\\n' \"$*\" >> '{}'; printf 'out'", log.display()),
        );
        let host = Host::new(bin, None);

        assert_eq!(
            read_recent_unwrapped(&host, "w1:p1", true, 1000).unwrap(),
            "out"
        );
        assert_eq!(read_visible(&host, "w1:p1", false).unwrap(), "out");
        assert_eq!(
            std::fs::read_to_string(log).unwrap(),
            "pane read w1:p1 --source recent-unwrapped --lines 1000 --format ansi\n\
             pane read w1:p1 --source visible --format text\n"
        );
    }
}
