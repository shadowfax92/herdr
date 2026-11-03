"""Exercise Menu confirmation -> real Herdr dispatch -> Scratch's owned sweep.

Usage (from herdr-tools/):
    python3 crates/scratch/tests/reap_menu_real.py bin/herdr-tools [MENU_ARGV...]

MENU_ARGV runs the menu TUI, reading `[[items]]` from
`$HERDR_PLUGIN_CONFIG_DIR/config.toml`; it defaults to the old herdr-menu binary
(`.../my/herdr-menu/target/release/herdr-menu menu`), since the menu mode is
ported separately. Ported from herdr-scratch's tests/reap_menu_real.py: the
menu item now invokes `shadowfax.tools.scratch-reap`.

Owns an IsolatedHerdr (temp XDG dirs, its own session), PTYs, tmux servers, and
all test sessions. The production scratch-reap action is copied without
lifecycle hooks so the background worker cannot remove candidates before the
menu action gets to them.
"""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import select
import shutil
import signal
import struct
import subprocess
import sys
import termios
import time
import tomllib

HERE = Path(__file__).resolve().parent
HERDR_TOOLS = HERE.parents[2]
sys.path.insert(0, str(HERDR_TOOLS / "tests" / "support"))
from isolated_herdr import IsolatedHerdr  # noqa: E402

OLD_MENU = Path("/Users/shadowfax/code/side-projects/herdr-custom-plugins/my/herdr-menu/target/release/herdr-menu")
scratch = str(Path(sys.argv[1]).resolve())
menu_argv = sys.argv[2:] or [str(OLD_MENU), "menu"]
menu_argv[0] = str(Path(menu_argv[0]).resolve())
assert Path(menu_argv[0]).is_file(), f"menu binary missing: {menu_argv[0]}"
assert shutil.which("tmux")
action = next(a for a in tomllib.loads((HERDR_TOOLS / "herdr-plugin.toml").read_text())["actions"] if a["id"] == "scratch-reap")
DEFAULT_CONFIG = (HERE.parent / "config.default.toml").read_text()
menu_item = '''[[items]]
key = "x"
label = "reap scratch sessions"
command = '"$HERDR_BIN_PATH" plugin action invoke shadowfax.tools.scratch-reap'
confirm = "Reap stale Herdr Scratch sessions?"
'''


def wait_for(predicate, label, seconds=10):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(.05)
    raise AssertionError("Timed out: " + label)


def main():
    with IsolatedHerdr(prefix="sr-") as herdr:
        root = herdr.root
        env = {k: v for k, v in herdr.env.items() if not k.startswith("TMUX")}
        env.update(TERM="xterm-256color", SHELL="/bin/sh")
        socket = herdr.socket
        sockets = []
        clients = []

        def cli(*args):
            return herdr.cli(*args, timeout=5)

        def tmux(sock, *args, check=True):
            result = subprocess.run(["tmux", "-S", str(sock), "-f", "/dev/null", *args],
                                    env=env, capture_output=True, text=True, timeout=5)
            if check:
                assert result.returncode == 0, result.stderr
            return result.stdout.strip()

        def create(sock, kind, pane):
            if sock not in sockets:
                sock.parent.mkdir(parents=True, mode=0o700, exist_ok=True)
                sockets.append(sock)
                tmux(sock, "new-session", "-d", "-s", "foreign", "sleep 300")
            name = f"hs/{kind}/{hashlib.sha256(socket.encode()).hexdigest()[:12]}/{pane.replace(':', '-')}"
            tmux(sock, "new-session", "-d", "-s", name, "sleep 300")
            tmux(sock, "set-option", "-t", name, "@herdr_source_pane", pane)
            tmux(sock, "set-option", "-t", name, "@herdr_env_version", "1")
            tmux(sock, "set-environment", "-t", name, "HERDR_SOCKET_PATH", socket)
            return sock, name

        def exists(pair):
            sock, name = pair
            return name in tmux(sock, "list-sessions", "-F", "#{session_name}").splitlines()

        def logs():
            return cli("plugin", "log", "list", "--plugin", "shadowfax.tools", "--limit", "50")["result"]["logs"]

        def run_menu(approve=True):
            # Deliberately give Menu its own plugin directories. Herdr dispatch must
            # replace them with Scratch's, rather than scanning Menu's decoy server.
            menu_env = env | dict(HERDR_BIN_PATH=herdr.herdr, HERDR_SOCKET_PATH=socket,
                                  HERDR_PLUGIN_CONFIG_DIR=str(menu_config), HERDR_PLUGIN_STATE_DIR=str(menu_state),
                                  HERDR_MENU_SOURCE_CWD=str(root), HERDR_MENU_SOURCE_PANE_ID=live)
            before = {log["log_id"] for log in logs()}
            pid, fd = pty.fork()
            if pid == 0:
                fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 110, 0, 0))
                os.execve(menu_argv[0], menu_argv, menu_env)
            output = bytearray()
            exited = False

            def read():
                if select.select([fd], [], [], .05)[0]:
                    try:
                        chunk = os.read(fd, 65536)
                        output.extend(chunk)
                        if b"\x1b[6n" in chunk:
                            os.write(fd, b"\x1b[1;1R")
                    except OSError:
                        pass

            def has_text(text):
                read()
                return text.encode() in output

            def done():
                result = os.waitpid(pid, os.WNOHANG)
                read()
                return result

            try:
                wait_for(lambda: has_text("reap scratch sessions"), "menu renders")
                os.write(fd, b"x")
                wait_for(lambda: has_text("Reap stale Herdr Scratch"), "confirmation renders")
                os.write(fd, b"y" if approve else b"n\x1b")
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline:
                    child, status = done()
                    if child:
                        exited = True
                        assert os.waitstatus_to_exitcode(status) == 0, output.decode(errors="replace")
                        break
                assert exited, output.decode(errors="replace")
            finally:
                if not exited:
                    os.kill(pid, signal.SIGKILL)
                    os.waitpid(pid, 0)
                os.close(fd)
            if not approve:
                assert {log["log_id"] for log in logs()} == before, "cancel dispatched cleanup"
                return None
            return wait_for(lambda: next((log for log in logs() if log["log_id"] not in before
                                          and log["status"] != "running"), None), "reap action finishes")

        try:
            assert Path(socket).is_relative_to(root)
            live = cli("workspace", "create", "--cwd", str(root), "--no-focus")["result"]["root_pane"]["pane_id"]
            closed = cli("pane", "split", live, "--direction", "right", "--no-focus")["result"]["pane"]["pane_id"]
            plugin = root / "plugin"
            plugin.mkdir()
            command = json.dumps([scratch, *action["command"][1:]])
            (plugin / "herdr-plugin.toml").write_text(f'''id = "shadowfax.tools"
name = "Isolated Scratch reap test"
version = "0.1.0"
description = "Tests cross-plugin cleanup routing"
min_herdr_version = "0.9.0"
[[actions]]
id = "scratch-reap"
title = "Reap scratch sessions"
command = {command}
''')
            cli("plugin", "link", str(plugin))
            config = Path(cli("plugin", "config-dir", "shadowfax.tools")) / "config.toml"
            assert config.is_relative_to(root), config
            config.parent.mkdir(parents=True, exist_ok=True)
            config.write_text(DEFAULT_CONFIG)
            state = root / "state/herdr/plugins/shadowfax.tools/scratch"
            menu_config, menu_state = root / "menu-config", root / "menu-state"
            menu_config.mkdir(); menu_state.mkdir()
            (menu_config / "config.toml").write_text(menu_item)
            minimal = state / f"tmux-{os.geteuid()}/shadowfax-herdr-scratch"
            workspace = state / f"tmux-{os.geteuid()}/shadowfax-herdr-workspace"
            stale = [create(minimal, "nvim", closed), create(workspace, "nvim", closed), create(workspace, "shell", closed)]
            survivors = [create(workspace, "nvim", live), create(workspace, "shell", live),
                         create(menu_state / f"scratch/tmux-{os.geteuid()}/shadowfax-herdr-workspace", "shell", closed)]
            client = subprocess.Popen(["tmux", "-S", str(workspace), "-C", "attach-session", "-t", survivors[0][1]],
                                      env=env, stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            clients.append(client)
            wait_for(lambda: tmux(workspace, "display-message", "-p", "-t", survivors[0][1], "#{session_attached}") == "1", "live attachment")
            cli("pane", "close", closed)
            run_menu(approve=False)
            assert all(exists(pair) for pair in stale + survivors)
            result = run_menu()
            assert result["status"] == "succeeded", result
            report = json.loads(result["stdout"])
            assert report["removed"] == 3 and report["owned_sessions"] == 5 and not report["errors"], report
            assert not any(exists(pair) for pair in stale)
            assert all(exists(pair) for pair in survivors)
            for sock in sockets:
                tmux(sock, "has-session", "-t", "foreign")
            print("PASS: menu cancellation, confirmed dispatch, both servers reaped; live, attached, foreign and Menu-owned sessions preserved", flush=True)
            config.write_text(config.read_text().replace("enabled = true", "enabled = false"))
            stale = create(workspace, "nvim", closed)
            result = run_menu()
            assert result["status"] == "succeeded" and not json.loads(result["stdout"])["enabled"], result
            assert exists(stale)
            config.write_text("invalid: [")
            result = run_menu()
            assert result["status"] == "failed" and "invalid scratch config" in result["stderr"], result
            assert exists(stale)
            print("PASS: disabled cleanup and invalid config preserve sessions and report the correct action status", flush=True)
        finally:
            for client in clients:
                client.terminate(); client.wait(timeout=5)
            for sock in sockets:
                tmux(sock, "kill-server", check=False)


if __name__ == "__main__":
    main()
