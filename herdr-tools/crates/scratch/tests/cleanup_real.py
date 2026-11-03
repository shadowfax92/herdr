"""Real Herdr/tmux cleanup lifecycle, using only an owned temporary server.

Usage (from herdr-tools/): python3 crates/scratch/tests/cleanup_real.py bin/herdr-tools
TTL's injected-clock real-tmux cases are in cleanup::real_tests.

Ported from herdr-scratch's tests/cleanup_real.py. Herdr is an IsolatedHerdr
(temp XDG dirs, its own session); scratch state, config and the private tmux
server live in its temp root, so nothing reaches the owner's sessions.
"""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parents[2] / "tests" / "support"))
from isolated_herdr import IsolatedHerdr  # noqa: E402

binary = str(Path(sys.argv[1]).resolve())
DEFAULT_CONFIG = (HERE.parent / "config.default.toml").read_text()


def main():
    with IsolatedHerdr(prefix="sc-") as herdr:
        root = herdr.root
        # HERDR_PLUGIN_STATE_DIR is plugin_state; scratch keeps its files in
        # plugin_state/scratch, the mode's own subdir.
        plugin_state, config_dir = root / "s", root / "c"
        state = plugin_state / "scratch"
        state.mkdir(parents=True); config_dir.mkdir()
        config_dir.joinpath("config.toml").write_text(
            DEFAULT_CONFIG.replace("interval_seconds = 60", "interval_seconds = 5"))
        plugin_env = dict(HERDR_PLUGIN_STATE_DIR=str(plugin_state), HERDR_PLUGIN_CONFIG_DIR=str(config_dir))
        env = {k: v for k, v in herdr.env.items() if k not in ("TMUX", "TMUX_PANE")} | plugin_env
        tmux_socket = state / f"tmux-{os.geteuid()}" / "shadowfax-herdr-scratch"
        tmux_socket.parent.mkdir(mode=0o700)
        socket = herdr.socket

        def cli(*args):
            return herdr.cli(*args, timeout=5)

        def tmux(*args):
            result = subprocess.run(["tmux", "-S", str(tmux_socket), "-f", "/dev/null", *args], env=env, text=True, capture_output=True, timeout=5)
            assert result.returncode == 0, result.stderr
            return result.stdout

        def cleanup(*args):
            result = subprocess.run([binary, "scratch", *args], env=env, text=True, capture_output=True, timeout=6)
            assert result.returncode == 0, result.stderr
            return json.loads(result.stdout) if result.stdout.strip() else None

        def create_scratch(pane):
            name = f"hs/shell/{hashlib.sha256(socket.encode()).hexdigest()[:12]}/{pane.replace(':', '-')}"
            tmux("new-session", "-d", "-s", name, "sleep 300")
            tmux("set-option", "-t", name, "@herdr_source_pane", pane)
            tmux("set-option", "-t", name, "@herdr_env_version", "1")
            tmux("set-environment", "-t", name, "HERDR_SOCKET_PATH", socket)
            return name

        def wait_for(predicate, seconds=8):
            end = time.monotonic() + seconds
            while time.monotonic() < end:
                if predicate(): return
                time.sleep(.05)
            raise AssertionError("condition did not become true")

        def server_running():
            status = herdr.cli("status", "--json", check=False)
            return isinstance(status, dict) and status.get("server", {}).get("running")

        def worker_locked():
            with (state / "cleanup-worker.lock").open("a") as f:
                try:
                    fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    return True
                return False

        try:
            assert Path(socket).is_relative_to(root)
            source = cli("workspace", "create", "--cwd", str(root), "--no-focus")["result"]["root_pane"]["pane_id"]
            # Keep the server and private tmux alive after the last tested owner dies.
            keeper = cli("workspace", "create", "--cwd", str(root), "--no-focus")["result"]["root_pane"]["pane_id"]
            tmux("new-session", "-d", "-s", "foreign", "sleep 300")
            name = create_scratch(source)
            assert cleanup("cleanup")["candidates"] == []
            assert not (state / "cleanup-state.json").exists(), "preview wrote owner state"
            assert cleanup("cleanup", "--apply")["removed"] == 0
            moved = cli("pane", "move", source, "--new-workspace", "--no-focus")["result"]["move_result"]["pane"]["pane_id"]
            assert cleanup("cleanup", "--apply")["removed"] == 0
            # Legacy sessions with no cached terminal must follow the inherited alias.
            (state / "cleanup-state.json").unlink()
            assert cleanup("cleanup", "--apply")["removed"] == 0
            cli("pane", "close", moved)
            preview = cleanup("cleanup")
            assert preview["candidates"][0]["reason"] == "source_closed", preview
            report = cleanup("cleanup", "--apply")
            assert report["removed"] == 1, report
            tmux("has-session", "-t", "foreign")
            print("PASS: real source move, legacy alias recovery, closure, preview, foreign preservation", flush=True)

            source = cli("pane", "split", keeper, "--direction", "right", "--cwd", str(root), "--no-focus")["result"]["pane"]["pane_id"]
            create_scratch(source)
            plugin = root / "plugin"
            plugin.mkdir()
            hook = json.dumps(["/usr/bin/env", f"HERDR_PLUGIN_STATE_DIR={plugin_state}", f"HERDR_PLUGIN_CONFIG_DIR={config_dir}", binary, "scratch", "cleanup-start"])
            plugin.joinpath("herdr-plugin.toml").write_text(f'''id = "test.scratch-cleanup"
name = "Isolated cleanup hook test"
version = "0.1.0"
description = "Owns only test processes"
min_herdr_version = "0.9.0"
[[startup]]
command = {hook}
[[events]]
on = "pane.closed"
command = {hook}
''')
            cli("plugin", "link", str(plugin))
            cli("pane", "close", source)
            wait_for(worker_locked)
            cleanup("cleanup-start")  # Idempotent; the lifetime lock remains held.
            wait_for(lambda: "hs/" not in tmux("list-sessions", "-F", "#{session_name}"))
            cli("plugin", "disable", "test.scratch-cleanup")
            # A restart must revoke a stop even while the old process still holds
            # its lifetime lock. No plugin hook is available to mask a lost restart.
            cleanup("cleanup-stop")
            cleanup("cleanup-start")
            time.sleep(.5)
            assert worker_locked(), "rapid stop/start left cleanup stopped"
            cleanup("cleanup-stop")
            wait_for(lambda: not worker_locked())
            print("PASS: real plugin hook, idempotent start, rapid restart, autonomous cleanup and stop", flush=True)

            # Status publication is advisory. Force rename to fail, then prove a
            # later sweep still reclaims a newly closed owner without another start.
            status = state / "cleanup-status.json"
            status.unlink(missing_ok=True)
            status.mkdir()
            cleanup("cleanup-start")
            wait_for(worker_locked)
            wait_for(lambda: "cleanup status:" in (state / "cleanup.log").read_text())
            assert worker_locked(), "status write failure killed cleanup"
            source = cli("pane", "split", keeper, "--direction", "right", "--cwd", str(root), "--no-focus")["result"]["pane"]["pane_id"]
            create_scratch(source)
            cli("pane", "close", source)
            wait_for(lambda: "hs/" not in tmux("list-sessions", "-F", "#{session_name}"))
            status.rmdir()
            wait_for(status.is_file)
            cleanup("cleanup-stop")
            wait_for(lambda: not worker_locked())
            print("PASS: status publication failure preserves future sweeps and recovers", flush=True)

            source = cli("pane", "split", keeper, "--direction", "right", "--cwd", str(root), "--no-focus")["result"]["pane"]["pane_id"]
            create_scratch(source)
            cli("server", "stop")
            wait_for(lambda: not server_running())
            report = cleanup("cleanup", "--apply")
            assert report["removed"] == 0 and report["errors"], report
            print("PASS: disconnected Herdr is preserved as unknown ownership", flush=True)
        finally:
            cleanup("cleanup-stop")
            wait_for(lambda: not worker_locked())
            subprocess.run(["tmux", "-S", str(tmux_socket), "kill-server"], env=env, capture_output=True, timeout=5)


if __name__ == "__main__":
    main()
