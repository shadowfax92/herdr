"""Opt-in real-server/UI regression; never connects to the user's Herdr session.

Usage (from herdr-tools/): python3 crates/menu/tests/real_redistribution.py target/release/herdr-tools
Owns a named server, attached TUI, popup-sized menu PTY, and long-running test
processes under temporary config/state roots. Tests the menu's actual key path.
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
session = f"menu-layout-test-{os.getpid()}"


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


with tempfile.TemporaryDirectory(prefix="menu-grid-", dir="/tmp") as root:
    env = {key: value for key, value in os.environ.items() if not key.startswith("HERDR_")}
    env.update(XDG_CONFIG_HOME=root + "/config", XDG_STATE_HOME=root + "/state",
               HERDR_CONFIG_PATH=str(Path(__file__).with_name("herdr-repro.toml").resolve()),
               TERM="xterm-256color")

    def cli(*args, text=False):
        result = subprocess.run([herdr, "--session", session, *args], env=env,
                                capture_output=True, text=True, timeout=10)
        assert result.returncode == 0, result.stderr or result.stdout
        return result.stdout if text or not result.stdout.lstrip().startswith("{") else json.loads(result.stdout)

    pid, fd = spawn([herdr, "--session", session], env, 60, 240)
    try:
        drain(fd, 2)
        socket = cli("status", "--json")["server"]["socket"]
        assert Path(socket).is_relative_to(root), "refuse to contact a non-test socket"
        first = cli("pane", "list")["result"]["panes"][0]["pane_id"]
        panes = [first]
        for _ in range(6):
            pane = cli("pane", "split", first, "--direction", "right", "--no-focus")["result"]["pane"]["pane_id"]
            panes.append(pane)
        # Distinct markers and live sleep processes prove this is redistribution,
        # not a replacement layout with newly launched lookalike shells.
        for index, pane in enumerate(panes):
            cli("pane", "run", pane, f"printf 'MENU_KEEP_{index}\\n'; sleep 300")
        drain(fd, 0.5)
        before_processes = {pane: cli("pane", "process-info", "--pane", pane)["result"] for pane in panes}
        config_dir = Path(root) / "menu-config"
        config_dir.mkdir()
        (config_dir / "config.toml").write_text('''[menu]
title = "test menu"
[[menu.items]]
type = "redistribute"
key = "d"
label = "redistribute panes"
''')
        plugin_env = dict(env, HERDR_BIN_PATH=herdr, HERDR_SOCKET_PATH=socket,
                          HERDR_PLUGIN_CONFIG_DIR=str(config_dir),
                          HERDR_PLUGIN_STATE_DIR=root + "/menu-state",
                          HERDR_MENU_SOURCE_CWD=root)

        def invoke(mode, source, rows=None, cancel=False):
            popup_env = dict(plugin_env, HERDR_MENU_SOURCE_PANE_ID=source)
            popup_pid, popup_fd = spawn([menu, "menu", "menu"], popup_env, 20, 80)
            output = ""
            exited = False
            try:
                output += drain(popup_fd, 0.3)
                os.write(popup_fd, b"d")
                output += drain(popup_fd, 0.5)
                assert "Chooseanarrangement" in re.sub(r"\s", "", output), output
                os.write(popup_fd, mode.encode())
                output += drain(popup_fd, 0.2)
                if rows is not None:
                    os.write(popup_fd, str(rows).encode())
                    output += drain(popup_fd, 0.2)
                if cancel:
                    os.write(popup_fd, b"\x1bq")  # Alt+q
                else:
                    os.write(popup_fd, b"\r")
                deadline = time.monotonic() + 20
                while time.monotonic() < deadline:
                    output += drain(popup_fd)
                    drain(fd, 0.02)
                    result, status = os.waitpid(popup_pid, os.WNOHANG)
                    if result:
                        exited = True
                        assert os.waitstatus_to_exitcode(status) == 0, output
                        return
                raise AssertionError("menu did not finish: " + output)
            finally:
                os.close(popup_fd)
                if not exited:
                    reap(popup_pid)

        original = cli("pane", "layout", "--pane", first)["result"]["layout"]
        invoke("g", first, rows=2, cancel=True)
        assert cli("pane", "layout", "--pane", first)["result"]["layout"] == original
        print("PASS cancel preserves the real layout", flush=True)

        for mode, rows, source in [("g", 2, first), ("s", None, panes[-1]), ("g", 3, first)]:
            invoke(mode, source, rows)
            after = cli("pane", "layout", "--pane", first)["result"]["layout"]
            assert {pane["pane_id"] for pane in after["panes"]} == set(panes), after
            groups = {}
            for pane in after["panes"]:
                groups.setdefault(pane["rect"]["y"], []).append(pane["rect"])
            expected = [4, 3] if rows == 2 else [3, 2, 2] if rows == 3 else [7]
            assert [len(group) for _, group in sorted(groups.items())] == expected, after
            assert after["focused_pane_id"] == source, after
            for group in groups.values():
                widths = [rect["width"] for rect in group]
                assert max(widths) - min(widths) <= 2, group
            for index, pane in enumerate(panes):
                process = cli("pane", "process-info", "--pane", pane)["result"]
                assert process == before_processes[pane], (before_processes[pane], process)
                assert f"MENU_KEEP_{index}" in cli("pane", "read", pane, "--source", "recent-unwrapped", "--lines", "500", text=True)
            tabs = cli("tab", "list")["result"]["tabs"]
            assert not any("menu-redistribute-" in str(tab.get("label", "")) for tab in tabs), tabs
            print(f"PASS {mode} rows={rows}: {expected}; same processes, markers, pane IDs, and source focus", flush=True)
        # Exercise Herdr's real session-modal popup too, in the isolated registry.
        plugin_root = Path(root) / "native-plugin"
        plugin_root.mkdir()
        (plugin_root / "herdr-plugin.toml").write_text(f'''id = "shadowfax.tools"
name = "Menu regression"
version = "0.3.0"
description = "Isolated native popup regression"
min_herdr_version = "0.9.0"
platforms = ["macos"]
[[actions]]
id = "menu"
title = "Open test menu"
contexts = ["pane"]
command = [{json.dumps(menu)}, "menu", "open"]
[[panes]]
id = "menu"
title = "Test menu"
placement = "popup"
command = [{json.dumps(menu)}, "menu", "menu"]
''')
        cli("plugin", "link", str(plugin_root))
        native_config = Path(cli("plugin", "config-dir", "shadowfax.tools", text=True).strip())
        native_config.mkdir(parents=True, exist_ok=True)
        (native_config / "config.toml").write_text((config_dir / "config.toml").read_text())
        cli("pane", "zoom", "--pane", panes[-1], "--off")
        cli("plugin", "action", "invoke", "shadowfax.tools.menu")
        native_output = drain(fd, 0.5)
        os.write(fd, b"d")
        native_output += drain(fd, 0.5)
        os.write(fd, b"g")
        native_output += drain(fd, 0.3)
        os.write(fd, b"2\r")
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            native_output += drain(fd, 0.2)
            after = cli("pane", "layout", "--pane", first)["result"]["layout"]
            counts = {}
            for pane in after["panes"]:
                counts[pane["rect"]["y"]] = counts.get(pane["rect"]["y"], 0) + 1
            if [count for _, count in sorted(counts.items())] == [4, 3] and after["focused_pane_id"] == panes[-1]:
                break
        else:
            raise AssertionError("native popup failed to apply layout: " + native_output)
        assert {pane["pane_id"] for pane in after["panes"]} == set(panes)
        for pane in panes:
            assert cli("pane", "process-info", "--pane", pane)["result"] == before_processes[pane]
        print("PASS native Herdr popup: 4+3 layout, same processes, original source focus", flush=True)
    finally:
        try:
            subprocess.run([herdr, "--session", session, "server", "stop"], env=env,
                           capture_output=True, timeout=10)
        finally:
            os.close(fd)
            reap(pid)
