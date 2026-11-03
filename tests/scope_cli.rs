//! `herdr scope` against a real, isolated server: focus and hide reach the
//! server through the CLI, and `scope.get` reports the server's verdict for
//! every local tab.
//!
//! The server runs with its own temporary `XDG_*` dirs and an explicit socket
//! path, and every inherited Herdr variable is cleared, so neither the server
//! nor any CLI call here can reach the owner's live session.

#![cfg(unix)]

pub mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use support::{cleanup_test_base, register_runtime_dir, register_spawned_herdr_pid};

const INHERITED_HERDR_VARS: &[&str] = &[
    "HERDR_SOCKET_PATH",
    "HERDR_CLIENT_SOCKET_PATH",
    "HERDR_CONFIG_PATH",
    "HERDR_ENV",
    "HERDR_SESSION",
    "HERDR_STARTUP_CWD",
    "HERDR_PANE_ID",
    "HERDR_WORKSPACE_ID",
    "HERDR_TAB_ID",
];

struct IsolatedServer {
    base: PathBuf,
    socket: PathBuf,
    child: Option<Child>,
}

impl IsolatedServer {
    fn start(config: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        // Short: macOS caps Unix socket paths near 104 bytes.
        let base = PathBuf::from(format!("/tmp/hscope-{}-{nanos}", std::process::id()));
        let app_dir = if cfg!(debug_assertions) {
            "herdr-dev"
        } else {
            "herdr"
        };
        let config_dir = base.join("config").join(app_dir);
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(config_dir.join("config.toml"), config).unwrap();
        let runtime = base.join("run");
        fs::create_dir_all(&runtime).unwrap();
        register_runtime_dir(&runtime);
        let mut server = Self {
            socket: base.join("api.sock"),
            base,
            child: None,
        };
        let child = server
            .command()
            .arg("server")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        register_spawned_herdr_pid(Some(child.id()));
        server.child = Some(child);
        server.wait_until_ready();
        server
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
        for var in INHERITED_HERDR_VARS {
            command.env_remove(var);
        }
        for (var, dir) in [
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_RUNTIME_DIR", "run"),
        ] {
            command.env(var, self.base.join(dir));
        }
        command
            .env("HERDR_SOCKET_PATH", &self.socket)
            .env("SHELL", "/bin/sh")
            .current_dir(&self.base);
        command
    }

    fn wait_until_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if std::os::unix::net::UnixStream::connect(&self.socket).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!(
            "isolated server socket never appeared at {}",
            self.socket.display()
        );
    }

    fn herdr(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    /// Runs a command that must succeed and returns its stdout.
    fn ok(&self, args: &[&str]) -> String {
        let output = self.herdr(args);
        assert!(
            output.status.success(),
            "herdr {args:?} failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let stdout = self.ok(args);
        serde_json::from_str(stdout.trim())
            .unwrap_or_else(|err| panic!("herdr {args:?} printed non-JSON ({err}): {stdout}"))
    }

    fn create_workspace(&self, label: &str) -> String {
        let response = self.json(&["workspace", "create", "--label", label]);
        response["result"]["workspace"]["workspace_id"]
            .as_str()
            .unwrap_or_else(|| panic!("workspace create response: {response}"))
            .to_owned()
    }

    /// `workspace_id -> [admitted]` for every local tab, from `scope get --json`.
    fn verdicts(&self) -> (serde_json::Value, Vec<(String, bool)>) {
        let response = self.json(&["scope", "get", "--json"]);
        assert_eq!(response["result"]["type"], "scope", "{response}");
        let tabs = response["result"]["local"]["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tab| {
                (
                    tab["workspace_id"].as_str().unwrap().to_owned(),
                    tab["admitted"].as_bool().unwrap(),
                )
            })
            .collect();
        (response, tabs)
    }
}

impl Drop for IsolatedServer {
    fn drop(&mut self) {
        let _ = self.herdr(&["server", "stop"]);
        if let Some(mut child) = self.child.take() {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            let _ = child.kill();
            let _ = child.wait();
            support::unregister_spawned_herdr_pid(Some(child.id()));
        }
        cleanup_test_base(&self.base);
    }
}

fn admitted_workspaces(tabs: &[(String, bool)]) -> Vec<&str> {
    let mut admitted: Vec<&str> = tabs
        .iter()
        .filter(|(_, admitted)| *admitted)
        .map(|(workspace, _)| workspace.as_str())
        .collect();
    admitted.dedup();
    admitted
}

fn assert_path_is_private(path: &Path) {
    // Guard against a misconfigured run talking to a real session.
    assert!(
        path.to_string_lossy().starts_with("/tmp/hscope-"),
        "{}",
        path.display()
    );
}

#[test]
fn scope_cli_focuses_hides_and_reports_server_verdicts() {
    let server = IsolatedServer::start(
        "onboarding = false\n[ui.sidebar.agents]\nhidden_workspaces = [\"ft\"]\n",
    );
    assert_path_is_private(&server.socket);
    let alpha = server.create_workspace("alpha");
    let beta = server.create_workspace("beta");
    let ft = server.create_workspace("ft");

    // Boot seeded the config hide: every tab but ft's is admitted.
    let (response, tabs) = server.verdicts();
    assert_eq!(
        response["result"]["scope"]["hidden"],
        serde_json::json!([{"machine": "*", "workspace_label": "ft", "config": true}])
    );
    let shown = admitted_workspaces(&tabs);
    assert!(shown.contains(&alpha.as_str()) && shown.contains(&beta.as_str()));
    assert!(!shown.contains(&ft.as_str()), "{tabs:?}");

    // Focus one workspace: only its tabs stay admitted, and the server pinned
    // the label to the workspace's id.
    let human = server.ok(&["scope", "focus", "alpha"]);
    assert!(human.starts_with("focus: on\n  local:alpha\n"), "{human}");
    let (response, tabs) = server.verdicts();
    assert_eq!(admitted_workspaces(&tabs), [alpha.as_str()], "{tabs:?}");
    assert_eq!(
        response["result"]["scope"]["focus"]["targets"][0]["workspace_id"],
        alpha.as_str()
    );

    // Toggle keeps the targets and switches focus off; the config hide applies.
    server.ok(&["scope", "toggle"]);
    let (response, tabs) = server.verdicts();
    assert_eq!(response["result"]["scope"]["focus"]["active"], false);
    let shown = admitted_workspaces(&tabs);
    assert!(shown.contains(&beta.as_str()) && !shown.contains(&ft.as_str()));

    // Un-hide the config entry by the form `scope get` shows, hide beta.
    server.ok(&["scope", "unhide", "*:ft"]);
    server.ok(&["scope", "hide", "beta"]);
    let (_, tabs) = server.verdicts();
    let shown = admitted_workspaces(&tabs);
    assert!(
        shown.contains(&ft.as_str()) && !shown.contains(&beta.as_str()),
        "{tabs:?}"
    );

    // Clear forgets focus and runtime hides and restores the config hide.
    let human = server.ok(&["scope", "clear"]);
    assert!(human.contains("hidden:\n  *:ft (config)\n"), "{human}");
    let (response, tabs) = server.verdicts();
    assert_eq!(
        response["result"]["scope"]["focus"]["targets"],
        serde_json::json!([])
    );
    let shown = admitted_workspaces(&tabs);
    assert!(shown.contains(&beta.as_str()) && !shown.contains(&ft.as_str()));

    // Nothing is attached to open a picker.
    let output = server.herdr(&["scope", "pick", "hide"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("no_client"));
}
