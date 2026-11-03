"""Real-herdr end-to-end check of the `comment` mode.

Run from `herdr-tools/` after `just ci` (it builds release through `install`):

    python3 crates/comment/tests/real_comment.py

Everything runs in an IsolatedHerdr (temp XDG dirs, its own session, a 50x200
PTY client); nothing touches ~/.config/herdr or the default session. Each
`shadowfax.tools.comment-*` action runs once through Herdr:

  capture (snapshot browser)  popup sized by [popup.size]; V, c, note, Enter, q
  capture (inline selection)  popup sized by [popup.size]; note, Enter
  review                      popup sized by a [[popup.profile]]; Neovim :wq
  paste                       the ready draft lands in the source pane's input

Popups are absent from `pane list` on Herdr 0.9.5, so sizes are read from the
server log's `pane.spawn.start` record: the PTY Herdr allocated, i.e. the outer
size minus the 2-row border and 3 columns of border plus scrollbar.
"""
import json
import re
import socket
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parents[2] / "tests" / "support"))
from isolated_herdr import IsolatedHerdr  # noqa: E402

SPAWN = re.compile(r'event="pane\.spawn\.start".*\bpane_id=(\d+) rows=(\d+) cols=(\d+)')


def settle(herdr, seconds=0.4):
    """Lets the focused popup process the last keys and redraw.

    Waits here are on state files and spawn records, not screen text: Herdr's
    renderer sends only the cells that changed, so "NORMAL" -> "VISUAL" reaches
    the client as "VISU", and no drawn string is a reliable signal."""
    herdr.drain(seconds)


def spawned_popups(herdr):
    """(rows, cols) of every pane Herdr spawned after the first (the startup shell)."""
    log = next(herdr.root.glob("config/herdr/sessions/*/herdr-server.log")).read_text()
    spawns = [tuple(map(int, match.groups())) for match in SPAWN.finditer(log)]
    return [(rows, cols) for pane_id, rows, cols in spawns if pane_id > 1]


def outer(size, available):
    return available * int(size.rstrip("%")) // 100 if isinstance(size, str) else size


def request(herdr, method, params):
    """One socket API call to the isolated server (the CLI cannot pass a selection)."""
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.connect(herdr.socket)
        sock.sendall(json.dumps({"id": "real-comment", "method": method, "params": params})
                     .encode() + b"\n")
        reply = b""
        while not reply.endswith(b"\n"):
            chunk = sock.recv(65536)
            assert chunk, "herdr closed the socket"
            reply += chunk
    reply = json.loads(reply)
    assert "error" not in reply, reply
    return reply["result"]


def state_dir(herdr):
    return herdr.root / "state/herdr/plugins/shadowfax.tools"


def comments(herdr):
    """Every collected comment, oldest first."""
    found = [json.loads(path.read_text())
             for path in state_dir(herdr).glob("collections/*/*.json")]
    return sorted(found, key=lambda comment: comment["created_at_ns"])


def ready_drafts(herdr):
    return [json.loads(path.read_text()) for path in state_dir(herdr).glob("ready/*.json")]


def action_log(herdr, action):
    return [entry for entry in herdr.plugin_log()["result"]["logs"]
            if entry.get("action_id") == action]


def invoke(herdr, action, context=None):
    """Invokes `shadowfax.tools.<action>` and waits for Herdr to log it as finished."""
    before = len(action_log(herdr, action))
    if context is None:
        herdr.cli("plugin", "action", "invoke", f"shadowfax.tools.{action}")
    else:
        request(herdr, "plugin.action.invoke",
                {"action_id": f"shadowfax.tools.{action}", "context": context})
    entries = herdr.wait_for(lambda: [entry for entry in action_log(herdr, action)[before:]
                                      if entry.get("status") != "running"],
                             timeout=15, what=f"the {action} action to finish")
    entry = entries[-1]
    assert entry["status"] == "succeeded" and entry["exit_code"] == 0, entry
    return entry


def expect_popup(herdr, before, name, width, height, area):
    popups = herdr.wait_for(lambda: spawned_popups(herdr)[before:], what=f"the {name} popup")
    rows, cols = popups[0]
    expected = (outer(height, area["height"]) - 2, outer(width, area["width"]) - 3)
    assert (rows, cols) == expected, (name, (rows, cols), expected)
    print(f"PASS popup {name}: config {width}x{height} -> Herdr PTY {cols}x{rows}")


def main():
    with IsolatedHerdr(rows=50, cols=200) as herdr:
        plugin_dir = herdr.install()
        version = herdr.run([str(plugin_dir / "bin/herdr-tools"), "--version"]).stdout.strip()
        print(f"PASS install + link: {plugin_dir} ({version})")

        work = herdr.root / "work"
        work.mkdir()
        source = herdr.cli("pane", "list")["result"]["panes"][0]["pane_id"]
        # Run in the temp root so nothing the pane does can land in the checkout.
        herdr.cli("pane", "run", source, f"cd '{work}' && printf 'alpha line\\nbeta line\\n'")
        herdr.wait_for(lambda: "beta line\n" in herdr.cli("pane", "read", source,
                                                         "--source", "recent"),
                       what="the source pane's output")
        area = herdr.cli("pane", "layout", "--pane", source)["result"]["layout"]["area"]
        client_width = area["x"] + area["width"]
        config = (
            "[comment]\n"
            "inline_comments = {inline}\n\n"
            "[popup.size]\n"
            "comment-capture = { width = 100, height = 30 }\n\n"
            "[[popup.profile]]\n"
            'name = "test-narrow"\n'
            f"max_client_width = {client_width}\n"
            'size.comment-review = { width = "60%", height = "50%" }\n'
        )

        # 1. comment-capture, snapshot browser: comment on the top lines, then quit.
        herdr.write_plugin_config(config.replace("{inline}", "false"))
        before = len(spawned_popups(herdr))
        invoke(herdr, "comment-capture")
        expect_popup(herdr, before, "comment-capture (snapshot)", 100, 30, area)
        settle(herdr, 1.0)
        for key in ["g", "V", "j", "c"]:
            herdr.type(key)
            settle(herdr)
        herdr.type("first note")
        settle(herdr)
        herdr.type("\r")
        herdr.wait_for(lambda: len(comments(herdr)) == 1, what="the first comment")
        herdr.type("q")
        herdr.wait_for(lambda: not list(state_dir(herdr).glob("runs/*.json")),
                       what="the annotation run to close")
        first = comments(herdr)[0]
        assert first["note"] == "first note" and first["source_text"].strip(), first
        print(f"PASS capture: collected {first['source_text']!r} -> {first['note']!r}")

        # 2. comment-capture, inline: a Copy-mode selection opens straight into the editor.
        herdr.write_plugin_config(config.replace("{inline}", "true"))
        before = len(spawned_popups(herdr))
        invoke(herdr, "comment-capture", {"selected_text": "gamma selection"})
        expect_popup(herdr, before, "comment-capture (inline)", 100, 30, area)
        settle(herdr, 1.0)
        herdr.type("second note")
        settle(herdr)
        herdr.type("\r")
        herdr.wait_for(lambda: len(comments(herdr)) == 2, what="the inline comment")
        herdr.wait_for(lambda: not list(state_dir(herdr).glob("runs/*.json")),
                       what="the inline popup to close")
        second = comments(herdr)[1]
        assert (second["source_text"], second["note"]) == ("gamma selection", "second note")
        print(f"PASS inline capture: collected {second['source_text']!r} -> {second['note']!r}")

        # 3. comment-review: Neovim on the assembled Markdown; :wq saves a ready draft.
        before = len(spawned_popups(herdr))
        invoke(herdr, "comment-review")
        expect_popup(herdr, before, "comment-review", "60%", "50%", area)
        settle(herdr, 2.0)
        herdr.type(":wq\r")
        drafts = herdr.wait_for(lambda: ready_drafts(herdr), timeout=10, what="the ready draft")
        markdown = drafts[0]["markdown"]
        expected = ("> " + first["source_text"].replace("\n", "\n> ").replace("> \n", ">\n")
                    + "\n\nfirst note\n\n> gamma selection\n\nsecond note\n")
        assert markdown == expected, (markdown, expected)
        assert len(comments(herdr)) == 2, "saving a review must not clear comments"
        print(f"PASS review: ready draft of {len(drafts[0]['comment_ids'])} comment(s)")

        # 4. comment-paste: the draft is typed into the source pane, unsubmitted.
        pasted = work / "pasted.txt"
        herdr.cli("pane", "run", source, f"cat > '{pasted}'")
        time.sleep(0.5)
        invoke(herdr, "comment-paste")
        herdr.wait_for(lambda: pasted.is_file() and "second note" in pasted.read_text(),
                       what="the pasted draft")
        text = pasted.read_text()
        assert "first note" in text and "> gamma selection" in text, text
        assert not ready_drafts(herdr) and not comments(herdr), "paste must clear what it sent"
        print(f"PASS paste: {len(text)} bytes reached the pane; draft and comments cleared")
        herdr.cli("pane", "send-keys", source, "C-d", check=False)


if __name__ == "__main__":
    main()
