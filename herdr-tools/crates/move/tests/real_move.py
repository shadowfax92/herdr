"""Real-herdr end-to-end check of the `move` mode (formerly herdr-ferry).

Run from `herdr-tools/` after a release build is possible:

    python3 crates/move/tests/real_move.py

Everything runs in an IsolatedHerdr (temp XDG dirs, its own session, a 50x200
PTY client); nothing touches ~/.config/herdr or the default session. The plugin
is laid out with `just install` into the temp XDG_DATA_HOME and linked there.

Each case invokes the real `shadowfax.tools.move` action on a focused pane,
checks that the picker popup opened at the size the test `config.toml` sets,
drives the picker with keystrokes through the client, and checks that Herdr's
topology changed the way the operation promises. Moves and merges start from a
pane outside the moved tab: Herdr closes a popup once its owner tab disappears,
which cuts short a picker that moves its own tab (ferry behaved the same; only
kills hand off to a detached executor). The operations:

    move pane · move tab · merge workspace · kill pane · kill tab · kill workspace

Popups are absent from `pane list` on Herdr 0.9.5, so the popup size is read
from the server log's `pane.spawn.start` record (the PTY Herdr allocated: outer
size minus 2 rows and 3 columns), as tests/real_scaffold.py does.
"""
import re
import sys
import time
from pathlib import Path

HERDR_TOOLS = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(HERDR_TOOLS / "tests/support"))
from isolated_herdr import IsolatedHerdr  # noqa: E402

SPAWN = re.compile(r'event="pane\.spawn\.start".*\bpane_id=(\d+) rows=(\d+) cols=(\d+)')
# Distinct from the manifest's 76x24, so a pass proves config sized the popup.
POPUP_WIDTH, POPUP_HEIGHT = 70, 22
ENTER, ESC = "\r", "\x1b"


def spawns(herdr):
    log = next(herdr.root.glob("config/herdr/sessions/*/herdr-server.log")).read_text()
    return [tuple(map(int, match.groups())) for match in SPAWN.finditer(log)]


def topology(herdr):
    get = lambda noun: herdr.cli(noun, "list")["result"][f"{noun}s"]  # noqa: E731
    return get("workspace"), get("tab"), get("pane")


def workspace_by_label(herdr, label):
    workspaces, _, _ = topology(herdr)
    found = [w for w in workspaces if w.get("label") == label]
    return found[0] if found else None


def workspace_named(herdr, label) -> dict:
    found = workspace_by_label(herdr, label)
    assert found, f"no workspace labelled {label}"
    return found


def tabs_of(herdr, workspace_id):
    _, tabs, _ = topology(herdr)
    return [t for t in tabs if t["workspace_id"] == workspace_id]


def pane_ids(herdr):
    return {p["pane_id"] for p in topology(herdr)[2]}


def create_workspace(herdr, label, tab_label):
    """A new focused workspace whose first tab is `tab_label`; returns (workspace, pane)."""
    herdr.cli("workspace", "create", "--cwd", "/tmp", "--label", label, "--focus")
    workspace = workspace_named(herdr, label)
    tab = tabs_of(herdr, workspace["workspace_id"])[0]
    herdr.cli("tab", "rename", tab["tab_id"], tab_label)
    pane = next(p for p in topology(herdr)[2] if p["tab_id"] == tab["tab_id"])
    return workspace, pane["pane_id"]


def create_tab(herdr, workspace_id, label):
    herdr.cli("tab", "create", "--workspace", workspace_id, "--cwd", "/tmp",
              "--label", label, "--focus")
    tab = next(t for t in tabs_of(herdr, workspace_id) if t.get("label") == label)
    pane = next(p for p in topology(herdr)[2] if p["tab_id"] == tab["tab_id"])
    return tab["tab_id"], pane["pane_id"]


def focus(herdr, workspace_id, tab_id):
    herdr.cli("workspace", "focus", workspace_id)
    herdr.cli("tab", "focus", tab_id)


def wait_until(predicate, what, timeout=10):
    """Polls `predicate` without draining the client, so its output stays buffered."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.05)
    raise AssertionError(f"timed out waiting for {what}")


def open_picker(herdr, what):
    """Invokes the move action on the focused pane; waits for the popup and its first screen,
    and returns the popup's size as Herdr spawned it."""
    before = len(spawns(herdr))
    herdr.drain(0)
    invoked = herdr.cli("plugin", "action", "invoke", "shadowfax.tools.move")
    assert invoked["result"]["action"]["command"] == ["./bin/herdr-tools", "move", "open"], invoked
    popup = wait_until(lambda: spawns(herdr)[before:], f"the {what} popup")
    _, rows, cols = popup[0]
    expected = (POPUP_HEIGHT - 2, POPUP_WIDTH - 3)
    assert (rows, cols) == expected, (what, (rows, cols), expected)
    # A fresh popup draws its whole first screen; Herdr may skip blank cells, so compare
    # without spaces.
    screen = []
    wait_until(lambda: screen.append(herdr.drain(0)) or
               "WhatshouldMovedo?" in "".join(screen).replace(" ", ""),
               f"the {what} picker's first screen")
    return f"outer {POPUP_WIDTH}x{POPUP_HEIGHT} -> PTY {cols}x{rows}"


def keys(herdr, *sequence):
    """Types each key, giving the picker time to redraw (and reach its next stage) between."""
    for key in sequence:
        herdr.type(key)
        time.sleep(0.3)


def settle(herdr):
    """Lets the finished picker exit and Herdr close its popup before the next case."""
    time.sleep(0.7)
    herdr.drain(0)


def last_action_ok(herdr):
    logs = [e for e in herdr.plugin_log()["result"]["logs"] if e.get("action_id") == "move"]
    entry = logs[-1]
    assert entry["status"] != "failed" and entry.get("exit_code") in (0, None), entry
    return entry


def main():
    with IsolatedHerdr(rows=50, cols=200) as herdr:
        plugin_dir = herdr.install()
        plugin = herdr.plugin()
        assert Path(plugin["plugin_root"]).resolve() == plugin_dir.resolve(), plugin
        herdr.write_plugin_config(
            f"[popup.size]\nmove = {{ width = {POPUP_WIDTH}, height = {POPUP_HEIGHT} }}\n"
        )
        print(f"PASS install + link: {plugin_dir}; config.toml sets move = "
              f"{POPUP_WIDTH}x{POPUP_HEIGHT} (manifest default 76x24)")

        # Topology: src {one: s1 | s2, two: s3 | s4}, dst {target: d1}, merge {m-a, m-b},
        # doomed {d-a: k1 | k2, d-b: k3}.
        src, s1 = create_workspace(herdr, "src", "one")
        herdr.cli("pane", "split", s1, "--direction", "right", "--cwd", "/tmp")
        two, s3 = create_tab(herdr, src["workspace_id"], "two")
        herdr.cli("pane", "split", s3, "--direction", "down", "--cwd", "/tmp")
        dst, d1 = create_workspace(herdr, "dst", "target")
        merge, m1 = create_workspace(herdr, "merge", "m-a")
        create_tab(herdr, merge["workspace_id"], "m-b")
        doomed, k1 = create_workspace(herdr, "doomed", "d-a")
        herdr.cli("pane", "split", k1, "--direction", "right", "--cwd", "/tmp")
        doomed_tab_b, k3 = create_tab(herdr, doomed["workspace_id"], "d-b")

        def focused_workspace():
            return next(w["label"] for w in topology(herdr)[0] if w.get("focused"))

        # 1. Move pane: the focused pane of src/one (one of two) into dst/target. The picker
        # finishes by focusing the destination, after the move.
        one = tabs_of(herdr, src["workspace_id"])[0]
        focus(herdr, src["workspace_id"], one["tab_id"])
        size = open_picker(herdr, "move pane")
        keys(herdr, "p", ENTER, "target", ENTER)
        wait_until(lambda: tabs_of(herdr, dst["workspace_id"])[0]["pane_count"] == 2,
                   "the pane to reach dst/target")
        assert next(t for t in tabs_of(herdr, src["workspace_id"])
                    if t["tab_id"] == one["tab_id"])["pane_count"] == 1
        wait_until(lambda: focused_workspace() == "dst", "the picker to focus dst")
        last_action_ok(herdr)
        print(f"PASS move pane: {size}; src/one 2 -> 1 panes, dst/target 1 -> 2 panes, dst focused")
        settle(herdr)

        # 2. Move tab: src/two (two panes split down), picked by search from src/one, into dst.
        focus(herdr, src["workspace_id"], one["tab_id"])
        size = open_picker(herdr, "move tab")
        keys(herdr, "t", "two", ENTER, "dst", ENTER)
        wait_until(lambda: any(t.get("label") == "two" for t in tabs_of(herdr, dst["workspace_id"])),
                   "tab two to reach dst")
        moved = next(t for t in tabs_of(herdr, dst["workspace_id"]) if t.get("label") == "two")
        wait_until(lambda: next(t for t in tabs_of(herdr, dst["workspace_id"])
                                if t["tab_id"] == moved["tab_id"])["pane_count"] == 2,
                   "both of tab two's panes to arrive")
        assert [t["label"] for t in tabs_of(herdr, src["workspace_id"])] == ["one"]
        wait_until(lambda: focused_workspace() == "dst", "the picker to focus dst")
        last_action_ok(herdr)
        print(f"PASS move tab: {size}; tab “two” (2 panes) moved src -> dst, dst focused")
        settle(herdr)

        # 3. Merge workspace: merge, picked by search from dst/target, into dst, tabs in order.
        focus(herdr, dst["workspace_id"], tabs_of(herdr, dst["workspace_id"])[0]["tab_id"])
        size = open_picker(herdr, "merge workspace")
        keys(herdr, "w", "merge", ENTER, "dst", ENTER)
        wait_until(lambda: workspace_by_label(herdr, "merge") is None,
                   "workspace merge to disappear")
        labels = [t["label"] for t in tabs_of(herdr, dst["workspace_id"])]
        assert labels[-2:] == ["m-a", "m-b"], labels
        last_action_ok(herdr)
        print(f"PASS merge workspace: {size}; “merge” gone, dst tabs now {labels}")
        settle(herdr)

        # 4. Kill pane: the focused pane of doomed/d-a (one of two), the popup's own pane.
        d_a = tabs_of(herdr, doomed["workspace_id"])[0]
        focus(herdr, doomed["workspace_id"], d_a["tab_id"])
        before = pane_ids(herdr)
        size = open_picker(herdr, "kill pane")
        keys(herdr, "P", ENTER)
        keys(herdr, "y")
        wait_until(lambda: len(before - pane_ids(herdr)) == 1, "one pane to be killed")
        assert next(t for t in tabs_of(herdr, doomed["workspace_id"])
                    if t["tab_id"] == d_a["tab_id"])["pane_count"] == 1
        last_action_ok(herdr)
        print(f"PASS kill pane: {size}; killed {sorted(before - pane_ids(herdr))}, doomed/d-a 2 -> 1 panes")
        settle(herdr)

        # 5. Kill tab: doomed/d-b, the current tab.
        focus(herdr, doomed["workspace_id"], doomed_tab_b)
        size = open_picker(herdr, "kill tab")
        keys(herdr, "T", ENTER)
        keys(herdr, "y")
        wait_until(lambda: [t["label"] for t in tabs_of(herdr, doomed["workspace_id"])] == ["d-a"],
                   "tab d-b to be killed")
        assert k3 not in pane_ids(herdr)
        last_action_ok(herdr)
        print(f"PASS kill tab: {size}; tab “d-b” killed, doomed keeps [d-a]")
        settle(herdr)

        # 6. Kill workspace: doomed, the popup's own workspace (the detached executor's case).
        focus(herdr, doomed["workspace_id"], d_a["tab_id"])
        size = open_picker(herdr, "kill workspace")
        keys(herdr, "W", ENTER)
        keys(herdr, "y")
        wait_until(lambda: workspace_by_label(herdr, "doomed") is None,
                   "workspace doomed to be killed")
        workspace_named(herdr, "dst"), workspace_named(herdr, "src")
        last_action_ok(herdr)
        print(f"PASS kill workspace: {size}; “doomed” killed with its own popup, src and dst intact")
        settle(herdr)


if __name__ == "__main__":
    main()
