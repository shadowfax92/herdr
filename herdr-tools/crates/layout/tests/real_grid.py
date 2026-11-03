"""Isolated Herdr regression for exact grids and both real popup entrypoints.

Usage, from herdr-tools/:
    python3 crates/layout/tests/real_grid.py [path/to/herdr-menu]

Ported from herdr-layouts' tests/real_grid.py. It lays out and links
shadowfax.tools with `just install` inside an IsolatedHerdr (temp XDG dirs, its
own session, a 60x240 client), so it never contacts the user session. The direct
CLI is `bin/herdr-tools layout grid`, the native popup is the
`shadowfax.tools.layout-grid` action. The optional old herdr-menu binary
exercises a menu's foreground handoff: it is linked as a separate
`shadowfax.menu` plugin whose `g` item runs `herdr-tools layout grid-dialog`,
the item the dropped `install-menu` used to write.
"""
import json
import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "tests/support"))
from isolated_herdr import IsolatedHerdr  # noqa: E402

menu = str(Path(sys.argv[1]).resolve()) if len(sys.argv) > 1 else None


with IsolatedHerdr(rows=60, cols=240, prefix="exact-grid-") as herdr:
    root = str(herdr.root)
    cli = herdr.cli
    plugin_dir = herdr.install()
    binary = str(plugin_dir / "bin/herdr-tools")
    first = cli("pane", "list")["result"]["panes"][0]["pane_id"]
    first_tab = cli("pane", "layout", "--pane", first)["result"]["layout"]["tab_id"]
    cli("pane", "run", first, f"cd {root}; printf 'GRID_KEEP_0\\n'; sleep 300")
    herdr.drain(0.4)
    plugin_env = dict(HERDR_BIN_PATH=herdr.herdr, HERDR_ENV="1", HERDR_PANE_ID=first)

    def layout():
        return cli("pane", "layout", "--pane", first)["result"]["layout"]

    def ordered(value):
        return [p["pane_id"] for p in sorted(value["panes"], key=lambda p: (p["rect"]["y"], p["rect"]["x"]))]

    def grid(spec, ok=True):
        result = herdr.run([binary, "layout", "grid", spec], plugin_env, timeout=30)
        assert (result.returncode == 0) == ok, result.stdout + result.stderr
        return result

    def check(columns, rows, originals, source=first):
        after = layout()
        assert after["tab_id"] == first_tab
        assert len(after["panes"]) == columns * rows, after
        assert ordered(after)[:len(originals)] == originals, after
        assert after["focused_pane_id"] == source, after
        assert not after["zoomed"]
        groups = {}
        for pane in after["panes"]:
            groups.setdefault(pane["rect"]["y"], []).append(pane["rect"])
        assert len(groups) == rows, after
        for group in groups.values():
            assert len(group) == columns, group
            assert max(p["width"] for p in group) - min(p["width"] for p in group) <= 2, group
        assert len(cli("tab", "list")["result"]["tabs"]) == 1
        return after

    original_process = cli("pane", "process-info", "--pane", first)["result"]
    grid("5x2")
    after = check(5, 2, [first])
    assert cli("pane", "process-info", "--pane", first)["result"] == original_process
    panes = ordered(after)
    for index, pane in enumerate(panes[1:], 1):
        info = cli("pane", "get", pane)["result"]["pane"]
        assert Path(info["cwd"]).resolve() == Path(root).resolve(), info
        cli("pane", "run", pane, f"printf 'GRID_KEEP_{index}\\n'; sleep 300")
    herdr.drain(0.4)
    processes = {p: cli("pane", "process-info", "--pane", p)["result"] for p in panes}
    print("PASS 1 → 5x2: nine new shells in the source cwd, original process preserved", flush=True)

    cli("pane", "zoom", "--pane", first, "--on")
    before = layout()
    tabs = cli("tab", "list")
    result = grid("4x2", ok=False)
    assert "More panes already exist" in result.stderr and "10 panes" in result.stderr and "only 8" in result.stderr
    assert layout() == before and cli("tab", "list") == tabs
    grid("100x100", ok=False)
    assert layout() == before
    print("PASS 10 → 8 rejected without changing panes, focus, zoom, or tabs", flush=True)

    grid("5x2")
    check(5, 2, panes)
    grid("6x2")
    check(6, 2, panes)
    for index, pane in enumerate(panes):
        assert cli("pane", "process-info", "--pane", pane)["result"] == processes[pane]
        output = cli("pane", "read", pane, "--source", "recent-unwrapped", "--lines", "500")
        assert f"GRID_KEEP_{index}" in output
    print("PASS 10 → 10 and 10 → 12: same original IDs, processes, scrollback, focus, and reading order", flush=True)

    # The plugin is the laid-out shadowfax.tools dir `install` linked above.
    cli("pane", "zoom", "--pane", panes[3], "--off")
    herdr.drain(0.1)
    cli("plugin", "action", "invoke", "shadowfax.tools.layout-grid")
    native_output = herdr.drain(0.5)
    herdr.type(b"7x2")
    native_output += herdr.drain(0.3)
    if "Createlayout" not in re.sub(r"\s", "", native_output):
        print(cli("plugin", "log", "--plugin", "shadowfax.tools"), flush=True)
        raise AssertionError("Native popup failed to open: " + native_output[-1000:])
    herdr.type(b"\x1bq")
    herdr.drain(0.3)
    check(6, 2, panes, source=panes[3])
    print("PASS native popup preview and cancel leave panes unchanged", flush=True)

    cli("plugin", "action", "invoke", "shadowfax.tools.layout-grid")
    herdr.drain(0.4)
    herdr.type(b"7x2\r")
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        herdr.drain(0.2)
        current = layout()
        if len(current["panes"]) == 14 and len(cli("tab", "list")["result"]["tabs"]) == 1:
            break
    else:
        raise AssertionError("Native grid popup did not finish")
    check(7, 2, panes, source=panes[3])
    print("PASS native popup creates 7x2 grid", flush=True)

    if menu:
        menu_root = Path(root) / "menu-plugin"
        menu_root.mkdir()
        (menu_root / "herdr-plugin.toml").write_text(f'''id = "shadowfax.menu"
name = "Menu regression"
version = "0.3.0"
description = "Isolated menu handoff"
min_herdr_version = "0.9.0"
platforms = ["macos"]
[[actions]]
id = "open"
title = "Open menu"
contexts = ["pane"]
command = [{json.dumps(menu)}, "open"]
[[panes]]
id = "menu"
title = "Menu"
placement = "popup"
command = [{json.dumps(menu)}, "menu"]
''')
        cli("plugin", "link", str(menu_root))
        menu_config = Path(cli("plugin", "config-dir", "shadowfax.menu").strip()) / "config.toml"
        menu_config.parent.mkdir(parents=True, exist_ok=True)
        # What `install-menu` appended, now pointing at `herdr-tools layout`.
        item = f"exec '{binary}' layout grid-dialog --pane \"$HERDR_MENU_SOURCE_PANE_ID\""
        menu_config.write_text(
            'title = "Test"\nwidth = 80\nheight = 24\n'
            '[[items]]\ntype = "redistribute"\nkey = "d"\nlabel = "Redistribute"\n'
            '[[items]]\ntype = "shell"\nkey = "g"\nlabel = "create layout…"\n'
            f"command = {json.dumps(item)}\nmode = \"foreground\"\n"
        )
        cli("pane", "zoom", "--pane", panes[-1], "--off")
        before = layout()
        cli("plugin", "action", "invoke", "shadowfax.menu.open")
        herdr.drain(0.4)
        herdr.type(b"g")
        output = herdr.drain(0.5)
        herdr.type(b"4x2\r")
        output += herdr.drain(0.4)
        assert "Morepanesalreadyexist" in re.sub(r"\s", "", output), output
        assert layout() == before
        herdr.type(b"\x15" + b"5x3\r")  # Ctrl-U replaces the rejected size.
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            output += herdr.drain(0.2)
            if len(layout()["panes"]) == 15 and len(cli("tab", "list")["result"]["tabs"]) == 1:
                break
        else:
            raise AssertionError("Menu grid did not finish: " + output[-1500:])
        check(5, 3, ordered(before), source=panes[-1])
        for pane in panes:
            assert cli("pane", "process-info", "--pane", pane)["result"] == processes[pane]
        print("PASS menu → g: error stays visible, correction applies, original processes survive", flush=True)
