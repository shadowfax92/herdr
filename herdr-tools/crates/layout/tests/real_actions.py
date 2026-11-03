"""Every `layout` action once, end to end, through `herdr plugin action invoke`.

Usage, from herdr-tools/:
    python3 crates/layout/tests/real_actions.py

An IsolatedHerdr (temp XDG dirs, its own session, a 50x200 client) gets
shadowfax.tools laid out and linked by `just install`, and a test config.toml
that sizes the `layout-grid` popup. Then, on the server-focused pane:

    layout-split-right   A          -> A | B     B focused, ~30% of the pair
    layout-split-left    A | B      -> A | C | B C focused, ~30% of C+B
    layout-equalize      A | C | B  -> equal thirds
    layout-grid          popup at the configured size; "3x2" Enter -> 3 kept + 3 shells

Popups are absent from `pane list` / `pane layout` on Herdr 0.9.5, so the popup
size is read from the server log's `pane.spawn.start` record (its PTY: the outer
size minus a 2-row border and 3 columns of border plus scrollbar).
"""
import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "tests/support"))
from isolated_herdr import IsolatedHerdr  # noqa: E402

SPAWN = re.compile(r'event="pane\.spawn\.start".*\bpane_id=(\d+) rows=(\d+) cols=(\d+)')
POPUP = {"width": 70, "height": 20}


def spawns(herdr):
    log = next(herdr.root.glob("config/herdr/sessions/*/herdr-server.log")).read_text()
    return [(int(rows), int(cols)) for _, rows, cols in SPAWN.findall(log)]


def main():
    with IsolatedHerdr(rows=50, cols=200, prefix="layout-actions-") as herdr:
        herdr.install()
        herdr.write_plugin_config(
            "[popup.size]\n"
            f"layout-grid = {{ width = {POPUP['width']}, height = {POPUP['height']} }}\n"
        )
        a = herdr.cli("pane", "list")["result"]["panes"][0]["pane_id"]

        screen = []  # every client byte since the last reset, ANSI stripped

        def wait(predicate, timeout=5, what="condition"):
            """IsolatedHerdr.wait_for, but keeping the drained client output."""
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                screen.append(herdr.drain(0.05))
                value = predicate()
                if value:
                    return value
            raise AssertionError(f"timed out waiting for {what}")

        def layout():
            return herdr.cli("pane", "layout", "--pane", a)["result"]["layout"]

        def row(value):
            """Pane ids left to right, with their widths."""
            panes = sorted(value["panes"], key=lambda p: (p["rect"]["y"], p["rect"]["x"]))
            return [p["pane_id"] for p in panes], [p["rect"]["width"] for p in panes]

        def invoke(action):
            seen = len(entries(action))
            herdr.cli("plugin", "action", "invoke", f"shadowfax.tools.{action}")
            entry = wait(
                lambda: (lambda logs: logs[seen] if len(logs) > seen
                         and logs[seen].get("status") not in (None, "running") else None)(entries(action)),
                timeout=15, what=f"the {action} log entry")
            assert entry.get("exit_code") == 0, entry
            return entry

        def entries(action):
            logs = herdr.plugin_log()["result"]["logs"]
            return [entry for entry in logs if entry.get("action_id") == action]

        # 1. split right: a new, focused 30% pane on the right.
        invoke("layout-split-right")
        after = wait(lambda: (lambda v: v if len(v["panes"]) == 2 else None)(layout()),
                               what="the right split")
        (left, b), (left_width, b_width) = row(after)
        assert left == a and after["focused_pane_id"] == b, after
        share = b_width / (left_width + b_width)
        assert 0.27 <= share <= 0.33, (left_width, b_width)
        print(f"PASS layout-split-right: {a} | {b}, {b} focused at {share:.0%} ({b_width} of "
              f"{left_width + b_width} cols)", flush=True)

        # 2. split left on B: the new pane lands left of B with 30% of their space.
        invoke("layout-split-left")
        after = wait(lambda: (lambda v: v if len(v["panes"]) == 3 else None)(layout()),
                               what="the left split")
        ids, widths = row(after)
        assert ids[0] == a and ids[2] == b, after
        c = ids[1]
        assert after["focused_pane_id"] == c, after
        share = widths[1] / (widths[1] + widths[2])
        assert 0.27 <= share <= 0.33, widths
        print(f"PASS layout-split-left: {a} | {c} | {b}, {c} focused at {share:.0%} of its pair "
              f"(widths {widths})", flush=True)

        # 3. equalize: the three-pane row becomes even.
        invoke("layout-equalize")
        after = wait(
            lambda: (lambda v: v if max(row(v)[1]) - min(row(v)[1]) <= 2 else None)(layout()),
            what="equal widths")
        ids, widths = row(after)
        assert ids == [a, c, b] and after["focused_pane_id"] == c, after
        print(f"PASS layout-equalize: widths {widths}", flush=True)

        # 4. grid: the popup opens at the configured size, and 3x2 keeps all three.
        area = after["area"]
        before = len(spawns(herdr))
        screen.clear()
        invoke("layout-grid")
        rows, cols = wait(lambda: (spawns(herdr)[before:] or [None])[0], what="the layout-grid popup")
        assert (rows, cols) == (POPUP["height"] - 2, POPUP["width"] - 3), (rows, cols)
        screen.append(herdr.drain(0.4))
        herdr.type(b"3x2")
        screen.append(herdr.drain(0.3))
        flat = re.sub(r"\s", "", "".join(screen))
        assert "Createlayout" in flat and "Keep3existing·add3emptyshells" in flat, flat[-800:]
        herdr.type(b"\r")
        # Panes reach 6 before the holding tab closes and focus returns; the
        # dialog's last steps are closing its holding shell and refocusing c.
        def finished():
            value = layout()
            done = (len(value["panes"]) == 6 and value["focused_pane_id"] == c
                    and len(herdr.cli("tab", "list")["result"]["tabs"]) == 1)
            return value if done else None

        try:
            after = wait(finished, timeout=20, what="the 3x2 grid")
        except AssertionError:
            raise AssertionError(f"grid did not finish: {layout()}") from None
        ordered = [p["pane_id"] for p in sorted(after["panes"], key=lambda p: (p["rect"]["y"], p["rect"]["x"]))]
        assert ordered[:3] == [a, c, b] and after["focused_pane_id"] == c, after
        assert len({p["rect"]["y"] for p in after["panes"]}) == 2, after
        assert len(herdr.cli("tab", "list")["result"]["tabs"]) == 1
        print(f"PASS layout-grid: popup outer {POPUP['width']}x{POPUP['height']} -> PTY {cols}x{rows} "
              f"(config; manifest default is 80x24) in a {area['width']}x{area['height']} pane area; "
              f"3x2 kept {[a, c, b]} and added {ordered[3:]}", flush=True)


if __name__ == "__main__":
    main()
