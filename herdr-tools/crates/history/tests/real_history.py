"""Real-herdr check of the `history` mode, end to end, in an IsolatedHerdr.

    python3 herdr-tools/crates/history/tests/real_history.py   # from the fork root

It lays the plugin out with `just install` (a release build) into the temp root.

Talon had no real-herdr tests; this covers what its Rust tests fake:

1. the real `herdr-tools history` CLI shape (talon's scaffold tests ran its binary);
2. `migrate-config` over a copy of the owner's talon config;
3. `shadowfax.tools.history` opens the picker popup at the `[popup.size]` size,
   a hint copies its target and closes the popup, and the handoff is gone;
4. a `[[popup.profile]]` size, a second open while that popup is up (refused
   with `ui_busy`, the open popup kept), and `q` closing it.

Everything runs against a temp XDG root and its own session (see
tests/support/isolated_herdr.py). `pbcopy` is a fake on PATH, so the owner's
clipboard is never written. Popup sizes come from the server log's
`pane.spawn.start` record: the PTY is the outer size minus 2 rows and 3 columns.
"""
import os
import re
import shutil
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
HERDR_TOOLS = HERE.parents[2]
sys.path.insert(0, str(HERDR_TOOLS / "tests/support"))
from isolated_herdr import IsolatedHerdr  # noqa: E402

FIXTURES = HERE / "fixtures"
SPAWN = re.compile(r'event="pane\.spawn\.start".*\bpane_id=(\d+) rows=(\d+) cols=(\d+)')
TITLE = "History · focused pane history"


def spawned_popups(herdr):
    """(rows, cols) of every pane Herdr spawned after the first (the startup shell)."""
    log = next(herdr.root.glob("config/herdr/sessions/*/herdr-server.log")).read_text()
    spawns = [tuple(map(int, match.groups())) for match in SPAWN.finditer(log)]
    return [(rows, cols) for pane_id, rows, cols in spawns if pane_id > 1]


class Screen:
    """Client output accumulated across waits. IsolatedHerdr.wait_for drains and
    discards output, and the picker draws once on open, so every wait here
    keeps what it drained."""

    def __init__(self, herdr):
        self.herdr, self.text = herdr, ""

    def wait(self, predicate, what, timeout=5):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.text += self.herdr.drain(0.1)
            value = predicate()
            if value:
                return value
        raise AssertionError(f"timed out waiting for {what}; screen tail: {self.text[-300:]!r}")

    def shows(self, needle):
        return self.wait(lambda: needle in self.text, repr(needle))

    def clear(self):
        self.text = ""
        self.herdr.drain(0.2)


def history_logs(herdr):
    logs = herdr.plugin_log().get("result", {}).get("logs", [])
    return [entry for entry in logs if entry.get("action_id") == "history"]


def open_history(herdr, screen):
    """Invokes the action and returns the new popup's PTY (rows, cols)."""
    before = len(spawned_popups(herdr))
    runs = len(history_logs(herdr))
    invoked = herdr.cli("plugin", "action", "invoke", "shadowfax.tools.history")
    assert invoked["result"]["action"]["command"] == ["./bin/herdr-tools", "history", "open"]
    entry = screen.wait(lambda: history_logs(herdr)[runs:], "the history action's log")[-1]
    assert entry["status"] != "failed", entry
    popups = screen.wait(lambda: spawned_popups(herdr)[before:], "the history popup")
    return popups[0]


def handoffs(herdr):
    """Files in the one handoff dir, `<plugin state dir>/history/runs`."""
    runs = list(herdr.root.rglob("history/runs"))
    assert [path.relative_to(herdr.root) for path in runs] == [
        Path("state/herdr/plugins/shadowfax.tools/history/runs")
    ], runs
    return list(runs[0].iterdir())


def main():
    fake_bin = Path(tempfile.mkdtemp(prefix="history-fake-bin-", dir="/tmp"))
    clipboard = fake_bin / "clipboard.txt"
    pbcopy = fake_bin / "pbcopy"
    pbcopy.write_text(f'#!/bin/sh\ncat > "{clipboard}"\n')
    pbcopy.chmod(0o755)
    # IsolatedHerdr copies os.environ, and Herdr hands it to plugin processes.
    os.environ["PATH"] = f"{fake_bin}:{os.environ['PATH']}"
    try:
        # The default prefix: a longer one pushes Herdr's socket paths past
        # macOS's 104-byte limit and the server never starts.
        with IsolatedHerdr(rows=50, cols=200) as herdr:
            run(herdr, clipboard)
    finally:
        shutil.rmtree(fake_bin, ignore_errors=True)


def run(herdr, clipboard):
    plugin_dir = herdr.install()
    binary = str(plugin_dir / "bin/herdr-tools")

    # 1. The real binary's CLI shape.
    for argv, succeeds in [([], False), (["unknown"], False), (["launch"], False),
                           (["install-keybindings"], False), (["--help"], True)]:
        result = herdr.run([binary, "history", *argv])
        assert (result.returncode == 0) == succeeds, (argv, result.returncode, result.stderr)
        if argv == ["--help"]:
            assert "open" in result.stdout and "picker" in result.stdout, result.stdout
    print("PASS cli: `history` needs a subcommand; unknown, launch and install-keybindings "
          "are refused; open and picker exist")

    # 2. migrate-config over a copy of the owner's talon config.
    migrated = herdr.run([binary, "migrate-config", "--legacy-root", str(FIXTURES), "--stdout"],
                         {"HERDR_PLUGIN_CONFIG_DIR": str(herdr.root / "migrate-dry-run")})
    assert migrated.returncode == 0, migrated.stderr
    text = migrated.stdout
    for name in ["git-ssh", "user-uuid", "user-sha", "file-line", "hostname"]:
        assert f'name = "{name}"' in text, text
    for name in ["laptop", "partial-ultrawide", "full-ultrawide"]:
        assert f'name = "{name}"' in text, text
    assert "[[history.patterns]]" in text and "[popup.size.history]" in text, text
    print("PASS migrate-config: the owner's 5 patterns -> [[history.patterns]], "
          "talon's built-in sizes -> [popup.size.history] + 3 [[popup.profile]]")

    # 3. Open, copy a hint, close.
    source = herdr.cli("pane", "list")["result"]["panes"][0]["pane_id"]
    area = herdr.cli("pane", "layout", "--pane", source)["result"]["layout"]["area"]
    client_width = area["x"] + area["width"]
    herdr.write_plugin_config(
        "[history]\n"
        "enabled_builtin_patterns = []\n\n"
        "[[history.patterns]]\n"
        'name = "ticket"\n'
        'regex = "TKT-[0-9]+"\n\n'
        "[popup.size]\n"
        "history = { width = 60, height = 20 }\n"
    )
    herdr.cli("pane", "run", source, "printf 'ticket TKT-%s ok\\n' 4242")
    herdr.cli("pane", "wait-output", source, "--match", "TKT-4242 ok")
    screen = Screen(herdr)
    screen.clear()
    rows, cols = open_history(herdr, screen)
    assert (rows, cols) == (20 - 2, 60 - 3), (rows, cols)
    screen.shows(TITLE)
    assert not handoffs(herdr), "the picker claims (removes) its handoff"
    herdr.type("a")  # the one target's hint
    screen.wait(lambda: clipboard.exists(), "the copy")
    assert clipboard.read_text() == "TKT-4242", clipboard.read_text()
    print(f"PASS open + copy: popup {cols}x{rows} PTY from history = 60x20; "
          f"hint 'a' copied {clipboard.read_text()!r} and closed the popup")

    # 4. A profile size, a refused second open, and `q`.
    herdr.write_plugin_config(
        "[popup.size]\n"
        "history = { width = 60, height = 20 }\n\n"
        "[[popup.profile]]\n"
        'name = "test-narrow"\n'
        f"max_client_width = {client_width}\n"
        'size.history = { width = "50%", height = "40%" }\n'
    )
    clipboard.unlink()
    time.sleep(0.3)  # the first popup's exit closes it
    screen.clear()
    rows, cols = open_history(herdr, screen)
    expected = (area["height"] * 40 // 100 - 2, area["width"] * 50 // 100 - 3)
    assert (rows, cols) == expected, ((rows, cols), expected)
    screen.shows(TITLE)

    runs = len(history_logs(herdr))
    herdr.cli("plugin", "action", "invoke", "shadowfax.tools.history")
    refused = screen.wait(lambda: history_logs(herdr)[runs:], "the refused open")[-1]
    assert refused["status"] == "failed" and "ui_busy" in refused["stderr"], refused
    assert not handoffs(herdr), "the refused open removes its handoff"

    herdr.type("q")
    time.sleep(0.5)
    assert not clipboard.exists(), "q copies nothing"
    open_history(herdr, screen)  # the slot is free again: q closed the popup
    herdr.type("q")
    print(f"PASS profile + busy + q: client width {client_width} -> profile test-narrow 50%x40% "
          f"-> PTY {cols}x{rows}; a second open failed with "
          f"{refused['stderr'].strip()!r} and left no handoff; q closed the popup")


if __name__ == "__main__":
    main()
