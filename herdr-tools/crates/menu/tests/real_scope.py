"""End-to-end check of herdr-tools against Herdr's native focus and hide: the
menu's focus / hide / focus_toggle rows (and the retired workspace_visibility
row, which is now hide) and Find's scope filter, in an isolated Herdr.

Usage (from herdr-tools/):
    HERDR_BIN=/path/to/fork/target/debug/herdr python3 crates/menu/tests/real_scope.py

It needs a Herdr with `herdr scope …` and the `scope.get` / `scope.set` /
`scope.pick` socket methods (the fork's focus-hide work). On a Herdr without
them (stable 0.9.5) it prints a SKIP line and exits 0; real_menu.py covers the
"(unavailable)" rows there. HERDR_BIN picks the binary (default: `herdr` on
PATH).

What it drives, all through the real laid-out plugin and the real CLI:

1. Find: `herdr scope hide beta` drops beta's pane from the navigator (Enter on
   its name focuses nothing) while alpha's is still reachable; with focus on
   beta it is the other way round.
2. Menu, focus_toggle (F): the row shows its configured label, no state; F
   asks the client to flip focus on Local (`scope.toggle`), closes the menu,
   and keeps the targets. On a build with scope.get but no scope.toggle yet the
   menu stays open with that refusal, and this step prints SKIP.
3. Menu, hide (x), focus (f) and the retired workspace_visibility row (v): each
   closes the menu and opens Herdr's picker in its mode. The picker's own keys
   (ctrl+j moves, Tab toggles the mode's mark and moves on, enter applies) then
   change exactly the part of the scope that mode owns.
4. The picker's search: typing `local beta` narrows the list to beta and
   selects it, Tab hides that filtered row, and enter applies it.

Everything runs in an IsolatedHerdr (temp XDG dirs, its own session, a PTY
client), so the owner's live Herdr and ~/.config/herdr are never touched.
"""
import json
import os
import re
import socket
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
TOOLS = HERE.parents[2]
sys.path.insert(0, str(TOOLS / "tests/support"))
from isolated_herdr import IsolatedHerdr  # noqa: E402

MENU = """\
[menu]
title = "scope e2e"

[[menu.items]]
type = "focus"
key = "f"
label = "focus…"

[[menu.items]]
type = "hide"
key = "x"
label = "hide…"

[[menu.items]]
type = "focus_toggle"
key = "F"
label = "focus on/off"

[[menu.items]]
type = "workspace_visibility"
key = "v"
label = "excluded workspaces"
"""


def flat(text):
    return re.sub(r"\s", "", text)


def main():
    binary = os.environ.get("HERDR_BIN")
    with IsolatedHerdr(rows=50, cols=200, herdr=binary, prefix="scope-e2e-") as herdr:
        version = herdr.cli("--version", check=False)
        version = getattr(version, "stdout", "").strip() or "unknown version"
        probe = herdr.cli("scope", "get", "--json", check=False)
        if not isinstance(probe, dict):
            detail = (probe.stderr or probe.stdout).strip().splitlines()[:1]
            print(f"SKIP real_scope.py: {herdr.herdr} ({version}) has no `herdr scope get` "
                  f"({detail[0] if detail else 'no output'}). Run it against a fork build with "
                  f"focus and hide: HERDR_BIN=/path/to/herdr python3 {Path(__file__).name}")
            return

        def scope():
            """`herdr scope get --json`: the scope.get reply, envelope or not."""
            reply = herdr.cli("scope", "get", "--json")
            return reply.get("result", reply)

        def rpc(method, params):
            """One socket request to this instance's server (never another)."""
            with socket.socket(socket.AF_UNIX) as connection:
                connection.settimeout(5)
                connection.connect(herdr.socket)
                request = {"id": "scope-e2e", "method": method, "params": params}
                connection.sendall(json.dumps(request).encode() + b"\n")
                response = json.loads(connection.makefile("rb").readline())
            assert "error" not in response, response
            return response["result"]

        def running(command):
            found = subprocess.run(["pgrep", "-f", f"{plugin_dir}/bin/herdr-tools {command}"],
                                   capture_output=True, text=True)
            return found.returncode == 0

        def screen_until(found, timeout=10, what="text"):
            screen, deadline = "", time.monotonic() + timeout
            while not found(flat(screen)):
                assert time.monotonic() < deadline, f"timed out waiting for {what}: {screen[-2000:]}"
                screen += herdr.drain(0.1)
            return screen

        def focused():
            return herdr.cli("api", "snapshot")["result"]["snapshot"]["focused_pane_id"]

        def refocus(pane, tab):
            rpc("pane.focus", {"pane_id": pane})
            rpc("tab.focus", {"tab_id": tab})
            herdr.drain(0.5)

        def invoke(action, popup):
            herdr.drain(0)
            herdr.cli("plugin", "action", "invoke", f"shadowfax.tools.{action}")
            herdr.wait_for(lambda: running(popup), timeout=10, what=f"the {action} popup")

        def find_and_enter(query):
            """Opens Find, types `query`, presses Enter; returns the panes
            focused before and after. Find loads its catalog on a worker, hence
            the waits."""
            start = focused()
            invoke("find", "find picker")
            herdr.drain(1.0)
            herdr.type(query)
            herdr.drain(0.6)
            herdr.type("\r")
            herdr.drain(1.0)
            if running("find picker"):  # no match: Enter has nothing to jump to
                herdr.type("\x1b")
                herdr.wait_for(lambda: not running("find picker"), what="Find to close")
            return start, focused()

        def find_lists(query, pane):
            """Whether Find lists `pane` under `query`: Enter on it focuses it."""
            start, end = find_and_enter(query)
            if end == pane:
                return True
            assert end == start, f"Enter on {query!r} focused {end}, neither {start} nor {pane}"
            return False

        plugin_dir = herdr.install()
        snapshot = herdr.cli("api", "snapshot")["result"]["snapshot"]
        source, source_tab = snapshot["panes"][0]["pane_id"], snapshot["panes"][0]["tab_id"]
        alpha = herdr.cli("workspace", "create", "--label", "alpha", "--no-focus")["result"]
        beta = herdr.cli("workspace", "create", "--label", "beta", "--no-focus")["result"]
        shown, hidden = alpha["root_pane"]["pane_id"], beta["root_pane"]["pane_id"]
        herdr.cli("pane", "rename", shown, "scope-e2e-alpha")
        herdr.cli("pane", "rename", hidden, "scope-e2e-beta")
        herdr.write_plugin_config(MENU)
        print(f"PASS {herdr.herdr} ({version}) has `herdr scope`; plugin at {plugin_dir}; "
              f"alpha pane {shown}, beta pane {hidden}")

        # --- 1. Find drops panes whose tab is out of scope ------------------
        herdr.cli("scope", "hide", "beta")
        tabs = {tab["tab_id"]: tab["admitted"] for tab in scope()["local"]["tabs"]}
        beta_tab = herdr.cli("pane", "get", hidden)["result"]["pane"]["tab_id"]
        assert tabs.get(beta_tab) is False and tabs.get(source_tab) is True, tabs
        print(f"PASS scope.get judges local tabs after `scope hide beta`: {tabs}")

        assert not find_lists("scope-e2e-beta", hidden), "Find listed a pane in hidden beta"
        assert find_lists("scope-e2e-alpha", shown), "Find lost alpha's pane"
        refocus(source, source_tab)
        print("PASS find: hidden beta's pane is not listed; alpha's still is")

        herdr.cli("scope", "unhide", "beta")
        herdr.cli("scope", "focus", "beta")
        assert scope()["scope"]["focus"]["active"], scope()
        assert not find_lists("scope-e2e-alpha", shown), "Find listed alpha outside the focus"
        assert find_lists("scope-e2e-beta", hidden), "Find lost focused beta's pane"
        refocus(source, source_tab)
        print("PASS find: with focus on beta only beta's pane is listed")

        # --- 2. focus_toggle: a static label; the client flips Local's focus -
        def press_toggle():
            """Opens the menu and presses F. True once the menu has closed;
            False when this Herdr refused scope.toggle as unknown (the menu
            then stays open with the refusal, and is closed here)."""
            # Not invoke(): its wait discards the output holding the first frame.
            herdr.drain(0)
            herdr.cli("plugin", "action", "invoke", "shadowfax.tools.menu")
            screen = screen_until(lambda text: "Ffocuson/off" in text, what="the F row")
            assert "(unavailable)" not in screen and "focuson/off·" not in flat(screen), screen
            herdr.type("F")
            screen, deadline = "", time.monotonic() + 10
            while running("menu menu"):
                assert time.monotonic() < deadline, f"F neither closed the menu nor failed: {screen[-1500:]}"
                screen += herdr.drain(0.1)
                if "hasnoscope.toggle" in flat(screen):
                    herdr.type("\x1b")
                    herdr.wait_for(lambda: not running("menu menu"), what="the menu to close after Esc")
                    return False
            return True

        def focus_active_is(value):
            return lambda: scope()["scope"]["focus"]["active"] is value

        # Step 1 left focus on, with beta its one target.
        if press_toggle():
            herdr.wait_for(focus_active_is(False), what="F to switch focus off on Local")
            assert len(scope()["scope"]["focus"]["targets"]) == 1, scope()
            assert press_toggle(), "the second F was refused"
            herdr.wait_for(focus_active_is(True), what="F to switch focus back on")
            assert len(scope()["scope"]["focus"]["targets"]) == 1, scope()
            print("PASS F: static 'focus on/off' label; on -> off -> on through the client, "
                  "targets kept, menu closed each time")
        else:
            print("SKIP F: this Herdr has scope.get but no scope.toggle yet; the menu showed "
                  "the refusal and stayed open")
        herdr.cli("scope", "clear")
        herdr.cli("scope", "unfocus")
        refocus(source, source_tab)

        # --- 3. focus / hide / retired v open the picker in their mode -------
        for key, mode, owned in [("x", "hide", "hidden"), ("v", "hide", "hidden"),
                                 ("f", "focus", "focus")]:
            before = scope()["scope"]
            invoke("menu", "menu menu")
            herdr.drain(0.5)
            herdr.type(key)
            # The menu closes its popup before Herdr opens the picker.
            herdr.wait_for(lambda: not running("menu menu"), what=f"the menu to close after {key}")
            herdr.drain(1.0)
            # The picker's contract (DESIGN.md, fzf keys): ctrl+j (a bare line
            # feed) moves, Tab toggles this mode's mark and moves on, enter
            # applies one scope.set to Local.
            herdr.type("\n")
            herdr.drain(0.3)
            herdr.type("\t")
            herdr.drain(0.3)
            herdr.type("\r")
            # A timeout with the picker open means its keys went elsewhere,
            # e.g. to the menu's closed popup.
            herdr.wait_for(lambda: scope()["scope"] != before, timeout=10,
                           what=f"{key}'s {mode} picker to apply a change "
                                "(ctrl+j, Tab, enter in Herdr's picker)")
            after = scope()["scope"]
            # Space toggles this mode's mark only, so the other list is untouched.
            if owned == "hidden":
                assert after["hidden"] != before["hidden"], (key, before, after)
                assert after["focus"]["targets"] == before["focus"]["targets"], (key, before, after)
            else:
                assert after["focus"]["targets"] != before["focus"]["targets"], (key, before, after)
                assert after["hidden"] == before["hidden"], (key, before, after)
            print(f"PASS {key}: menu closed, Herdr's {mode} picker opened; Tab+enter changed "
                  f"only scope.{owned}")
            herdr.cli("scope", "clear")
            herdr.cli("scope", "unfocus")
            refocus(source, source_tab)

        # --- 4. search: type a query, Tab the filtered row, enter ------------
        assert not scope()["scope"]["hidden"], scope()
        invoke("menu", "menu menu")
        herdr.drain(0.5)
        herdr.type("x")
        herdr.wait_for(lambda: not running("menu menu"), what="the menu to close after x")
        herdr.drain(1.0)
        # Two terms across machine and workspace; the best match (beta) is
        # selected as the query changes, so Tab hides it without moving there.
        herdr.type("local beta")
        herdr.drain(0.5)
        herdr.type("\t")
        herdr.drain(0.3)
        herdr.type("\r")
        herdr.wait_for(lambda: scope()["scope"]["hidden"], timeout=10,
                       what="the hide picker to apply the searched row")
        hidden = scope()["scope"]["hidden"]
        assert [(t.get("workspace_label"), t.get("tab_label")) for t in hidden] == [("beta", None)], hidden
        assert hidden[0].get("workspace_id") == beta["workspace"]["workspace_id"], (hidden, beta)
        assert not scope()["scope"]["focus"]["targets"], scope()
        print("PASS search: typing 'local beta' in the hide picker selected beta; Tab+enter hid "
              "exactly that workspace")


if __name__ == "__main__":
    main()
