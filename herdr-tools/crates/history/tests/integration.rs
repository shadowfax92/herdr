//! `history open` against a fake Herdr. Talon spoke only the CLI; through
//! tools-core the client width (`pane.layout`) and the popup open
//! (`plugin.pane.open`) now go over the socket API, so the fake has two halves:
//! talon's shell-script CLI for `pane read` and `notification show`, and core's
//! fake socket for the rest. Marker files beside the script switch behaviors
//! (`herdr.empty-recent`, `herdr.whitespace-recent`, `herdr.ui-busy`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tempfile::tempdir;
use tools_core::testing::{fake_cli, fake_host, ok};
use tools_core::{Host, Invocation};
use tools_history::config::{Config, PatternDefinition};
use tools_history::snapshot::{launch, launch_with_reporting, LaunchOutcome, RunStore};

const CLI: &str = r#"
printf '%s\n' "$*" >> "$0.log"
if [ "$1 $2" = "pane read" ]; then
  pane="$3"
  format=text
  source=recent-unwrapped
  while [ "$#" -gt 0 ]; do
    if [ "$1" = "--format" ]; then format="$2"; fi
    if [ "$1" = "--source" ]; then source="$2"; fi
    shift
  done
  if [ "$pane" != "w1:p1" ]; then
    printf '%s\n' 'unexpected pane read' >&2
    exit 3
  elif [ "$source" = "recent-unwrapped" ] && [ -e "$0.empty-recent" ]; then
    :
  elif [ "$source" = "recent-unwrapped" ] && [ -e "$0.whitespace-recent" ]; then
    printf '   \n'
  elif [ "$source" = "visible" ]; then
    if [ "$format" = "ansi" ]; then printf '\033[36mvisible-value\033[0m\n'; else printf 'visible-value\n'; fi
  else
    if [ "$format" = "ansi" ]; then printf '\033[33mdeadbeef\033[0m\n'; else printf 'deadbeef\n'; fi
  fi
elif [ "$1 $2" = "notification show" ]; then
  printf '%s\n' '{"result":{"type":"ok"}}'
else
  printf '%s\n' 'unsupported fake command' >&2
  exit 2
fi"#;

/// A fake Herdr: `cli` holds the script, its `herdr.log` and marker files;
/// `requests` records every socket request.
struct FakeHerdr {
    cli: tempfile::TempDir,
    _socket: tempfile::TempDir,
    requests: Arc<Mutex<Vec<Value>>>,
    host: Host,
}

impl FakeHerdr {
    fn new() -> Self {
        let cli = tempdir().unwrap();
        let binary = fake_cli(cli.path(), CLI);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let ui_busy = cli.path().join("herdr.ui-busy");
        let (socket, socket_host) = fake_host(move |request| {
            seen.lock().unwrap().push(request.clone());
            match request["method"].as_str() {
                // A 70-column client: the pane area starts after a 30-column sidebar.
                Some("pane.layout") if request["params"]["pane_id"] == "w1:p1" => ok(
                    &request,
                    json!({"layout": {"workspace_id": "w1", "tab_id": "w1:t1", "zoomed": false,
                        "area": {"x": 30, "y": 1, "width": 40, "height": 10},
                        "focused_pane_id": "w1:p1"}, "type": "pane_layout"}),
                ),
                Some("plugin.pane.open") if ui_busy.exists() => json!({"id": request["id"],
                    "error": {"code": "ui_busy", "message": "another modal is active"}}),
                Some("plugin.pane.open") => ok(&request, json!({"type": "ok"})),
                _ => json!({"id": request["id"],
                    "error": {"code": "unsupported", "message": "unsupported fake request"}}),
            }
        });
        let host = Host::new(&binary, socket_host.socket_path().map(Path::to_path_buf))
            .with_timeout(Duration::from_secs(2));
        Self {
            cli,
            _socket: socket,
            requests,
            host,
        }
    }

    fn marker(&self, name: &str) -> PathBuf {
        self.cli.path().join(format!("herdr.{name}"))
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.marker("log")).unwrap_or_default()
    }

    fn requests(&self, method: &str) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["method"] == method)
            .cloned()
            .collect()
    }
}

fn context() -> Invocation {
    Invocation::from_context_json(r#"{"focused_pane_id":"w1:p1"}"#).unwrap()
}

/// Talon's built-in profiles as `migrate-config` writes them for an installed
/// talon without its own popup settings.
fn migrated_config() -> Config {
    let legacy = tempdir().unwrap();
    std::fs::create_dir(legacy.path().join("shadowfax.talon")).unwrap();
    let fragment = tools_history::migrate_legacy(legacy.path()).unwrap();
    let tools = tools_core::Config::parse(&toml::to_string(&fragment).unwrap()).unwrap();
    Config::from_tools(&tools).unwrap()
}

#[test]
fn concurrent_launch_preserves_the_existing_popup_and_removes_its_failed_handoff() {
    let herdr = FakeHerdr::new();
    let state = tempdir().unwrap();
    let store = RunStore::new(state.path()).unwrap();
    let context = context();

    let LaunchOutcome::Opened { run_id } =
        launch(&herdr.host, &context, &Config::default(), &store).unwrap();
    store.claim(&run_id).unwrap();
    std::fs::write(herdr.marker("ui-busy"), "").unwrap();

    let error = launch(&herdr.host, &context, &Config::default(), &store)
        .unwrap_err()
        .to_string();

    assert!(error.contains("ui_busy"));
    assert_eq!(herdr.requests("plugin.pane.open").len(), 2);
    assert!(herdr.requests("plugin.pane.close").is_empty());
    assert!(!herdr.log().contains("plugin pane close"));
    assert_eq!(std::fs::read_dir(store.root()).unwrap().count(), 0);
}

#[test]
fn launch_captures_only_focused_history_and_opens_a_responsive_popup() {
    let herdr = FakeHerdr::new();
    let state = tempdir().unwrap();
    let store = RunStore::new(state.path()).unwrap();
    let context = Invocation::from_context_json(
        r#"{"focused_pane_id":"w1:p1","focused_pane_cwd":"/tmp/project"}"#,
    )
    .unwrap();
    let config = migrated_config();

    let LaunchOutcome::Opened { run_id } = launch(&herdr.host, &context, &config, &store).unwrap();
    let snapshot = store.claim(&run_id).unwrap();
    let log = herdr.log();

    assert_eq!(snapshot.source_pane_id, "w1:p1");
    assert_eq!(snapshot.text, "deadbeef\n");
    assert!(snapshot.ansi.contains("\u{1b}[33m"));
    assert_eq!(
        snapshot
            .targets
            .iter()
            .map(|target| target.text.as_str())
            .collect::<Vec<_>>(),
        vec!["deadbeef"]
    );
    assert_eq!(snapshot.alphabet, Config::default().alphabet);
    assert!(log.contains("pane read w1:p1 --source recent-unwrapped --lines 1000 --format text"));
    assert!(log.contains("pane read w1:p1 --source recent-unwrapped --lines 1000 --format ansi"));
    assert!(!log.contains("pane read w1:p2"));
    assert_eq!(
        herdr.requests("pane.layout")[0]["params"],
        json!({"pane_id": "w1:p1"})
    );
    // The client is 70 cells wide, so the `laptop` profile (<= 310) applies.
    assert_eq!(
        herdr.requests("plugin.pane.open")[0]["params"],
        json!({"plugin_id": "shadowfax.tools", "entrypoint": "history", "placement": "popup",
               "focus": true, "width": "95%", "height": "90%",
               "env": {"HERDR_HISTORY_RUN_ID": run_id}})
    );
}

#[test]
fn unconfigured_popup_defers_to_the_manifest_size() {
    let herdr = FakeHerdr::new();
    let state = tempdir().unwrap();
    let store = RunStore::new(state.path()).unwrap();

    launch(&herdr.host, &context(), &Config::default(), &store).unwrap();

    let open = &herdr.requests("plugin.pane.open")[0]["params"];
    assert!(
        open.get("width").is_none() && open.get("height").is_none(),
        "{open}"
    );
}

#[test]
fn empty_recent_capture_falls_back_to_the_visible_source() {
    let herdr = FakeHerdr::new();
    std::fs::write(herdr.marker("empty-recent"), "").unwrap();
    let state = tempdir().unwrap();
    let store = RunStore::new(state.path()).unwrap();

    let LaunchOutcome::Opened { run_id } =
        launch(&herdr.host, &context(), &Config::default(), &store).unwrap();
    let snapshot = store.claim(&run_id).unwrap();
    let log = herdr.log();

    assert_eq!(snapshot.text, "visible-value\n");
    assert!(snapshot.ansi.contains("\u{1b}[36m"));
    assert!(log.contains("pane read w1:p1 --source visible --format text"));
    assert!(log.contains("pane read w1:p1 --source visible --format ansi"));
}

#[test]
fn whitespace_only_recent_capture_is_preserved() {
    let herdr = FakeHerdr::new();
    std::fs::write(herdr.marker("whitespace-recent"), "").unwrap();
    let state = tempdir().unwrap();
    let store = RunStore::new(state.path()).unwrap();

    let LaunchOutcome::Opened { run_id } =
        launch(&herdr.host, &context(), &Config::default(), &store).unwrap();
    let snapshot = store.claim(&run_id).unwrap();

    assert_eq!(snapshot.text, "   \n");
    assert!(!herdr.log().contains("pane read w1:p1 --source visible"));
}

#[test]
fn no_matches_still_opens_a_manual_selection_popup() {
    let herdr = FakeHerdr::new();
    let state = tempdir().unwrap();
    let store = RunStore::new(state.path()).unwrap();
    let config = Config {
        alphabet: vec!['a', 's'],
        patterns: vec![PatternDefinition {
            name: "never".into(),
            regex: "ZZZ".into(),
        }],
        ..Config::default()
    };

    let LaunchOutcome::Opened { run_id } =
        launch(&herdr.host, &context(), &config, &store).unwrap();

    assert!(store.claim(&run_id).unwrap().targets.is_empty());
    assert_eq!(herdr.requests("plugin.pane.open").len(), 1);
    assert!(!herdr.log().contains("notification show"));
}

#[test]
fn invalid_config_notifies_and_fails_before_opening() {
    let herdr = FakeHerdr::new();
    let state = tempdir().unwrap();
    let config = state.path().join("config.toml");
    std::fs::write(&config, "[[patterns]\n").unwrap();

    let error = launch_with_reporting(
        &herdr.host,
        r#"{"focused_pane_id":"w1:p1"}"#,
        &config,
        state.path(),
    )
    .unwrap_err()
    .to_string();

    assert!(
        error.contains(&format!("invalid {}", config.display())),
        "{error}"
    );
    assert!(herdr
        .log()
        .contains("notification show History --body History could not start:"));
    assert!(herdr.requests("plugin.pane.open").is_empty());
}
