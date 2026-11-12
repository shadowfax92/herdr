"""Check zoom retention through a real CLI split and the attached TUI.

Run with python3 herdr-tools/tests/real_zoom_keep.py /absolute/path/to/herdr.
Only IsolatedHerdr's temporary session, files and shells are used. Distinct
terminal output markers verify that hidden output reaches the pane buffer but
does not reach the attached client's screen until zoom ends.
"""

import json
import shlex
import socket
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent / "support"))
from isolated_herdr import IsolatedHerdr  # noqa: E402


def api(herdr, method, **params):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(10)
        connection.connect(herdr.socket)
        connection.sendall((json.dumps({"id": "zoom", "method": method, "params": params}) + "\n").encode())
        response = json.loads(connection.makefile("rb").readline())
    assert "error" not in response, response
    return response["result"]


def pane_size(herdr, pane, name):
    output = herdr.root / name
    output.unlink(missing_ok=True)
    herdr.cli("pane", "run", pane, f"stty size > {shlex.quote(str(output))}")
    herdr.wait_for(lambda: output.exists() and output.read_text().strip(), what="shell dimensions")
    size = tuple(map(int, output.read_text().split()))
    assert len(size) == 2 and all(value > 0 for value in size), size
    return size


def ultrawide_area(herdr, pane):
    area = herdr.cli("pane", "layout", "--pane", pane)["result"]["layout"]["area"]
    return area if area["width"] >= 400 else None


def lone_fitted_lifecycle(binary):
    """Worker arrival and departure must not consume the owner's lone zoom."""
    with IsolatedHerdr(rows=50, cols=483, herdr=binary, prefix="zk-") as herdr:
        owner = herdr.cli("pane", "list")["result"]["panes"][0]["pane_id"]
        area = herdr.wait_for(lambda: ultrawide_area(herdr, owner), what="ultrawide geometry")
        fitted_width = area["width"] - 2 * (area["width"] * 20 // 100) - 3
        api(herdr, "pane.zoom", pane_id=owner, mode="on")
        herdr.wait_for(lambda: pane_size(herdr, owner, "lone-before")[1] == fitted_width,
                       what="a lone fitted zoom")
        # Repeat to verify the split after the last sibling's departure also
        # stays hidden, rather than merely retaining a stale zoom decoration.
        for cycle in range(2):
            worker = herdr.cli("pane", "split", owner, "--direction", "right")["result"]["pane"]["pane_id"]
            layout = herdr.cli("pane", "layout", "--pane", owner)["result"]["layout"]
            assert layout["zoomed"] and layout["focused_pane_id"] == owner, layout
            assert pane_size(herdr, owner, f"lone-split-{cycle}")[1] == fitted_width
            herdr.cli("pane", "close", worker)
            herdr.wait_for(lambda: len(herdr.cli("pane", "list")["result"]["panes"]) == 1,
                           what="the last sibling closing")
            layout = herdr.cli("pane", "layout", "--pane", owner)["result"]["layout"]
            assert layout["zoomed"] and layout["focused_pane_id"] == owner, layout
            assert pane_size(herdr, owner, f"lone-closed-{cycle}")[1] == fitted_width
        api(herdr, "pane.zoom", pane_id=owner, mode="toggle")
        assert not herdr.cli("pane", "layout", "--pane", owner)["result"]["layout"]["zoomed"]
        herdr.wait_for(lambda: pane_size(herdr, owner, "lone-unzoomed")[1] == area["width"] - 1,
                       what="the owner's explicit unzoom")
        print(f"PASS lone fitted: two unfocused split/close cycles keep {fitted_width} columns and focus; toggle unzooms")


def main(binary):
    widths = {}
    for width, method in [("fitted", "pane.zoom"), ("full", "pane.zoom_full")]:
        with IsolatedHerdr(rows=50, cols=483, herdr=binary, prefix="zk-") as herdr:
            owner = herdr.cli("pane", "list")["result"]["panes"][0]["pane_id"]
            area = herdr.wait_for(
                lambda: ultrawide_area(herdr, owner),
                what="the ultrawide client geometry",
            )
            # Full zoom is intentionally a no-op in an unzoomed one-pane tab.
            herdr.cli("pane", "split", owner, "--direction", "down")
            api(herdr, method, pane_id=owner, mode="on")
            # Both paths have borders and a primary-screen scrollbar gutter.
            expected_width = area["width"] - 3
            if width == "fitted":
                expected_width -= 2 * (area["width"] * 20 // 100)
            herdr.wait_for(lambda: pane_size(herdr, owner, "owner-size")[1] == expected_width,
                           what=f"{width} zoom dimensions")
            widths[width] = expected_width
            herdr.drain(0.3)
            new = herdr.cli("pane", "split", owner, "--direction", "right")["result"]["pane"]["pane_id"]
            layout = herdr.cli("pane", "layout", "--pane", owner)["result"]["layout"]
            assert layout["zoomed"] and layout["focused_pane_id"] == owner, layout
            assert pane_size(herdr, owner, "owner-after-split")[1] == expected_width
            hidden_size = pane_size(herdr, new, "hidden-size")
            herdr.cli("pane", "run", new, "printf 'HIDDEN_ZOOM_KEEP_MARKER\\n'")
            herdr.cli("pane", "run", owner, "printf 'OWNER_ZOOM_KEEP_MARKER\\n'")
            output = herdr.drain(0.5)
            assert "OWNER_ZOOM_KEEP_MARKER" in output, output
            assert "HIDDEN_ZOOM_KEEP_MARKER" not in output, output
            hidden = herdr.cli("pane", "read", new)
            assert "HIDDEN_ZOOM_KEEP_MARKER" in json.dumps(hidden), hidden
            layout = herdr.cli("pane", "layout", "--pane", owner)["result"]["layout"]
            assert layout["zoomed"] and layout["focused_pane_id"] == owner, layout
            api(herdr, "pane.zoom", pane_id=owner, mode="off")
            output = herdr.drain(0.5)
            assert "HIDDEN_ZOOM_KEEP_MARKER" in output, output
            visible_size = pane_size(herdr, new, "visible-size")
            assert hidden_size == visible_size, (hidden_size, visible_size)
            assert not herdr.cli("pane", "layout", "--pane", owner)["result"]["layout"]["zoomed"]
            print(f"PASS {width}: CLI split kept focus + zoom; only owner output visible; hidden/visible shell size {hidden_size}")
    assert widths["fitted"] < widths["full"], widths
    print(f"PASS zoom widths: {widths}")
    lone_fitted_lifecycle(binary)


if __name__ == "__main__":
    assert len(sys.argv) == 2, "pass the freshly built herdr binary"
    main(str(Path(sys.argv[1]).resolve(strict=True)))
