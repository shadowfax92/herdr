"""Exercise the actual picker process while read-only host calls are stalled.

Usage (from herdr-tools/): just bin && python3 crates/find/tests/slow_host.py
Runs `herdr-tools find picker` from target/release. Each case owns a fake Unix
host and PTY. No Herdr server is contacted.
"""
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import signal
import socket
import struct
import subprocess
import tempfile
import termios
import threading
import time

binary = Path(__file__).resolve().parents[3] / "target/release/herdr-tools"
assert binary.is_file(), f"build it first: {binary}"
panes = [{"pane_id": f"w1:p{i}", "terminal_id": f"term{i}", "tab_id": "w1:t1",
          "workspace_id": "w1", "label": label} for i, label in [(1, "source alpha"), (2, "target beta")]]
snapshot = {"workspaces":[{"workspace_id":"w1"}], "tabs":[{"tab_id":"w1:t1", "workspace_id":"w1"}], "panes":panes}


def exercise(mode):
    with tempfile.TemporaryDirectory(prefix="jump-slow-", dir="/tmp") as directory:
        path = str(Path(directory) / "s")
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(path)
        listener.listen()
        listener.settimeout(0.05)
        stalled = threading.Event()
        release = threading.Event()
        stop = threading.Event()
        focused = []
        closed = []
        errors = []
        handlers = []

        def handle(connection):
            try:
                with connection:
                    connection.settimeout(5)
                    request = json.loads(connection.makefile("rb").readline())
                    method, params = request["method"], request["params"]
                    if method == "session.snapshot":
                        if mode == "catalog-cancel":
                            stalled.set()
                            release.wait(5)
                        result = {"snapshot":snapshot}
                    elif method in ("pane.get", "pane.focus"):
                        pane = next(p for p in panes if p["pane_id"] == params["pane_id"])
                        if method == "pane.focus":
                            focused.append(pane["pane_id"])
                        result = {"pane":pane}
                    elif method == "pane.close":
                        closed.append(params["pane_id"])
                        result = {}
                    elif method == "tab.focus":
                        result = {"tab":{"tab_id":"w1:t1"}}
                    elif method == "pane.read":
                        stalled.set()
                        release.wait(5)
                        result = {"read":{"pane_id":params["pane_id"], "text":"late preview"}}
                    elif method == "scope.get":
                        # The catalog read asks for focus/hide after the
                        # snapshot. Refuse it as stable Herdr 0.9.5 does, so
                        # Find fails open and lists every pane.
                        connection.sendall(json.dumps({"id": request["id"], "error": {
                            "code": "invalid_request",
                            "message": "invalid request: unknown variant `scope.get`, expected one of `ping`"}}).encode() + b"\n")
                        return
                    else:
                        raise AssertionError(method)
                    connection.sendall(json.dumps({"id":request["id"],"result":result}).encode() + b"\n")
            except (BrokenPipeError, ConnectionResetError):
                pass  # The picker is expected to exit before a stalled read.
            except Exception as error:
                errors.append(str(error))

        def serve():
            while not stop.is_set():
                try:
                    connection, _ = listener.accept()
                except TimeoutError:
                    continue
                thread = threading.Thread(target=handle, args=(connection,))
                handlers.append(thread)
                thread.start()

        server = threading.Thread(target=serve)
        server.start()
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 26, 110, 0, 0))
        env = {k:v for k,v in os.environ.items() if not k.startswith("HERDR_")}
        env.update(HERDR_SOCKET_PATH=path, HERDR_JUMP_SOURCE_PANE="w1:p1", TERM="xterm-256color",
                   HOME=directory, XDG_STATE_HOME=str(Path(directory) / "state"),
                   CODEX_HOME=str(Path(directory) / "codex"), CLAUDE_CONFIG_DIR=str(Path(directory) / "claude"))
        process = subprocess.Popen([str(binary), "find", "picker"], env=env, stdin=slave, stdout=slave, stderr=slave)
        os.close(slave)
        output = bytearray()

        def drain_until(predicate, seconds=2):
            deadline = time.monotonic() + seconds
            while not predicate():
                assert time.monotonic() < deadline, (mode, output[-2000:])
                ready, _, _ = select.select([master], [], [], 0.02)
                if ready:
                    try:
                        data = os.read(master, 262144)
                    except OSError:
                        data = b""
                    output.extend(data)

        try:
            if mode == "catalog-cancel":
                drain_until(stalled.is_set)
                os.write(master, b"typing before loading\x1b")
            elif mode == "confirm-ahead":
                drain_until(lambda: b"source alpha" in output)
                # Stop only this owned picker so every byte is already queued
                # before it sees Enter. More than 64 keys exercises the drain
                # beyond the normal event batch, with a resize queued as well.
                os.kill(process.pid, signal.SIGSTOP)
                os.waitpid(process.pid, os.WUNTRACED)
                try:
                    os.write(master, b"\x18beta\t\r" + b"y" * 128)
                    fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 28, 120, 0, 0))
                    os.kill(process.pid, signal.SIGWINCH)
                finally:
                    os.kill(process.pid, signal.SIGCONT)
                settled = time.monotonic() + 0.2
                drain_until(lambda: closed or time.monotonic() >= settled)
                assert not closed, ("queued y killed before confirmation", closed)
                os.write(master, b"y")
                drain_until(lambda: closed)
                assert closed == ["w1:p2"], closed
                os.write(master, b"\x03")
            else:
                drain_until(lambda: b"source alpha" in output)
                os.write(master, b"\x00")
                drain_until(stalled.is_set)
                os.write(master, b"target beta\r")
            # Keep the host read blocked throughout the exit check. This proves
            # Escape/Enter don't join the worker or wait for its timeout.
            drain_until(lambda: process.poll() is not None, seconds=1)
            assert process.returncode == 0, output[-2000:]
            assert focused == (["w1:p2"] if mode == "preview-jump" else []), focused
            assert not errors, errors
            print(json.dumps({"case":mode, "passed":True, "focus":focused, "closed":closed}))
        finally:
            release.set()
            stop.set()
            server.join(2)
            listener.close()
            for thread in handlers:
                thread.join(2)
            if process.poll() is None:
                process.terminate()
            process.wait(timeout=2)
            os.close(master)


exercise("catalog-cancel")
exercise("preview-jump")

exercise("confirm-ahead")
