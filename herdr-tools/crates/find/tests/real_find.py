"""End to end, each `find` action once in an isolated Herdr: its popup opens at
the size shadowfax.tools' config.toml sets for its key, and Enter does the job.

Usage (from herdr-tools/): python3 crates/find/tests/real_find.py

It runs in tests/support/isolated_herdr.py's IsolatedHerdr (temp XDG dirs, its
own session, a 50x200 client), lays the plugin out with `just install`, which
links it, and binds Alt-p / Alt-n in that Herdr's own config.toml. zoxide reads
a private database and copies go to a file, so the owner's zoxide data and
clipboard are never touched.

Popups are absent from `pane list` on Herdr 0.9.5, so sizes come from the
server log's `pane.spawn.start` record, as in tests/real_scaffold.py: Herdr
spawns a popup's PTY at its outer size less 2 rows and 3 columns.
"""
import json
import re
import socket
import subprocess
import sys
import tempfile
from pathlib import Path

HERDR_TOOLS = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(HERDR_TOOLS / "tests/support"))
from isolated_herdr import IsolatedHerdr  # noqa: E402

SPAWN = re.compile(r'event="pane\.spawn\.start".*\bpane_id=(\d+) rows=(\d+) cols=(\d+)')
CONFIG = """\
[popup.size]
find = { width = 100, height = 30 }
find-paths = { width = 70, height = 20 }

[[popup.profile]]
name = "e2e"
max_client_width = 250
size.find-paths = { width = "50%", height = "40%" }
"""
KEYS = """
[[keys.command]]
key = "alt+p"
type = "plugin_action"
command = "shadowfax.tools.find"
description = "Search all open panes"

[[keys.command]]
key = "alt+n"
type = "plugin_action"
command = "shadowfax.tools.find-paths"
description = "Paste a zoxide path"
"""


class FindHerdr(IsolatedHerdr):
    """IsolatedHerdr strips every HERDR_* variable; the server (and so each
    popup) also needs the private zoxide database and clipboard stand-in."""

    def __init__(self, extra_env, **kwargs):
        super().__init__(**kwargs)
        self.extra_env = extra_env

    def _spawn_client(self):
        self.env.update(self.extra_env)
        super()._spawn_client()


def log_text(herdr):
    return next(herdr.root.glob("config/herdr/sessions/*/herdr-server.log")).read_text()


def popup_spawn(herdr, since):
    """(rows, cols) of the one pane spawned after log offset `since`."""
    def spawned():
        found = [tuple(map(int, m.groups()))[1:] for m in SPAWN.finditer(log_text(herdr)[since:])]
        return found or None
    found = herdr.wait_for(spawned, timeout=5, what="a popup spawn in the server log")
    assert len(found) == 1, found
    return found[0]


def rpc(herdr, method, params):
    """One socket API request to this instance's server (never another)."""
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(5)
        connection.connect(herdr.socket)
        connection.sendall(json.dumps({"id": "find-e2e", "method": method, "params": params}).encode() + b"\n")
        response = json.loads(connection.makefile("rb").readline())
        assert "error" not in response, response
        return response["result"]


def main():
    with tempfile.TemporaryDirectory(prefix="find-e2e-", dir="/tmp") as private:
        private = Path(private).resolve()
        clip = private / "clipboard"
        extra = {"_ZO_DATA_DIR": str(private / "zoxide"),
                 "HERDR_JUMP_COPY_COMMAND": f"cat > '{clip}'"}
        with FindHerdr(extra, rows=50, cols=200) as herdr:
            plugin_dir = herdr.install()
            plugin = herdr.plugin()
            assert Path(plugin["plugin_root"]).resolve() == plugin_dir.resolve() and plugin["enabled"], plugin
            herdr_config = herdr.root / "config/herdr/config.toml"
            herdr_config.write_text(herdr_config.read_text() + KEYS)
            herdr.cli("server", "reload-config")
            herdr.cli("config", "check")
            herdr.write_plugin_config(CONFIG)
            print(f"PASS install + link: {plugin_dir}; Alt-p/Alt-n bound in Herdr's config.toml")

            snapshot = herdr.cli("api", "snapshot")["result"]["snapshot"]
            source = snapshot["panes"][0]["pane_id"]
            source_tab = snapshot["panes"][0]["tab_id"]
            area = herdr.cli("pane", "layout", "--pane", source)["result"]["layout"]["area"]
            client_width = area["x"] + area["width"]

            # 1. find: Alt-p opens the navigator at [popup.size].find; typing a
            #    pane's name and Enter focuses that pane. The target lives in
            #    an `ft` workspace, which Find lists like any other: this
            #    Herdr's config hides nothing (real_scope.py covers hiding).
            target = herdr.cli("workspace", "create", "--label", "ft", "--no-focus")["result"]["root_pane"]["pane_id"]
            herdr.cli("pane", "rename", target, "find-e2e-target")
            herdr.drain(0.5)
            since = len(log_text(herdr))
            herdr.type("\x1bp")
            # The client draws semantic frames, not plain text, so the server
            # log's spawn record is the signal that the popup opened.
            rows, cols = popup_spawn(herdr, since)
            herdr.drain(1.0)  # the picker loads its catalog on a worker
            assert (rows, cols) == (30 - 2, 100 - 3), (rows, cols)
            herdr.type("find-e2e-target")
            herdr.drain(0.5)
            herdr.type("\r")
            focused = herdr.wait_for(
                lambda: herdr.cli("api", "snapshot")["result"]["snapshot"]["focused_pane_id"] == target,
                what="Enter to focus the target pane")
            assert focused
            print(f"PASS find: config 100x30 -> Herdr PTY {cols}x{rows}; Enter focused {target} in workspace ft")
            rpc(herdr, "pane.focus", {"pane_id": source})
            rpc(herdr, "tab.focus", {"tab_id": source_tab})
            herdr.drain(0.5)

            # 2. find-paths: Alt-n opens paths at the e2e profile's 50%x40% (the
            #    client is under 250 cells wide); Enter types the path into the
            #    source pane, shell-quoted, and copies it raw.
            directory = private / "e2e paths" / "find target"
            directory.mkdir(parents=True)
            subprocess.run(["zoxide", "add", str(directory)], env=dict(herdr.env, **extra), check=True)
            since = len(log_text(herdr))
            herdr.type("\x1bn")
            rows, cols = popup_spawn(herdr, since)
            herdr.drain(1.0)  # zoxide runs on the picker's load worker
            assert client_width <= 250, client_width
            assert (rows, cols) == (area["height"] * 40 // 100 - 2, area["width"] * 50 // 100 - 3), (rows, cols, area)
            herdr.type("find target")
            herdr.drain(0.5)
            herdr.type("\r")
            quoted = f"'{directory}'"
            herdr.wait_for(lambda: clip.exists() and clip.read_text() == str(directory),
                           what="the copy to reach the clipboard file")
            typed = herdr.wait_for(
                lambda: quoted in herdr.cli("pane", "read", source, "--source", "visible"),
                what="the quoted path in the source pane")
            assert typed
            print(f"PASS find-paths: profile e2e 50%x40% of {area['width']}x{area['height']} -> "
                  f"Herdr PTY {cols}x{rows}; Enter typed {quoted} and copied it")

            log = herdr.plugin_log()["result"]["logs"]
            failed = [entry for entry in log if entry.get("plugin_id") == "shadowfax.tools"
                      and entry.get("action_id", "").startswith("find") and entry.get("status") != "succeeded"]
            assert not failed, failed


if __name__ == "__main__":
    main()
