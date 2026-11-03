"""Real-server regression for row-aware fork keys; uses a fake RTR, not real agents.

Usage: python3 tests/real_fork_rows.py target/release/herdr-menu
Owns an isolated server, popup, tabs and all test processes. Never contacts the
user session. Pane IDs, processes, scrollback and untouched row geometry are checked.
"""
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time

menu = str(Path(sys.argv[1]).resolve())
herdr = shutil.which("herdr")
assert herdr, "Herdr must be installed"
session = f"menu-fork-row-test-{os.getpid()}"


def drain(fd, seconds=0.15):
    deadline = time.monotonic() + seconds
    output = b""
    while time.monotonic() < deadline:
        ready, _, _ = select.select([fd], [], [], max(0, deadline - time.monotonic()))
        if not ready:
            continue
        try:
            chunk = os.read(fd, 262144)
        except OSError:
            break
        if not chunk:
            break
        output += chunk
        if b"\x1b[6n" in chunk:
            os.write(fd, b"\x1b[1;1R")
    return re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", output).decode("utf-8", errors="replace")


def spawn(argv, env, rows, cols):
    pid, fd = pty.fork()
    if pid == 0:
        os.execve(argv[0], argv, env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    return pid, fd


def reap(pid):
    for _ in range(50):
        result, _ = os.waitpid(pid, os.WNOHANG)
        if result:
            return
        time.sleep(0.05)
    # Only this script's own child, never a discovered/shared Herdr process.
    os.kill(pid, signal.SIGKILL)
    os.waitpid(pid, 0)


with tempfile.TemporaryDirectory(prefix="fork-rows-", dir="/tmp") as root:
    env = {key: value for key, value in os.environ.items() if not key.startswith("HERDR_")}
    config = Path(root) / "herdr.toml"
    config.write_text('onboarding = false\n[terminal]\ndefault_shell = "/bin/sh"\n')
    rtr = Path(root) / "rtr"
    rtr.write_text('#!/bin/sh\nprintf "FORK_STARTED %s\\n" "$*"\nexec sleep 300\n')
    rtr.chmod(0o700)
    env.update(XDG_CONFIG_HOME=root + "/config", XDG_STATE_HOME=root + "/state",
               HERDR_CONFIG_PATH=str(config), TERM="xterm-256color", RTR_BIN_PATH=str(rtr))

    def cli(*args, text=False):
        result = subprocess.run([herdr, "--session", session, *args], env=env,
                                capture_output=True, text=True, timeout=10)
        assert result.returncode == 0, result.stderr or result.stdout
        return result.stdout if text or not result.stdout.lstrip().startswith("{") else json.loads(result.stdout)

    def layout(pane):
        return cli("pane", "layout", "--pane", pane)["result"]["layout"]

    def split_even(pane, count, uneven=False):
        if count == 1:
            return [pane]
        middle = (count + 1) // 2
        second = cli("pane", "split", pane, "--direction", "right", "--ratio", str(0.55 if uneven else middle / count),
                     "--cwd", root, "--no-focus")["result"]["pane"]["pane_id"]
        return split_even(pane, middle) + split_even(second, count - middle)

    # The shim fails only commands issued by this test's menu. The harness's
    # inspection/cleanup CLI remains independent, so failures are observable.
    shim = Path(root) / "herdr-shim"
    mode_file = Path(root) / "failure-mode"
    move_file = Path(root) / "move-count"
    failure_file = Path(root) / "failure-hit"
    shim.write_text("#!/usr/bin/env python3\n" + f'''import os, pathlib, subprocess, sys
mode_file = pathlib.Path({str(mode_file)!r})
move_file = pathlib.Path({str(move_file)!r})
failure_file = pathlib.Path({str(failure_file)!r})
mode = mode_file.read_text() if mode_file.exists() else ""
args = sys.argv[1:]
if args[:2] == ["pane", "move"] and mode == "staging":
    count = int(move_file.read_text()) + 1 if move_file.exists() else 1
    move_file.write_text(str(count))
    if count == 6:
        failure_file.touch()
        sys.exit("injected first restoration failure")
if args[:2] == ["pane", "run"] and mode == "run":
    failure_file.touch()
    sys.exit("injected RTR input failure")
result = subprocess.run([{herdr!r}, *args])
if args[:2] == ["pane", "split"] and mode == "state" and result.returncode == 0:
    path = pathlib.Path(os.environ["HERDR_PLUGIN_STATE_DIR"]) / "last-fork.json"
    path.unlink(missing_ok=True)
    path.mkdir()
    failure_file.touch()
sys.exit(result.returncode)
''')
    shim.chmod(0o700)

    def create_rows(key, source_row, row_count, topology):
        created = cli("tab", "create", "--workspace", workspace, "--cwd", root,
                      "--label", f"case-{key}-{source_row}-{row_count}-{topology}", "--no-focus")["result"]
        tab, first = created["tab"]["tab_id"], created["root_pane"]["pane_id"]
        if row_count == 1:
            rows = [split_even(first, 5)]
        elif topology == "columns":
            tops = split_even(first, 5, uneven=True)
            bottoms = [cli("pane", "split", pane, "--direction", "down", "--ratio", "0.4",
                           "--cwd", root, "--no-focus")["result"]["pane"]["pane_id"] for pane in tops]
            rows = [tops, bottoms]
        else:
            second = cli("pane", "split", first, "--direction", "down", "--ratio", "0.4",
                         "--cwd", root, "--no-focus")["result"]["pane"]["pane_id"]
            rows = [split_even(first, 5), split_even(second, 5)]
        source = rows[source_row][2]
        cli("tab", "focus", tab)
        cli("pane", "zoom", "--pane", source, "--off")
        for index, pane in enumerate(sum(rows, [])):
            cli("pane", "run", pane, f"printf 'KEEP_{index}\\n'; sleep 300")
        drain(fd, 0.2)
        return tab, rows, source

    pid, fd = spawn([herdr, "--session", session], env, 60, 240)
    try:
        drain(fd, 2)
        socket = cli("status", "--json")["server"]["socket"]
        assert Path(socket).is_relative_to(root), "refuse to contact a non-test socket"
        workspace = cli("pane", "list")["result"]["panes"][0]["workspace_id"]
        plugin = Path(root) / "plugin"
        plugin.mkdir()
        (plugin / "herdr-plugin.toml").write_text(f'''id = "shadowfax.menu"
name = "Fork row regression"
version = "0.4.0"
description = "Isolated native popup regression"
min_herdr_version = "0.9.0"
platforms = ["macos"]
[[actions]]
id = "open"
title = "Open test menu"
contexts = ["pane"]
command = ["/usr/bin/env", {json.dumps("HERDR_BIN_PATH=" + str(shim))}, {json.dumps(menu)}, "open"]
[[panes]]
id = "menu"
title = "Test menu"
placement = "popup"
width = 80
height = 24
command = ["/usr/bin/env", {json.dumps("HERDR_BIN_PATH=" + str(shim))}, {json.dumps(menu)}, "menu"]
''')
        cli("plugin", "link", str(plugin))
        config_dir = Path(cli("plugin", "config-dir", "shadowfax.menu", text=True).strip())
        config_dir.mkdir(parents=True, exist_ok=True)
        config_dir.joinpath("config.toml").write_text(Path(__file__).resolve().parents[1].joinpath("config.default.toml").read_text())

        cases = [(key, source_row, row_count, "rows") for key, source_row, row_count in
                 [("l", 0, 2), ("r", 0, 2), ("l", 1, 2), ("r", 1, 2), ("b", 0, 2),
                  ("b", 1, 2), ("t", 1, 2), ("t", 0, 2), ("t", 0, 1), ("b", 0, 1)]]
        cases += [(key, 0, 2, "columns") for key in "lrtb"]
        for key, source_row, row_count, topology in cases:
            tab, rows, source = create_rows(key, source_row, row_count, topology)
            panes = sum(rows, [])
            before = layout(source)
            original_rects = {p["pane_id"]: p["rect"] for p in before["panes"]}
            processes = {p: cli("pane", "process-info", "--pane", p)["result"] for p in panes}
            cli("plugin", "action", "invoke", "shadowfax.menu.open")
            output = drain(fd, 0.3)
            os.write(fd, key.encode())
            deadline = time.monotonic() + 15
            fork = None
            while time.monotonic() < deadline:
                output += drain(fd, 0.1)
                after = layout(source)
                new = {p["pane_id"] for p in after["panes"]} - set(panes)
                if len(new) == 1:
                    candidate = next(iter(new))
                    if "FORK_STARTED" in cli("pane", "read", candidate, "--source", "recent-unwrapped", "--lines", "50", text=True):
                        fork = candidate
                        break
            assert fork, f"Fork key {key} failed: " + output[-3000:]
            assert after["focused_pane_id"] == fork
            actual_rows = {}
            for pane in sorted(after["panes"], key=lambda p: (p["rect"]["y"], p["rect"]["x"])):
                actual_rows.setdefault(pane["rect"]["y"], []).append(pane)
            actual_rows = list(actual_rows.values())
            assert len(actual_rows) == 2, after
            if row_count == 2:
                destination = source_row if key in "lr" else 0 if key == "t" else 1
                other = 1 - destination
                assert len(actual_rows[destination]) == 6 and len(actual_rows[other]) == 5, after
                assert [p["pane_id"] for p in actual_rows[other]] == rows[other]
                assert [p["pane_id"] for p in actual_rows[destination] if p["pane_id"] != fork] == rows[destination]
                for p in actual_rows[other]:
                    assert p["rect"] == original_rects[p["pane_id"]], (before, after)
                widths = [p["rect"]["width"] for p in actual_rows[destination]]
                assert max(widths) - min(widths) <= 2, widths
                for p in actual_rows[destination]:
                    assert p["rect"]["height"] == original_rects[rows[destination][0]]["height"]
                if key in "lr":
                    order = [p["pane_id"] for p in actual_rows[destination]]
                    assert order.index(fork) == order.index(source) + (-1 if key == "l" else 1)
            else:
                expected_rows = [[fork], panes] if key == "t" else [panes, [fork]]
                assert [[p["pane_id"] for p in row] for row in actual_rows] == expected_rows
            assert len(after["panes"]) == len(panes) + 1
            for index, pane in enumerate(panes):
                assert cli("pane", "process-info", "--pane", pane)["result"] == processes[pane]
                assert f"KEEP_{index}" in cli("pane", "read", pane, "--source", "recent-unwrapped", "--lines", "500", text=True)
            assert not any("menu-redistribute-" in str(t.get("label", "")) for t in cli("tab", "list")["result"]["tabs"])
            print(f"PASS {topology}: {key} from row {source_row + 1}/{row_count}: {[len(row) for row in actual_rows]}, preserved processes and other row geometry", flush=True)
            cli("tab", "close", tab)
            drain(fd, 0.2)
        for failure, key, row_count, topology in [("run", "r", 2, "rows"), ("state", "l", 2, "rows"),
                                                   ("run", "t", 1, "rows"), ("run", "b", 2, "columns"),
                                                   ("staging", "t", 1, "rows")]:
            tab, rows, source = create_rows(key, 0, row_count, topology)
            panes = sum(rows, [])
            before = {p["pane_id"]: p["rect"] for p in layout(source)["panes"]}
            processes = {p: cli("pane", "process-info", "--pane", p)["result"] for p in panes}
            mode_file.write_text(failure)
            move_file.unlink(missing_ok=True)
            failure_file.unlink(missing_ok=True)
            cli("plugin", "action", "invoke", "shadowfax.menu.open")
            output = drain(fd, 0.3)
            os.write(fd, key.encode())
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                output += drain(fd, 0.1)
                if failure_file.exists():
                    current = layout(source)
                    if failure == "staging" and current["tab_id"] != tab:
                        break
                    if failure != "staging" and {p["pane_id"]: p["rect"] for p in current["panes"]} == before:
                        break
            else:
                raise AssertionError("Expected recovery screen: " + output[-3000:])
            # Let the final frame settle before inspecting the completed rollback.
            output += drain(fd, 0.6)
            if failure == "staging":
                tabs = cli("tab", "list")["result"]["tabs"]
                assert tab in [t["tab_id"] for t in tabs], "original tab was deleted"
                holding = [t for t in tabs if str(t.get("label", "")).startswith("menu-redistribute-")]
                assert len(holding) == 1, tabs
                assert all(layout(p)["tab_id"] == holding[0]["tab_id"] for p in panes)
                assert "Source" in output and "Holding" in output, output[-3000:]
            else:
                after = {p["pane_id"]: p["rect"] for p in layout(source)["panes"]}
                assert before == after, (failure, topology, before, after, output[-3000:])
                assert layout(source)["tab_id"] == tab
            for pane in panes:
                assert cli("pane", "process-info", "--pane", pane)["result"] == processes[pane]
            mode_file.unlink()
            for path in Path(root).rglob("last-fork.json"):
                if path.is_dir():
                    path.rmdir()
            os.write(fd, b"\x1bq")
            drain(fd, 0.2)
            cli("tab", "close", tab)
            for holding in cli("tab", "list")["result"]["tabs"]:
                if str(holding.get("label", "")).startswith("menu-redistribute-"):
                    cli("tab", "close", holding["tab_id"])
            print(f"PASS {failure} failure ({key}, {row_count} rows, {topology}): original processes and recovery geometry retained", flush=True)
    finally:
        try:
            subprocess.run([herdr, "--session", session, "server", "stop"], env=env,
                           capture_output=True, timeout=10)
        finally:
            os.close(fd)
            reap(pid)
