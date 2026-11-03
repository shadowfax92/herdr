"""Real-herdr check of the scaffold: install + link, a stub action's error, and
popup sizing through Host::open_popup. Run with `just -f herdr-tools/justfile real`.

Everything runs in an IsolatedHerdr (temp XDG dirs, its own session); nothing
touches ~/.config/herdr or the default session.

Popups are absent from `pane list`, `pane layout` and `api snapshot` on Herdr
0.9.5, so sizes are read from the server log's `pane.spawn.start` record, which
states the PTY Herdr allocated for the popup: the inner rect, i.e. the outer
size minus the 2-cell border and Herdr's 1-column scrollbar.
"""
import json
import re
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE / "support"))
from isolated_herdr import HERDR_TOOLS, IsolatedHerdr  # noqa: E402

OPEN_POPUP = HERDR_TOOLS / "target/debug/examples/open_popup"
SPAWN = re.compile(r'event="pane\.spawn\.start".*\bpane_id=(\d+) rows=(\d+) cols=(\d+)')


def spawned_popups(herdr):
    """(rows, cols) of every pane Herdr spawned after the first (the startup shell)."""
    log = next(herdr.root.glob("config/herdr/sessions/*/herdr-server.log")).read_text()
    spawns = [tuple(map(int, match.groups())) for match in SPAWN.finditer(log)]
    return [(rows, cols) for pane_id, rows, cols in spawns if pane_id > 1]


def inner(outer, available):
    """Herdr's popup geometry: outer size (cells or "N%") to PTY size."""
    if isinstance(outer, str):
        outer = available * int(outer.rstrip("%")) // 100
    return outer


def main():
    assert OPEN_POPUP.is_file(), "build it: cargo build -p tools-core --example open_popup"
    with IsolatedHerdr(rows=50, cols=200) as herdr:
        # 1. Lay out the plugin dir with `just install` and link it.
        plugin_dir = herdr.install()
        for laid_out in ["herdr-plugin.toml", "bin/herdr-tools", "README.md", "LICENSE", "nvim"]:
            assert (plugin_dir / laid_out).exists(), f"install left out {laid_out}"
        assert not list(plugin_dir.parent.glob(".shadowfax.tools.*")), "install left a stage dir"
        plugin = herdr.plugin()
        assert Path(plugin["plugin_root"]).resolve() == plugin_dir.resolve(), plugin["plugin_root"]
        assert plugin["enabled"]
        again = herdr.run(["just", "-f", str(HERDR_TOOLS / "justfile"), "install"], timeout=600)
        assert again.returncode == 0 and "already linked" in again.stdout, again.stdout + again.stderr
        version = herdr.run([str(plugin_dir / "bin/herdr-tools"), "--version"]).stdout.strip()
        print(f"PASS install + link: {plugin_dir} ({version}); a second install keeps the link")

        # 2. A stub action through Herdr: it fails, and the plugin log says why.
        invoked = herdr.cli("plugin", "action", "invoke", "shadowfax.tools.move")
        assert invoked["result"]["action"]["command"] == ["./bin/herdr-tools", "move", "open"]
        logs = herdr.wait_for(
            lambda: [entry for entry in herdr.plugin_log()["result"]["logs"]
                     if entry.get("action_id") == "move"],
            what="the move action's log entry",
        )
        entry = logs[-1]
        assert entry["status"] == "failed" and entry["exit_code"] == 1, entry
        assert entry["stderr"] == "herdr-tools move: move is not ported yet\n", entry
        print(f"PASS stub action: status={entry['status']} exit={entry['exit_code']} "
              f"stderr={entry['stderr'].strip()!r}")

        # 3. Popups through Host::open_popup, sized from a test config.toml.
        source = herdr.cli("pane", "list")["result"]["panes"][0]["pane_id"]
        area = herdr.cli("pane", "layout", "--pane", source)["result"]["layout"]["area"]
        client_width = area["x"] + area["width"]
        herdr.write_plugin_config(
            "[popup.size]\n"
            "move = { width = 60, height = 20 }\n\n"
            "[[popup.profile]]\n"
            'name = "test-narrow"\n'
            f"max_client_width = {client_width}\n"
            'size.history = { width = "50%", height = "40%" }\n'
        )
        plugin_env = {
            "HERDR_PLUGIN_CONFIG_DIR": str(herdr.plugin_config_dir),
            "HERDR_PLUGIN_CONTEXT_JSON": json.dumps({"focused_pane_id": source}),
        }
        cases = [
            # entrypoint, popup key, expected resolution, outer (w, h) Herdr should use
            ("move", "move", {"width": 60, "height": 20, "profile": None}, (60, 20)),
            ("history", "history", {"width": "50%", "height": "40%", "profile": "test-narrow"},
             ("50%", "40%")),
            ("layout-grid", "layout-grid", None, (80, 24)),  # manifest default
        ]
        for entrypoint, key, resolved, (width, height) in cases:
            before = len(spawned_popups(herdr))
            result = herdr.run([str(OPEN_POPUP), entrypoint, key], plugin_env)
            assert result.returncode == 0, result.stderr
            assert json.loads(result.stdout) == resolved, result.stdout
            popups = herdr.wait_for(lambda: spawned_popups(herdr)[before:],
                                    what=f"the {entrypoint} popup")
            rows, cols = popups[0]
            expected = (inner(height, area["height"]) - 2, inner(width, area["width"]) - 3)
            assert (rows, cols) == expected, (entrypoint, (rows, cols), expected)
            print(f"PASS popup {entrypoint}: config {resolved} -> outer {width}x{height} "
                  f"-> Herdr PTY {cols}x{rows}")
            # The stub popup exits at once and Herdr closes it, freeing the slot.
            time.sleep(0.5)


if __name__ == "__main__":
    main()
