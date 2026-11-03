"""End-to-end check of the menu mode: the real laid-out shadowfax.tools plugin in
an isolated Herdr, opened through the `shadowfax.tools.menu` action, with every
kind of menu item driven once.

Usage (from herdr-tools/): python3 crates/menu/tests/real_menu.py

1. The owner's old shadowfax.menu config goes through `herdr-tools migrate-config`
   and must open at its old 80x32. Its rewritten scratch-reap item must reach the
   new action, and its rewritten "create layout…" item must run the merged
   binary from $HERDR_PLUGIN_ROOT.
2. A test config sizes the popup through [popup.size].menu and drives a shell
   item (foreground, then detached behind a confirmation), the retired
   visibility row, copy context, fork, rotate and redistribute. `copy-context
   --stdout` runs from a pane as the CLI.

The retired visibility row now opens Herdr's hide picker. On a Herdr without
`herdr scope` (0.9.5) it reads "(unavailable)" and does nothing, which is what
this test checks there; on a scope-aware Herdr it is a live row, and
real_scope.py drives it.

Everything runs in an IsolatedHerdr (temp XDG dirs, its own session). RTR and
pbcopy are fakes first on PATH, and copy context is only pressed after the
popup's own PATH is shown to resolve the fake pbcopy, so the owner's clipboard
and agents are never touched. Shell items run under fish when it is installed,
the owner's shell, so their commands are tested as they will run.
"""
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import time
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent
TOOLS = HERE.parents[2]
sys.path.insert(0, str(TOOLS / "tests/support"))
from isolated_herdr import IsolatedHerdr  # noqa: E402

LEGACY = HERE / "fixtures/legacy"  # holds the owner's shadowfax.menu/config.toml
SPAWN = re.compile(r'event="pane\.spawn\.start".*\bpane_id=(\d+) rows=(\d+) cols=(\d+)')


def flat(text):
    return re.sub(r"\s", "", text)


def executable(path, body):
    path.write_text(body)
    path.chmod(path.stat().st_mode | stat.S_IXUSR)
    return path


fakes = Path(tempfile.mkdtemp(prefix="menu-e2e-bin-", dir="/tmp")).resolve()
clipboard = fakes / "clipboard.txt"
executable(fakes / "pbcopy", f'#!/bin/sh\ncat > "{clipboard}"\n')
rtr = executable(fakes / "rtr", '#!/bin/sh\nprintf "FORK_STARTED %s\\n" "$*"\nexec sleep 300\n')
# IsolatedHerdr copies os.environ into the client, which starts the server,
# which starts every action and popup.
os.environ["PATH"] = f"{fakes}:{os.environ['PATH']}"
os.environ["RTR_BIN_PATH"] = str(rtr)
os.environ["SHELL"] = shutil.which("fish") or "/bin/sh"


def main():
    with IsolatedHerdr(rows=50, cols=200, prefix="menu-e2e-") as herdr:
        out = herdr.root / "out"
        out.mkdir()

        def spawns():
            log = next(herdr.root.glob("config/herdr/sessions/*/herdr-server.log")).read_text()
            return [tuple(map(int, m.groups())) for m in SPAWN.finditer(log)]

        def open_menu():
            """Invokes the real action from the focused pane; returns the popup's
            PTY (rows, cols) and the menu as first drawn."""
            before = len(spawns())
            herdr.drain(0)
            invoked = herdr.cli("plugin", "action", "invoke", "shadowfax.tools.menu")
            assert invoked["result"]["action"]["command"] == ["./bin/herdr-tools", "menu", "open"]
            # IsolatedHerdr.wait_for discards what it drains; keep the first frame.
            screen, deadline = "", time.monotonic() + 5
            while not spawns()[before:]:
                assert time.monotonic() < deadline, "the menu popup did not open"
                screen += herdr.drain(0.05)
            _, rows, cols = spawns()[before]
            screen += herdr.drain(1.0)
            return (rows, cols), screen

        def screen_until(found, timeout=15, what="text"):
            """Client output, kept, until `found(flat output)` holds."""
            screen, deadline = "", time.monotonic() + timeout
            while not found(flat(screen)):
                assert time.monotonic() < deadline, f"timed out waiting for {what}: {screen[-2000:]}"
                screen += herdr.drain(0.1)
            return screen

        def menu_closed():
            result = subprocess.run(["pgrep", "-f", f"{plugin_dir}/bin/herdr-tools menu menu"],
                                    capture_output=True, text=True)
            return result.returncode != 0

        def layout(pane):
            return herdr.cli("pane", "layout", "--pane", pane)["result"]["layout"]

        def read(pane):
            return herdr.cli("pane", "read", pane, "--source", "recent-unwrapped", "--lines", "80")

        plugin_dir = herdr.install()
        tools = plugin_dir / "bin/herdr-tools"
        assert Path(herdr.plugin()["plugin_root"]).resolve() == plugin_dir.resolve()
        source = herdr.cli("pane", "list")["result"]["panes"][0]["pane_id"]
        area = layout(source)["area"]
        print(f"PASS install + link {plugin_dir}; source {source}, pane area "
              f"{area['width']}x{area['height']}, shell items under {os.environ['SHELL']}")
        scoped = isinstance(herdr.cli("scope", "get", "--json", check=False), dict)
        print(f"INFO this Herdr {'has' if scoped else 'lacks'} `herdr scope`: the v row is "
              f"{'live' if scoped else 'unavailable'}")

        # --- 1. the owner's migrated config ---------------------------------
        legacy = herdr.root / "legacy"
        shutil.copytree(LEGACY, legacy)
        migrated = herdr.run([str(tools), "migrate-config", "--legacy-root", str(legacy)],
                             {"HERDR_PLUGIN_CONFIG_DIR": str(herdr.plugin_config_dir)})
        assert migrated.returncode == 0, migrated.stderr
        config = (herdr.plugin_config_dir / "config.toml").read_text()
        assert "menu = { width = 80, height = 32 }" in config or (
            "[popup.size.menu]" in config and "width = 80" in config), config
        assert "invoke shadowfax.tools.scratch-reap" in config, config
        assert '"$HERDR_PLUGIN_ROOT/bin/herdr-tools" layout grid-dialog' in config, config
        print("PASS migrate-config: owner menu -> [menu] + popup.size.menu 80x32, "
              "scratch-reap and herdr-layouts binary rewritten")

        size, screen = open_menu()
        assert size == (30, 77), size  # 80x32 outer, minus border and scrollbar
        assert "commands" in screen and "excludedworkspaces" in flat(screen), screen
        if scoped:
            assert "excludedworkspaces(unavailable)" not in flat(screen), screen
            print(f"PASS owner menu opens at 80x32 (PTY {size[1]}x{size[0]}); "
                  "'v excluded workspaces' is a live hide row")
        else:
            assert "excludedworkspaces(unavailable)" in flat(screen), screen
            assert "Focusandhideunavailable" in flat(screen), screen
            print(f"PASS owner menu opens at 80x32 (PTY {size[1]}x{size[0]}); "
                  "'v excluded workspaces (unavailable)' with the no-scope panel")

        herdr.type("x")
        screen_until(lambda text: "ReapstaleHerdrScratchsessions?" in text, what="x's confirmation")
        herdr.type("y")
        reaps = herdr.wait_for(
            lambda: [e for e in herdr.plugin_log()["result"]["logs"]
                     if e.get("action_id") == "scratch-reap"],
            timeout=15, what="the scratch-reap action")
        herdr.wait_for(menu_closed, what="the menu to close after x")
        print(f"PASS x (confirm, y) ran `plugin action invoke shadowfax.tools.scratch-reap`: "
              f"action status={reaps[-1]['status']} stderr={reaps[-1].get('stderr', '').strip()!r}")
        time.sleep(0.5)

        open_menu()
        herdr.type("g")
        stub_failed = "createlayout…:commandexitstatus:1"
        dialog = ("Createlayout", "Columnsxrows")  # the layout mode's dialog chrome
        screen = screen_until(lambda text: stub_failed in text or any(d in text for d in dialog),
                              what="the merged binary's layout grid-dialog")
        if stub_failed in flat(screen):
            # On a branch where `layout` is still a stub the dialog exits 1 (a
            # wrong path would be fish's 127). Its stderr is redrawn over before
            # the client sees it, so show the same command line's own error.
            command = next(item["command"] for item in tomllib.loads(config)["menu"]["items"]
                           if item.get("key") == "g")
            direct = herdr.run([os.environ["SHELL"], "-lc", command],
                               {"HERDR_PLUGIN_ROOT": str(plugin_dir),
                                "HERDR_MENU_SOURCE_PANE_ID": source})
            assert direct.returncode == 1, (command, direct)
            assert "herdr-tools layout: layout is not ported yet" in direct.stderr, direct
            outcome = ("herdr-tools layout grid-dialog ran: exit 1, "
                       f"{direct.stderr.strip()!r} (layout is a stub on this branch)")
        else:
            outcome = "herdr-tools layout grid-dialog opened the Create layout dialog"
        herdr.type("\x1b")
        time.sleep(0.4)
        if not menu_closed():
            herdr.type("\x1b")
        herdr.wait_for(menu_closed, what="the menu to close after g")
        print(f"PASS g ran exec \"$HERDR_PLUGIN_ROOT/bin/herdr-tools\" under fish: {outcome}")
        time.sleep(0.5)

        # --- 2. a test config: popup size and every item kind ---------------
        env_file, detached_file = out / "foreground.txt", out / "detached.txt"
        herdr.write_plugin_config(f'''[popup.size]
menu = {{ width = 70, height = 22 }}

[menu]
title = "e2e menu"
max_height = 30

[[menu.items]]
key = "f"
label = "foreground shell"
command = 'printf "%s\\n" "$HERDR_PLUGIN_ROOT" "$HERDR_MENU_SOURCE_PANE_ID" "$PWD" > {env_file}'

[[menu.items]]
key = "z"
label = "detached shell"
command = 'printf done > {detached_file}'
mode = "detached"
confirm = "Run the detached item?"

[[menu.items]]
type = "separator"

[[menu.items]]
type = "fork"
key = "l"
label = "fork left"
placement = "left"

[[menu.items]]
type = "rotate_fork"
key = "o"
label = "rotate last fork"

[[menu.items]]
type = "redistribute"
key = "d"
label = "redistribute panes…"

[[menu.items]]
type = "copy_context"
key = "h"
label = "copy pane context"

[[menu.items]]
type = "workspace_visibility"
key = "v"
label = "excluded workspaces"
''')
        source_cwd = herdr.cli("pane", "get", source)["result"]["pane"]["cwd"]

        size, screen = open_menu()
        assert size == (20, 67), size
        assert "e2emenu" in flat(screen), screen
        print(f"PASS [popup.size].menu = 70x22 opens the popup at PTY {size[1]}x{size[0]}")
        herdr.type("f")
        lines = herdr.wait_for(lambda: env_file.exists() and env_file.read_text().splitlines(),
                               timeout=15, what="the foreground item")
        herdr.wait_for(menu_closed, what="the menu to close after f")
        assert Path(lines[0]).resolve() == plugin_dir.resolve(), lines
        assert lines[1] == source, lines
        assert Path(lines[2]).resolve() == Path(source_cwd).resolve(), lines
        print(f"PASS f (foreground shell) ran in the source cwd with HERDR_PLUGIN_ROOT={lines[0]} "
              f"and HERDR_MENU_SOURCE_PANE_ID={lines[1]}")
        time.sleep(0.5)

        open_menu()
        herdr.type("z")
        screen_until(lambda text: "Runthedetacheditem?" in text, what="z's confirmation")
        herdr.type("y")
        herdr.wait_for(lambda: detached_file.exists() and detached_file.read_text() == "done",
                       timeout=15, what="the detached item")
        herdr.wait_for(menu_closed, what="the menu to close after z")
        print("PASS z (detached shell behind confirm) ran after y")
        time.sleep(0.5)

        _, screen = open_menu()
        if not scoped:
            assert "excludedworkspaces(unavailable)" in flat(screen), screen
            herdr.type("v")
            time.sleep(0.6)
            assert not menu_closed(), "v must do nothing"
        # The popup's own PATH must pick the fake pbcopy before `h` may run.
        pid = subprocess.run(["pgrep", "-f", f"{plugin_dir}/bin/herdr-tools menu menu"],
                             capture_output=True, text=True).stdout.split()[0]
        environment = subprocess.run(["ps", "eww", "-o", "command=", "-p", pid],
                                     capture_output=True, text=True).stdout
        found = re.search(r"(?:^| )PATH=(\S+)", environment)
        assert found, environment
        popup_path = found.group(1)
        assert shutil.which("pbcopy", path=popup_path) == str(fakes / "pbcopy"), popup_path
        herdr.type("\x1b")
        herdr.wait_for(menu_closed, what="the menu to close after Esc")
        print(f"PASS {'v left to real_scope.py' if scoped else 'v (no scope) does nothing'}; "
              "Esc closes; the popup's PATH resolves the fake pbcopy")
        time.sleep(0.5)

        open_menu()
        herdr.type("h")
        copied = herdr.wait_for(lambda: clipboard.exists() and clipboard.read_text(),
                                timeout=15, what="the copied context")
        herdr.wait_for(menu_closed, what="the menu to close after h")
        assert copied.startswith("# Herdr pane context"), copied[:200]
        assert f'"pane_id": "{source}"' in copied, copied
        print(f"PASS h (copy context) copied {len(copied)} bytes naming {source}")
        time.sleep(0.5)

        open_menu()
        herdr.type("l")
        def forked():
            panes = layout(source)["panes"]
            new = [p for p in panes if p["pane_id"] != source]
            return new and "FORK_STARTED fork --here" in read(new[0]["pane_id"]) and new[0]["pane_id"]
        fork = herdr.wait_for(forked, timeout=20, what="the fork pane")
        herdr.wait_for(menu_closed, what="the menu to close after l")
        rects = {p["pane_id"]: p["rect"] for p in layout(source)["panes"]}
        assert rects[fork]["x"] < rects[source]["x"], rects
        assert layout(source)["focused_pane_id"] == fork
        state = herdr.root / "state/herdr/plugins/shadowfax.tools/menu/last-fork.json"
        record = json.loads(state.read_text())
        assert record["source_pane_id"] == source and record["fork_pane_id"] == fork, record
        print(f"PASS l (fork left) created {fork} left of {source}, ran `rtr fork --here`, "
              f"recorded it in {state.relative_to(herdr.root)}")
        time.sleep(0.5)

        open_menu()  # from the focused fork pane
        herdr.type("o")
        herdr.wait_for(
            lambda: (lambda r: r[fork]["x"] > r[source]["x"])(
                {p["pane_id"]: p["rect"] for p in layout(source)["panes"]}),
            timeout=15, what="the rotated fork")
        herdr.wait_for(menu_closed, what="the menu to close after o")
        print(f"PASS o (rotate) swapped {fork} to the right of {source}")
        time.sleep(0.5)

        open_menu()
        herdr.type("d")
        # The client redraws only changed cells, so match a line drawn whole.
        screen_until(lambda text: "choosethenumberofrows" in text, what="the arrangement dialog")
        herdr.type("g")
        herdr.drain(0.4)
        herdr.type("2")
        herdr.drain(0.4)
        herdr.type("\r")
        herdr.wait_for(lambda: len({p["rect"]["y"] for p in layout(source)["panes"]}) == 2,
                       timeout=20, what="two rows")
        herdr.wait_for(menu_closed, what="the menu to close after d")
        after = layout(source)
        assert {p["pane_id"] for p in after["panes"]} == {source, fork}, after
        assert "FORK_STARTED" in read(fork)
        print(f"PASS d (redistribute, grid 2 rows) stacked {source} and {fork} in two rows; "
              "the fork's process kept running")

        stdout_file = out / "copy-context.txt"
        herdr.cli("pane", "run", source, f"'{tools}' menu copy-context --stdout > '{stdout_file}'")
        text = herdr.wait_for(lambda: stdout_file.exists() and stdout_file.read_text(),
                              timeout=15, what="copy-context --stdout")
        assert text.startswith("# Herdr pane context") and f'"pane_id": "{source}"' in text
        print(f"PASS `herdr-tools menu copy-context --stdout` from {source} printed its context")

        failed = [e for e in herdr.plugin_log()["result"]["logs"]
                  if e.get("action_id") == "menu" and e.get("status") != "succeeded"]
        assert not failed, failed
        print("PASS every shadowfax.tools.menu invocation succeeded in the plugin log")
    shutil.rmtree(fakes, ignore_errors=True)


if __name__ == "__main__":
    main()
