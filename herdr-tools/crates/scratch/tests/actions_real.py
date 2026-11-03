"""End to end: every `scratch` action of the installed shadowfax.tools plugin.

Usage (from herdr-tools/): python3 crates/scratch/tests/actions_real.py

In an IsolatedHerdr (temp XDG dirs, its own session, a 50x200 client) this lays
the plugin out with `just install`, links it, and drives Herdr's real dispatch:

- scratch-nvim / scratch-shell open the `scratch` popup at the size config.toml
  gives popup keys `scratch-nvim` ([popup.size]) and `scratch-shell` (a width
  profile), attach a private tmux session running the scratch, hide on the
  scratch's key, and reattach to the same process on the next toggle;
- pane.closed starts the cleanup worker (Herdr runs [[startup]] only when its
  server starts, not on link), and a later pane.closed leaves it the one owner;
- scratch-reap reaps a closed pane's scratch and keeps the live ones;
- [[startup]] relaunches a stopped worker when a server starts on the session.

Popups are absent from Herdr 0.9.5's pane list, so sizes come from the server
log's `pane.spawn.start` record (outer size minus border and scrollbar). The
private workspace tmux server is prestarted with `-f /dev/null` so the owner's
~/.tmux.conf (and anything it runs) never loads.
"""
import fcntl
import hashlib
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
HERDR_TOOLS = HERE.parents[2]
sys.path.insert(0, str(HERDR_TOOLS / "tests" / "support"))
from isolated_herdr import IsolatedHerdr  # noqa: E402

SPAWN = re.compile(r'event="pane\.spawn\.start".*\bpane_id=(\d+) rows=(\d+) cols=(\d+)')


def main():
    # Herdr hands its environment to plugin processes; a login shell from the
    # owner's SHELL could read or write their shell state, so pin /bin/sh.
    os.environ["SHELL"] = "/bin/sh"
    with IsolatedHerdr(rows=50, cols=200, prefix="sa-") as herdr:
        root = herdr.root
        env = {k: v for k, v in herdr.env.items() if not k.startswith("TMUX")}
        state = root / "state/herdr/plugins/shadowfax.tools/scratch"
        sockets = state / f"tmux-{os.geteuid()}"
        workspace_socket = sockets / "shadowfax-herdr-workspace"
        minimal_socket = sockets / "shadowfax-herdr-scratch"
        source = herdr.cli("pane", "list")["result"]["panes"][0]["pane_id"]
        # A headless server lays panes out at 120x40 until the client's size
        # arrives; profiles must match the real client's width.
        area = herdr.wait_for(
            lambda: (lambda area: area if area["height"] == herdr.rows - 1 else None)(
                herdr.cli("pane", "layout", "--pane", source)["result"]["layout"]["area"]),
            timeout=10, what="the client's pane area")
        client_width = area["x"] + area["width"]
        server_hash = hashlib.sha256(herdr.socket.encode()).hexdigest()[:12]
        session = lambda kind, pane=source: f"hs/{kind}/{server_hash}/{pane.replace(':', '-')}"

        def tmux(socket, *args, check=True):
            result = subprocess.run(["tmux", "-S", str(socket), *args], env=env,
                                    capture_output=True, text=True, timeout=5)
            if check:
                assert result.returncode == 0, (args, result.stderr)
            return result.stdout.strip()

        def spawns():
            log = next(root.glob("config/herdr/sessions/*/herdr-server.log")).read_text()
            return [tuple(map(int, match.groups())) for match in SPAWN.finditer(log)]

        def logs():
            return herdr.cli("plugin", "log", "list", "--plugin", "shadowfax.tools",
                             "--limit", "100")["result"]["logs"]

        def worker_locked():
            lock = state / "cleanup-worker.lock"
            if not lock.exists():
                return False
            with lock.open("a") as f:
                try:
                    fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    return True
                return False

        def server_running():
            status = herdr.cli("status", "--json", check=False)
            return isinstance(status, dict) and status.get("server", {}).get("running")

        def attached(socket, name):
            value = tmux(socket, "display-message", "-p", "-t", "=" + name + ":", "#{session_attached}", check=False)
            return value if value else None

        def plugin_cmd(*args):
            return herdr.run([str(plugin_dir / "bin/herdr-tools"), "scratch", *args],
                             {"HERDR_PLUGIN_STATE_DIR": str(state.parent),
                              "HERDR_PLUGIN_CONFIG_DIR": str(herdr.plugin_config_dir)})

        herdr.write_plugin_config(f"""
[popup.size]
scratch-nvim = {{ width = 60, height = 20 }}

[[popup.profile]]
name = "test-narrow"
max_client_width = {client_width}
size.scratch-shell = {{ width = "50%", height = "40%" }}

[scratch.cleanup]
interval_seconds = 86400

[scratch.scratches.nvim]
command = ["nvim", "--clean"]
tmx_type = "vim"
tmux_mode = "workspace"
tmux_prefix = "ctrl+a"
key = "alt+i"

[scratch.scratches.shell]
shell = true
tmux_mode = "minimal"
key = "alt+0"
""")
        sockets.mkdir(parents=True, mode=0o700)
        tmux(workspace_socket, "-f", "/dev/null", "new-session", "-d", "-s", "fixture", "sleep 300")
        try:
            plugin_dir = herdr.install()
            print(f"PASS install + link: {plugin_dir} ({herdr.plugin()['plugin_root']})", flush=True)
            # Linking runs no [[startup]] hook, so the first pane.closed starts the
            # worker (interval 86400 s: after one empty sweep it only waits).
            assert not worker_locked()
            throwaway = herdr.cli("pane", "split", source, "--direction", "down", "--no-focus")["result"]["pane"]["pane_id"]
            seen = {log["log_id"] for log in logs()}
            herdr.cli("pane", "close", throwaway)
            event = herdr.wait_for(lambda: next((log for log in logs() if log["log_id"] not in seen
                                                 and log.get("status") != "running"), None),
                                   timeout=10, what="the first pane.closed hook")
            assert event["status"] == "succeeded", event
            herdr.wait_for(worker_locked, timeout=10, what="the cleanup worker")
            print(f"PASS pane.closed cleanup-start: status={event['status']} exit={event.get('exit_code')}; "
                  f"worker holds {state / 'cleanup-worker.lock'}", flush=True)

            shown = herdr.run([str(plugin_dir / "bin/herdr-tools"), "scratch", "config"],
                              {"HERDR_PLUGIN_CONFIG_DIR": str(herdr.plugin_config_dir)})
            assert shown.returncode == 0, shown.stderr
            print("scratch config:\n  " + shown.stdout.strip().replace("\n", "\n  "), flush=True)

            cases = [
                # action, kind, socket, hide key, expected outer (w, h)
                ("scratch-nvim", "nvim", workspace_socket, "\x1bi", (60, 20)),
                ("scratch-shell", "shell", minimal_socket, "\x1b0",
                 (area["width"] * 50 // 100, area["height"] * 40 // 100)),
            ]
            for action, kind, socket, hide, (width, height) in cases:
                before = len(spawns())
                herdr.cli("plugin", "action", "invoke", f"shadowfax.tools.{action}")
                popup = herdr.wait_for(lambda: spawns()[before:], timeout=10, what=f"the {kind} popup")[0]
                assert popup[1:] == (height - 2, width - 3), (kind, popup, (width, height))
                name = session(kind)
                herdr.wait_for(lambda: attached(socket, name) == "1", timeout=10, what=f"{name} attached")
                command = tmux(socket, "display-message", "-p", "-t", "=" + name + ":", "#{pane_current_command}")
                pid = tmux(socket, "display-message", "-p", "-t", "=" + name + ":", "#{pane_pid}")
                env_pane = tmux(socket, "show-environment", "-t", "=" + name, "HERDR_SCRATCH_SOURCE_PANE")
                assert env_pane == f"HERDR_SCRATCH_SOURCE_PANE={source}", env_pane
                # The scratch's key detaches its tmux client, which ends run-popup
                # and closes the popup; the session (and its process) stays.
                herdr.drain(0.5)
                herdr.type(hide)
                herdr.wait_for(lambda: attached(socket, name) == "0", timeout=10, what=f"{name} hidden")
                before = len(spawns())
                herdr.cli("plugin", "action", "invoke", f"shadowfax.tools.{action}")
                herdr.wait_for(lambda: spawns()[before:], timeout=10, what=f"the {kind} popup again")
                herdr.wait_for(lambda: attached(socket, name) == "1", timeout=10, what=f"{name} reattached")
                assert tmux(socket, "display-message", "-p", "-t", "=" + name + ":", "#{pane_pid}") == pid
                herdr.drain(0.5)
                herdr.type(hide)
                herdr.wait_for(lambda: attached(socket, name) == "0", timeout=10, what=f"{name} hidden again")
                print(f"PASS {action}: popup outer {width}x{height} -> Herdr PTY {popup[2]}x{popup[1]}; "
                      f"{name} runs {command}; hide key detaches; toggle reattaches pid {pid}", flush=True)

            failed = [log for log in logs() if log.get("status") == "failed"]
            assert not failed, failed

            # scratch-reap: a scratch whose source pane closed is reclaimed; the
            # live pane's scratches survive. pane.closed only ensures the worker.
            closed = herdr.cli("pane", "split", source, "--direction", "right", "--no-focus")["result"]["pane"]["pane_id"]
            stale = session("shell", closed)
            tmux(workspace_socket, "new-session", "-d", "-s", stale, "sleep 300")
            tmux(workspace_socket, "set-option", "-t", stale, "@herdr_source_pane", closed)
            tmux(workspace_socket, "set-option", "-t", stale, "@herdr_env_version", "1")
            tmux(workspace_socket, "set-environment", "-t", stale, "HERDR_SOCKET_PATH", herdr.socket)
            seen = {log["log_id"] for log in logs()}
            herdr.cli("pane", "close", closed)
            event = herdr.wait_for(lambda: next((log for log in logs() if log["log_id"] not in seen
                                                 and log.get("status") != "running"), None),
                                   timeout=10, what="the pane.closed hook")
            assert event["status"] == "succeeded", event
            assert worker_locked()
            assert stale in tmux(workspace_socket, "list-sessions", "-F", "#{session_name}").splitlines()
            print(f"PASS pane.closed cleanup-start: status={event['status']}; worker still the one owner", flush=True)

            seen = {log["log_id"] for log in logs()}
            herdr.cli("plugin", "action", "invoke", "shadowfax.tools.scratch-reap")
            reap = herdr.wait_for(lambda: next((log for log in logs() if log["log_id"] not in seen
                                                and log.get("action_id") == "scratch-reap"
                                                and log.get("status") != "running"), None),
                                  timeout=15, what="the scratch-reap action")
            assert reap["status"] == "succeeded", reap
            report = json.loads(reap["stdout"])
            assert report["removed"] == 1 and not report["errors"], report
            names = tmux(workspace_socket, "list-sessions", "-F", "#{session_name}").splitlines()
            assert stale not in names and session("nvim") in names and "fixture" in names, names
            assert attached(minimal_socket, session("shell")) == "0"
            print(f"PASS scratch-reap: removed={report['removed']} of {report['owned_sessions']} owned; "
                  f"live nvim/shell scratches and the foreign session kept", flush=True)

            # [[startup]]: stop the worker and the server, then start a headless
            # server on the same isolated session; its startup hook relaunches it.
            assert plugin_cmd("cleanup-stop").returncode == 0
            herdr.wait_for(lambda: not worker_locked(), timeout=10, what="the worker to stop")
            herdr.cli("server", "stop")
            herdr.wait_for(lambda: not server_running(), timeout=10, what="the server to stop")
            with (root / "restarted-server.log").open("w") as log:
                restarted = subprocess.Popen([herdr.herdr, "server"], env=herdr.env, stdout=log, stderr=log)
            try:
                herdr.wait_for(server_running, timeout=10, what="the restarted server")
                herdr.wait_for(worker_locked, timeout=10, what="the [[startup]] hook's worker")
                startup = herdr.wait_for(lambda: [log for log in logs() if "cleanup-start" in json.dumps(log)
                                                  and log.get("status") != "running"],
                                         timeout=10, what="the startup hook's log")[-1]
                assert startup["status"] == "succeeded", startup
                print(f"PASS [[startup]] cleanup-start: status={startup['status']}; the restarted server "
                      f"relaunched the stopped worker", flush=True)
                assert plugin_cmd("cleanup-stop").returncode == 0
                herdr.wait_for(lambda: not worker_locked(), timeout=10, what="the worker to stop")
                herdr.cli("server", "stop")
            finally:
                restarted.wait(timeout=10)
        finally:
            stopped = plugin_cmd("cleanup-stop") if (state / "cleanup-worker.lock").exists() else None
            if stopped is not None:
                deadline = time.monotonic() + 5
                while worker_locked() and time.monotonic() < deadline:
                    time.sleep(0.05)
                assert not worker_locked(), "cleanup worker did not stop"
            for socket in (workspace_socket, minimal_socket):
                tmux(socket, "kill-server", check=False)


if __name__ == "__main__":
    main()
