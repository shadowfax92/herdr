"""End to end, Find's ctrl-o reach toggle with Herdr's focus on, in an
isolated Herdr: a peek at a pane outside the focus that leaves the focus alone.

Usage (from herdr-tools/):
    HERDR_BIN=/path/to/fork/target/debug/herdr python3 crates/find/tests/real_scope_toggle.py

It needs a Herdr with `herdr scope …` and `scope.get` (the fork's focus and
hide); on one without (stable 0.9.5) it prints a SKIP line and exits 0. HERDR_BIN
picks the binary (default: `herdr` on PATH).

Three workspaces: `alpha` is focused, `home` (the first one) and `beta` are
not. Each root pane carries a blocked agent reported through
`herdr pane report-agent`, a stand-in that needs no real agent process. Then:

1. Alt-p from alpha opens Find on the focus only: the header reads
   `In focus 1  Everything 3`, and beta's pane is not listed.
2. Ctrl-o lists every pane, beta's marked `⊘`; Enter on it focuses beta, the
   client draws beta's pane, and the scope (`herdr scope get`) is unchanged.
   Beta stays focused: nothing bounces it back into the focus.
3. From beta, Herdr's own keys lead back into the focus: Alt-} and Alt-{ (next
   and previous workspace) step to alpha, the nearest focused workspace, and
   Alt-u (next attention agent) lands on alpha's agent, the one in the focus.

Everything runs in tests/support/isolated_herdr.py's IsolatedHerdr (temp XDG
dirs, its own session, a PTY client), so the owner's live Herdr and
~/.config/herdr are never touched.
"""
import json
import os
import re
import socket
import subprocess
import sys
import time
from pathlib import Path

HERDR_TOOLS = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(HERDR_TOOLS / "tests/support"))
from isolated_herdr import IsolatedHerdr  # noqa: E402

KEYS = """
[keys]
previous_workspace = "alt+{"
next_workspace = "alt+}"
next_attention_agent = "alt+u"

[[keys.command]]
key = "alt+p"
type = "plugin_action"
command = "shadowfax.tools.find"
description = "Search all open panes"
"""
CTRL_O = "\x0f"
SHOWN = "FSC_BETA_SHOWN"


def flat(text):
    return re.sub(r"\s", "", text)


def main():
    binary = os.environ.get("HERDR_BIN")
    with IsolatedHerdr(rows=50, cols=200, herdr=binary, prefix="fsc-") as herdr:
        version = herdr.cli("--version", check=False)
        version = getattr(version, "stdout", "").strip() or "unknown version"
        probe = herdr.cli("scope", "get", "--json", check=False)
        if not isinstance(probe, dict):
            print(f"SKIP real_scope_toggle.py: {herdr.herdr} ({version}) has no `herdr scope get`. "
                  f"Run it against a fork build: HERDR_BIN=/path/to/herdr python3 {Path(__file__).name}")
            return

        def scope():
            reply = herdr.cli("scope", "get", "--json")
            return reply.get("result", reply)

        def rpc(method, params):
            """One socket request to this instance's server (never another)."""
            with socket.socket(socket.AF_UNIX) as connection:
                connection.settimeout(5)
                connection.connect(herdr.socket)
                request = {"id": "fsc-e2e", "method": method, "params": params}
                connection.sendall(json.dumps(request).encode() + b"\n")
                response = json.loads(connection.makefile("rb").readline())
            assert "error" not in response, response
            return response["result"]

        def snapshot():
            return herdr.cli("api", "snapshot")["result"]["snapshot"]

        def focused():
            return snapshot()["focused_pane_id"]

        def focused_workspace():
            return snapshot()["focused_workspace_id"]

        def focus(pane):
            tab = herdr.cli("pane", "get", pane)["result"]["pane"]["tab_id"]
            rpc("pane.focus", {"pane_id": pane})
            rpc("tab.focus", {"tab_id": tab})
            herdr.wait_for(lambda: focused() == pane, what=f"{pane} to be focused")
            herdr.drain(0.5)

        def find_open():
            found = subprocess.run(["pgrep", "-f", f"{plugin_dir}/bin/herdr-tools find picker"],
                                   capture_output=True, text=True)
            return found.returncode == 0

        def screen_until(found, timeout=10, what="text"):
            screen, deadline = "", time.monotonic() + timeout
            while not found(flat(screen)):
                assert time.monotonic() < deadline, f"timed out waiting for {what}: {screen[-2500:]}"
                screen += herdr.drain(0.1)
            return screen

        # The just recipe links the plugin with `herdr`; point it at this binary.
        herdr.env["HERDR"] = herdr.herdr
        plugin_dir = herdr.install()
        herdr_config = herdr.root / "config/herdr/config.toml"
        herdr_config.write_text(herdr_config.read_text() + KEYS)
        herdr.cli("server", "reload-config")
        herdr.cli("config", "check")

        home = snapshot()["panes"][0]["pane_id"]
        alpha = herdr.cli("workspace", "create", "--label", "alpha", "--no-focus")["result"]
        beta = herdr.cli("workspace", "create", "--label", "beta", "--no-focus")["result"]
        alpha_ws, alpha_pane = alpha["workspace"]["workspace_id"], alpha["root_pane"]["pane_id"]
        beta_ws, beta_pane = beta["workspace"]["workspace_id"], beta["root_pane"]["pane_id"]
        for pane, name in [(home, "fsc-home"), (alpha_pane, "fsc-alpha"), (beta_pane, "fsc-beta")]:
            herdr.cli("pane", "rename", pane, name)
            herdr.cli("pane", "report-agent", "--source", "fsc-e2e", "--agent", "claude",
                      "--state", "blocked", pane)
        # Text only beta's pane shows, so the client's output proves it drew beta.
        herdr.cli("pane", "run", beta_pane, f"echo {SHOWN}")
        herdr.cli("scope", "focus", "alpha")
        before = scope()
        assert before["scope"]["focus"]["active"], before
        admitted = {tab["workspace_id"]: tab["admitted"] for tab in before["local"]["tabs"]}
        assert admitted[alpha_ws] and not admitted[beta_ws], admitted
        focus(alpha_pane)
        print(f"PASS {herdr.herdr} ({version}): focus on alpha; home {home}, alpha {alpha_pane}, "
              f"beta {beta_pane}, each a blocked agent")

        # --- 1. Find opens on the focus only --------------------------------
        # Read the screen straight away: wait_for drains, and would discard
        # the popup's first frame, the only one that draws its header.
        herdr.drain(0)
        herdr.type("\x1bp")
        screen_until(lambda text: "Infocus1" in text and "Everything3" in text,
                     what="the reach strip")
        assert find_open(), "the reach strip drew, but no Find popup is running"
        herdr.type("fsc-beta")
        screen_until(lambda text: "Nomatchingpanes·ctrl-oeverything" in text,
                     what="beta to be left out of the focus")
        herdr.type("\r")
        herdr.drain(0.5)
        assert find_open() and focused() == alpha_pane, "Enter jumped to a pane the focus leaves out"
        print("PASS find opens on the focus: In focus 1 · Everything 3; beta is not listed")

        # --- 2. ctrl-o lists beta; Enter peeks at it, focus unchanged --------
        herdr.type(CTRL_O)
        screen_until(lambda text: "⊘fsc-beta" in text, what="beta's row marked ⊘ after ctrl-o")
        herdr.drain(0)
        herdr.type("\r")
        # One accumulating read covers the jump: the client's redraw of beta's
        # pane can arrive before Find has exited or the snapshot has moved.
        screen_until(lambda text: SHOWN in text and not find_open() and focused() == beta_pane,
                     what="Enter to close Find, focus beta's pane and the client to draw it")
        after = scope()
        assert after["scope"] == before["scope"], (before["scope"], after["scope"])
        assert after["local"]["tabs"] == before["local"]["tabs"], after
        herdr.drain(1.5)
        assert focused() == beta_pane and focused_workspace() == beta_ws, "beta did not stay focused"
        print(f"PASS ctrl-o + Enter: focused beta {beta_pane} and it stayed; scope unchanged "
              f"(focus {after['scope']['focus']})")

        # --- 3. Herdr's own keys lead back into the focus --------------------
        for key, name in [("\x1b}", "alt+} (next workspace)"), ("\x1b{", "alt+{ (previous workspace)")]:
            focus(beta_pane)
            herdr.type(key)
            herdr.wait_for(lambda: focused_workspace() == alpha_ws, what=f"{name} to reach alpha")
            print(f"PASS {name} from beta lands in focused alpha")
        focus(beta_pane)
        herdr.type("\x1bu")
        herdr.wait_for(lambda: focused() == alpha_pane, what="alt+u to reach alpha's agent")
        assert scope()["scope"] == before["scope"], scope()
        print("PASS alt+u (next attention agent) from beta lands on alpha's agent; scope still unchanged")

        log = herdr.plugin_log()["result"]["logs"]
        failed = [entry for entry in log if entry.get("plugin_id") == "shadowfax.tools"
                  and entry.get("action_id", "").startswith("find") and entry.get("status") != "succeeded"]
        assert not failed, failed


if __name__ == "__main__":
    main()
