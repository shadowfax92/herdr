//! `herdr scope`: read and change the local server's focus and hide scope
//! (`crate::scope`).
//!
//! The server stores the scope and judges tabs; this module only turns
//! subcommands into `scope.get`, `scope.set`, `scope.clear` and `scope.pick`.
//! Adding or removing one target is a read followed by a whole-field
//! `scope.set`: the server never flips or merges, so a retried request cannot
//! undo itself. Targets go out as labels; the server pins unique local labels
//! to ids, so removal matches by label as well as by `same_target`.

use crate::api::schema::{EmptyParams, Method, Request};
use crate::scope::{ScopePickMode, ScopePickParams, ScopeSetParams, ScopeState, ScopeTarget};

const USAGE: &str = "usage: herdr scope get [--json] | focus [TARGET...] [--add] | unfocus | toggle | hide TARGET... | unhide TARGET... | clear | pick [focus|hide]";

pub(super) fn run_scope_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(String::as_str) else {
        print_scope_help();
        return Ok(2);
    };
    if matches!(subcommand, "help" | "--help" | "-h") {
        print_scope_help();
        return Ok(0);
    }
    let parsed = match parse_scope_args(subcommand, &args[1..]) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            eprintln!("{USAGE}");
            return Ok(2);
        }
    };
    run(parsed)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ScopeCommand {
    Get,
    Focus {
        targets: Vec<ScopeTarget>,
        add: bool,
    },
    Unfocus,
    Toggle,
    Hide(Vec<ScopeTarget>),
    Unhide(Vec<ScopeTarget>),
    Clear,
    Pick(ScopePickMode),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedScopeArgs {
    command: ScopeCommand,
    json: bool,
}

fn parse_scope_args(subcommand: &str, args: &[String]) -> Result<ParsedScopeArgs, String> {
    let mut json = false;
    let mut add = false;
    let mut words = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            "--add" if subcommand == "focus" => add = true,
            flag if flag.starts_with("--") => return Err(format!("unknown option: {flag}")),
            word => words.push(word),
        }
    }
    let targets = |words: &[&str]| {
        words
            .iter()
            .map(|word| ScopeTarget::parse(word))
            .collect::<Result<Vec<_>, _>>()
    };
    let no_words = |command: ScopeCommand| {
        if words.is_empty() {
            Ok(command)
        } else {
            Err(format!("`herdr scope {subcommand}` takes no targets"))
        }
    };
    let command = match subcommand {
        "get" => no_words(ScopeCommand::Get)?,
        "focus" => ScopeCommand::Focus {
            targets: targets(&words)?,
            add,
        },
        "unfocus" => no_words(ScopeCommand::Unfocus)?,
        "toggle" => no_words(ScopeCommand::Toggle)?,
        "hide" | "unhide" => {
            if words.is_empty() {
                return Err(format!(
                    "`herdr scope {subcommand}` needs at least one TARGET"
                ));
            }
            let targets = targets(&words)?;
            if subcommand == "hide" {
                ScopeCommand::Hide(targets)
            } else {
                ScopeCommand::Unhide(targets)
            }
        }
        "clear" => no_words(ScopeCommand::Clear)?,
        "pick" => {
            if json {
                return Err("`herdr scope pick` prints nothing to format as JSON".into());
            }
            match words.as_slice() {
                [] | ["focus"] => ScopeCommand::Pick(ScopePickMode::Focus),
                ["hide"] => ScopeCommand::Pick(ScopePickMode::Hide),
                _ => return Err("`herdr scope pick` takes `focus` or `hide`".into()),
            }
        }
        other => return Err(format!("unknown scope command: {other}")),
    };
    Ok(ParsedScopeArgs { command, json })
}

fn run(parsed: ParsedScopeArgs) -> std::io::Result<i32> {
    let ParsedScopeArgs { command, json } = parsed;
    let response = match command {
        ScopeCommand::Get => send(Method::ScopeGet(EmptyParams::default()))?,
        ScopeCommand::Focus { targets, add } => {
            let params = if targets.is_empty() {
                // No targets: switch the kept set back on.
                ScopeSetParams {
                    focus_active: Some(true),
                    ..ScopeSetParams::default()
                }
            } else {
                let focus_targets = if add {
                    let Some(scope) = read_scope()? else {
                        return Ok(1);
                    };
                    with_added(scope.focus.targets, targets)
                } else {
                    targets
                };
                ScopeSetParams {
                    focus_targets: Some(focus_targets),
                    focus_active: Some(true),
                    hidden: None,
                }
            };
            send(Method::ScopeSet(params))?
        }
        ScopeCommand::Unfocus => send(Method::ScopeSet(ScopeSetParams {
            focus_active: Some(false),
            ..ScopeSetParams::default()
        }))?,
        ScopeCommand::Toggle => {
            let Some(scope) = read_scope()? else {
                return Ok(1);
            };
            send(Method::ScopeSet(ScopeSetParams {
                focus_active: Some(!scope.focus.active),
                ..ScopeSetParams::default()
            }))?
        }
        ScopeCommand::Hide(targets) => {
            let Some(scope) = read_scope()? else {
                return Ok(1);
            };
            send(Method::ScopeSet(ScopeSetParams {
                hidden: Some(with_added(scope.hidden, targets)),
                ..ScopeSetParams::default()
            }))?
        }
        ScopeCommand::Unhide(targets) => {
            let Some(scope) = read_scope()? else {
                return Ok(1);
            };
            let (hidden, unmatched) = without(scope.hidden, &targets);
            for target in unmatched {
                eprintln!("not hidden: {}", target.display());
            }
            send(Method::ScopeSet(ScopeSetParams {
                hidden: Some(hidden),
                ..ScopeSetParams::default()
            }))?
        }
        ScopeCommand::Clear => send(Method::ScopeClear(EmptyParams::default()))?,
        ScopeCommand::Pick(mode) => {
            let response = send(Method::ScopePick(ScopePickParams { mode }))?;
            if response.get("error").is_some() {
                eprintln!("{response}");
                return Ok(1);
            }
            return Ok(0);
        }
    };
    if response.get("error").is_some() {
        eprintln!("{response}");
        return Ok(1);
    }
    if json {
        println!("{response}");
        return Ok(0);
    }
    match scope_from_response(&response) {
        Some(scope) => {
            print!("{}", render_human(&scope, &response["result"]["local"]));
            Ok(0)
        }
        None => {
            eprintln!("unexpected scope response: {response}");
            Ok(1)
        }
    }
}

fn send(method: Method) -> std::io::Result<serde_json::Value> {
    super::send_request(&Request {
        id: "cli:scope".into(),
        method,
    })
}

/// `None` (after reporting the error) when the server refused `scope.get`.
fn read_scope() -> std::io::Result<Option<ScopeState>> {
    let response = send(Method::ScopeGet(EmptyParams::default()))?;
    if response.get("error").is_some() {
        eprintln!("{response}");
        return Ok(None);
    }
    let scope = scope_from_response(&response);
    if scope.is_none() {
        eprintln!("unexpected scope response: {response}");
    }
    Ok(scope)
}

fn scope_from_response(response: &serde_json::Value) -> Option<ScopeState> {
    serde_json::from_value(response.get("result")?.get("scope")?.clone()).ok()
}

/// Whether a stored target is the one the user typed. Besides `same_target`,
/// a typed label matches a stored target the server pinned to an id, and a
/// typed machine label matches a target stored under the machine's key.
fn matches_typed(stored: &ScopeTarget, typed: &ScopeTarget) -> bool {
    if stored.same_target(typed) {
        return true;
    }
    let machine = stored.machine.eq_ignore_ascii_case(&typed.machine)
        || stored
            .machine_label
            .as_deref()
            .is_some_and(|label| label.eq_ignore_ascii_case(&typed.machine));
    machine
        && typed.workspace_label.is_some()
        && stored.workspace_label == typed.workspace_label
        && stored.tab_label == typed.tab_label
}

fn with_added(mut list: Vec<ScopeTarget>, targets: Vec<ScopeTarget>) -> Vec<ScopeTarget> {
    for target in targets {
        if !list.iter().any(|stored| matches_typed(stored, &target)) {
            list.push(target);
        }
    }
    list
}

/// Removes every stored target a typed one matches; returns the kept list and
/// the typed targets that matched nothing.
fn without(
    list: Vec<ScopeTarget>,
    targets: &[ScopeTarget],
) -> (Vec<ScopeTarget>, Vec<&ScopeTarget>) {
    let unmatched = targets
        .iter()
        .filter(|target| !list.iter().any(|stored| matches_typed(stored, target)))
        .collect();
    let kept = list
        .into_iter()
        .filter(|stored| !targets.iter().any(|target| matches_typed(stored, target)))
        .collect();
    (kept, unmatched)
}

fn render_human(scope: &ScopeState, local: &serde_json::Value) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    let focus = if scope.focus.active { "on" } else { "off" };
    let _ = writeln!(out, "focus: {focus}");
    if scope.focus.active && scope.focus.targets.is_empty() {
        let _ = writeln!(out, "  (no targets; nothing is filtered)");
    }
    for target in &scope.focus.targets {
        let _ = writeln!(out, "  {}", target.display());
    }
    if scope.hidden.is_empty() {
        let _ = writeln!(out, "hidden: none");
    } else {
        let _ = writeln!(out, "hidden:");
        for target in &scope.hidden {
            let origin = if target.config { " (config)" } else { "" };
            let _ = writeln!(out, "  {}{origin}", target.display());
        }
    }
    if let Some(tabs) = local["tabs"].as_array() {
        let admitted = tabs
            .iter()
            .filter(|tab| tab["admitted"].as_bool() == Some(true))
            .count();
        let _ = writeln!(out, "local tabs shown: {admitted} of {}", tabs.len());
    }
    out
}

fn print_scope_help() {
    eprintln!("herdr scope commands (focus and hide; TARGET is [machine:]workspace[/tab]):");
    eprintln!("  herdr scope get [--json]");
    eprintln!("  herdr scope focus [TARGET...] [--add] [--json]");
    eprintln!("  herdr scope unfocus [--json]");
    eprintln!("  herdr scope toggle [--json]");
    eprintln!("  herdr scope hide TARGET... [--json]");
    eprintln!("  herdr scope unhide TARGET... [--json]");
    eprintln!("  herdr scope clear [--json]");
    eprintln!("  herdr scope pick [focus|hide]");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn target(text: &str) -> ScopeTarget {
        ScopeTarget::parse(text).unwrap()
    }

    #[test]
    fn subcommands_parse_targets_and_flags() {
        assert_eq!(
            parse_scope_args("focus", &args(&["setup", "studio:main/outreach", "--add"])),
            Ok(ParsedScopeArgs {
                command: ScopeCommand::Focus {
                    targets: vec![target("setup"), target("studio:main/outreach")],
                    add: true,
                },
                json: false,
            })
        );
        assert_eq!(
            parse_scope_args("get", &args(&["--json"])).map(|parsed| parsed.json),
            Ok(true)
        );
        assert_eq!(
            parse_scope_args("pick", &args(&["hide"])).map(|parsed| parsed.command),
            Ok(ScopeCommand::Pick(ScopePickMode::Hide))
        );
        assert_eq!(
            parse_scope_args("pick", &[]).map(|parsed| parsed.command),
            Ok(ScopeCommand::Pick(ScopePickMode::Focus))
        );
    }

    #[test]
    fn bad_arguments_are_usage_errors() {
        for (subcommand, rest) in [
            ("hide", &[][..]),
            ("unhide", &[][..]),
            ("get", &["setup"][..]),
            ("toggle", &["--add"][..]),
            ("pick", &["sideways"][..]),
            ("focus", &["studio:/x"][..]),
            ("nope", &[][..]),
        ] {
            assert!(
                parse_scope_args(subcommand, &args(rest)).is_err(),
                "{subcommand} {rest:?} should not parse"
            );
        }
    }

    #[test]
    fn typed_labels_match_targets_the_server_pinned_to_ids() {
        let pinned = ScopeTarget {
            workspace_id: Some("w1".into()),
            ..target("setup")
        };
        let remote = ScopeTarget {
            machine: "ssh:0123".into(),
            machine_label: Some("studio".into()),
            workspace_id: Some("w9".into()),
            ..target("studio:main")
        };
        assert!(matches_typed(&pinned, &target("setup")));
        assert!(matches_typed(&pinned, &target("Local:setup")));
        assert!(matches_typed(&remote, &target("studio:main")));
        assert!(!matches_typed(&pinned, &target("setup/herdr")));
        assert!(!matches_typed(&pinned, &target("studio:setup")));
        // A config hide covers every machine; only `*:ft` names it.
        let config = ScopeState::from_config_hidden(&["ft".into()])
            .hidden
            .remove(0);
        assert!(!matches_typed(&config, &target("ft")));
        assert!(matches_typed(&config, &target("*:ft")));
    }

    #[test]
    fn hide_adds_once_and_unhide_reports_what_matched_nothing() {
        let hidden = with_added(
            vec![target("setup")],
            vec![target("setup"), target("admin")],
        );
        assert_eq!(hidden, [target("setup"), target("admin")]);
        let typed = [target("admin"), target("ghost")];
        let (kept, unmatched) = without(hidden, &typed);
        assert_eq!(kept, [target("setup")]);
        assert_eq!(unmatched, [&target("ghost")]);
    }

    #[test]
    fn human_output_lists_focus_and_hides() {
        let mut scope = ScopeState::from_config_hidden(&["ft".into()]);
        scope.focus.active = true;
        scope.focus.targets.push(target("setup"));
        scope.hidden.push(target("studio:main/outreach"));
        let local = serde_json::json!({"tabs": [
            {"workspace_id": "w1", "tab_id": "w1:t1", "admitted": true},
            {"workspace_id": "w2", "tab_id": "w2:t1", "admitted": false},
        ]});
        assert_eq!(
            render_human(&scope, &local),
            "focus: on\n  local:setup\nhidden:\n  *:ft (config)\n  studio:main/outreach\nlocal tabs shown: 1 of 2\n"
        );
        assert_eq!(
            render_human(&ScopeState::default(), &serde_json::Value::Null),
            "focus: off\nhidden: none\n"
        );
    }
}
