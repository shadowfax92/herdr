//! `herdr-tools find`, run as the actual binary so a green library test cannot
//! hide missing plugin entrypoints. Ported from herdr-jump's `tests/cli.rs`; it
//! lives in the root package because only the package that owns the
//! `herdr-tools` binary gets `CARGO_BIN_EXE_herdr-tools`.
use std::process::Command;

fn find() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-tools"));
    command.arg("find");
    // A test run inside Herdr inherits the owner's live socket and context.
    for key in [
        "HERDR_SOCKET_PATH",
        "HERDR_PLUGIN_CONTEXT_JSON",
        "HERDR_PANE_ID",
        "HERDR_JUMP_SOURCE_PANE",
        "HERDR_JUMP_PICKER",
    ] {
        command.env_remove(key);
    }
    command
}

#[test]
fn tests_that_help_describes_plugin_entrypoints() {
    let result = find().arg("--help").output().unwrap();
    assert!(result.status.success());
    let text = String::from_utf8_lossy(&result.stdout);
    assert!(text.contains("open") && text.contains("picker"), "{text}");
    // Keys are bound in Herdr's config.toml now, never by the plugin.
    assert!(!text.contains("install-keybindings"), "{text}");
}

#[test]
fn tests_that_open_without_host_context_fails_clearly() {
    let result = find().arg("open").output().unwrap();
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.starts_with("herdr-tools find: "), "{stderr}");
    assert!(stderr.contains("HERDR_SOCKET_PATH"), "{stderr}");
}

#[test]
fn tests_that_an_unknown_picker_fails_before_any_host_call() {
    let result = find().args(["open", "nope"]).output().unwrap();
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("unknown picker \"nope\""), "{stderr}");
}

#[test]
fn tests_that_picker_without_its_launch_context_names_the_action() {
    let result = find().arg("picker").output().unwrap();
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("shadowfax.tools.find"), "{stderr}");
}
