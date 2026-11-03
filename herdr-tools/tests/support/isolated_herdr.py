"""A throwaway Herdr for real-herdr tests of herdr-tools.

Every instance owns a temp root under /tmp holding its XDG config, state, data
and runtime dirs, a named session, and a PTY-attached client of a fixed size.
Subprocesses get `env`, which strips every inherited HERDR_* variable (a test
started inside Herdr would otherwise inherit the owner's live socket) and points
XDG_* and HERDR_SESSION at the instance. Startup refuses to continue unless the
server's socket lives inside the temp root, so nothing here can reach the
owner's default session or ~/.config/herdr.

    import sys; sys.path.insert(0, "tests/support")
    from isolated_herdr import IsolatedHerdr

    with IsolatedHerdr(rows=50, cols=200) as herdr:
        pane = herdr.cli("pane", "list")["result"]["panes"][0]["pane_id"]
        herdr.link(herdr.install())          # lay out + link shadowfax.tools
        herdr.cli("plugin", "action", "invoke", "shadowfax.tools.move")
        herdr.plugin_config_dir              # .../plugins/config/shadowfax.tools

The client terminal is rows x cols; Herdr's pane area is that minus its sidebar.
"""
import fcntl
import json
import os
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import tempfile
import termios
import threading
import time
from pathlib import Path

HERDR_TOOLS = Path(__file__).resolve().parents[2]
ANSI = re.compile(rb"\x1b\[[0-?]*[ -/]*[@-~]")


class IsolatedHerdr:
    def __init__(self, rows=50, cols=200, herdr=None, prefix="herdr-tools-real-"):
        self.rows, self.cols = rows, cols
        found = herdr or shutil.which("herdr")
        assert found, "herdr must be on PATH"
        self.herdr: str = found
        self.prefix = prefix
        self.session = f"{prefix}{os.getpid()}"
        self.pid = self.fd = None
        self._output = bytearray()
        self._lock = threading.Lock()
        self._reader = None

    # --- lifecycle -------------------------------------------------------

    def __enter__(self):
        self.root = Path(tempfile.mkdtemp(prefix=self.prefix, dir="/tmp")).resolve()
        config_home = self.root / "config"
        herdr_config = config_home / "herdr" / "config.toml"
        herdr_config.parent.mkdir(parents=True)
        herdr_config.write_text('onboarding = false\n[terminal]\ndefault_shell = "/bin/sh"\n')
        env = {k: v for k, v in os.environ.items() if not k.startswith("HERDR_")}
        env.update(
            XDG_CONFIG_HOME=str(config_home),
            XDG_STATE_HOME=str(self.root / "state"),
            XDG_DATA_HOME=str(self.root / "data"),
            XDG_RUNTIME_DIR=str(self.root / "run"),
            HERDR_CONFIG_PATH=str(herdr_config),
            HERDR_SESSION=self.session,
            TERM="xterm-256color",
        )
        self.env = env
        self.plugin_config_dir = config_home / "herdr/plugins/config/shadowfax.tools"
        self.plugin_dir = self.root / "data/herdr/plugins/shadowfax.tools"
        try:
            self._spawn_client()
            self.socket = self._wait_for_server()
            # The one guard that keeps tests off the owner's live session.
            assert Path(self.socket).resolve().is_relative_to(self.root), (
                f"refusing to use non-test socket {self.socket}"
            )
            self.env["HERDR_SOCKET_PATH"] = self.socket
        except BaseException:
            self.__exit__(None, None, None)
            raise
        return self

    def __exit__(self, *_exc):
        try:
            if getattr(self, "env", None):
                subprocess.run([self.herdr, "server", "stop"], env=self.env,
                               capture_output=True, timeout=10)
        finally:
            # Close the master before reaping: on macOS an exiting client blocks
            # closing its tty while unread output is queued, even after SIGKILL.
            if self.fd is not None:
                os.close(self.fd)
                self.fd = None
            if self._reader:
                self._reader.join(timeout=2)
            if self.pid:
                _reap(self.pid)
            if os.environ.get("KEEP_HERDR_ROOT"):
                print(f"kept {self.root}")
            else:
                shutil.rmtree(self.root, ignore_errors=True)
        return False

    def _spawn_client(self):
        argv = [self.herdr, "--session", self.session]
        pid, fd = pty.fork()
        if pid == 0:
            os.execve(argv[0], argv, self.env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", self.rows, self.cols, 0, 0))
        self.pid, self.fd = pid, fd
        # Drain the client for its whole life: a full PTY would block the client
        # (and its tty close at exit) while a long step such as `install` runs.
        self._reader = threading.Thread(target=self._read_client, daemon=True)
        self._reader.start()

    def _read_client(self):
        while True:
            fd = self.fd
            if fd is None:
                return
            try:
                ready, _, _ = select.select([fd], [], [], 0.1)
                if not ready:
                    continue
                chunk = os.read(fd, 262144)
            except (OSError, ValueError):
                return
            if not chunk:
                return
            if b"\x1b[6n" in chunk:  # answer cursor-position queries
                try:
                    os.write(fd, b"\x1b[1;1R")
                except OSError:
                    return
            with self._lock:
                self._output += chunk
                del self._output[:-1_000_000]

    def _wait_for_server(self, timeout=10):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.drain(0.2)
            status = self.cli("status", "--json", check=False)
            server = status.get("server", {}) if isinstance(status, dict) else {}
            if server.get("running") and self._has_pane():
                return server["socket"]
        raise AssertionError("isolated herdr did not start")

    def _has_pane(self):
        panes = self.cli("pane", "list", check=False)
        return isinstance(panes, dict) and bool(panes.get("result", {}).get("panes"))

    # --- driving it ------------------------------------------------------

    def cli(self, *args, check=True, timeout=15):
        """Runs `herdr <args>` against this instance; JSON stdout is decoded."""
        result = subprocess.run([self.herdr, *args], env=self.env, capture_output=True,
                                text=True, timeout=timeout)
        if check:
            assert result.returncode == 0, f"herdr {' '.join(args)}: {result.stderr or result.stdout}"
        out = result.stdout.strip()
        if out.startswith("{"):
            try:
                return json.loads(out)
            except json.JSONDecodeError:
                pass
        return result if not check else out

    def run(self, argv, extra_env=None, timeout=60, **kwargs):
        """Runs any command with this instance's env (plus `extra_env`)."""
        return subprocess.run(argv, env=dict(self.env, **(extra_env or {})),
                              capture_output=True, text=True, timeout=timeout, **kwargs)

    def drain(self, seconds=0.15):
        """Waits `seconds`, then returns (and forgets) the client output since the
        last call, ANSI stripped. A background thread does the actual reading."""
        time.sleep(seconds)
        with self._lock:
            output = bytes(self._output)
            self._output.clear()
        return ANSI.sub(b"", output).decode("utf-8", errors="replace")

    def type(self, text):
        """Sends keystrokes to the client as if typed."""
        os.write(self.fd, text.encode() if isinstance(text, str) else text)

    def wait_for(self, predicate, timeout=5, interval=0.05, what="condition"):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.drain(interval)
            value = predicate()
            if value:
                return value
        raise AssertionError(f"timed out waiting for {what}")

    # --- the plugin ------------------------------------------------------

    def install(self):
        """`just install` into this instance's XDG_DATA_HOME; returns the plugin dir."""
        result = self.run(["just", "-f", str(HERDR_TOOLS / "justfile"), "install"], timeout=600)
        assert result.returncode == 0, result.stdout + result.stderr
        assert (self.plugin_dir / "bin/herdr-tools").is_file()
        return self.plugin_dir

    def link(self, plugin_dir):
        """Links `plugin_dir` (idempotent) and returns Herdr's plugin record."""
        self.cli("plugin", "link", str(plugin_dir))
        return self.plugin()

    def plugin(self):
        plugins = self.cli("plugin", "list", "--json")["result"]["plugins"]
        return next(p for p in plugins if p["plugin_id"] == "shadowfax.tools")

    def write_plugin_config(self, text):
        self.plugin_config_dir.mkdir(parents=True, exist_ok=True)
        (self.plugin_config_dir / "config.toml").write_text(text)

    def plugin_log(self):
        return self.cli("plugin", "log", "--plugin", "shadowfax.tools", check=False)


def _reap(pid):
    for _ in range(50):
        result, _ = os.waitpid(pid, os.WNOHANG)
        if result:
            return
        time.sleep(0.05)
    # Only this harness's own child, never a discovered or shared Herdr process.
    os.kill(pid, signal.SIGKILL)
    os.waitpid(pid, 0)
