"""Opt-in real TUI acceptance, using only a private HOME and named Herdr server.

Usage: uv run --with pyte==0.8.2 python tests/real_herdr.py [--width 140] [--height 40] [--evidence /absolute/directory]
Build the release binary first. No live plugin registry, focus, or config is used.
"""
import argparse
from datetime import datetime, timezone
import fcntl
import json
import os
from pathlib import Path
import pty
import pyte
import re
import select
import shutil
import socket
import struct
import subprocess
import tempfile
import termios
import threading
import time
import uuid

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--width", type=int, default=140)
parser.add_argument("--height", type=int, default=40)
parser.add_argument("--evidence", type=Path)
args = parser.parse_args()
root_repo = Path(__file__).resolve().parents[1]
herdr = shutil.which("herdr")
assert herdr and (root_repo / "target/release/herdr-jump").is_file()
session = f"jump-test-{os.getpid()}"
checks = []

with tempfile.TemporaryDirectory(prefix="jump-tui-", dir="/tmp") as directory:
    # macOS reports process cwd under /private/tmp, even when launched via
    # /tmp. HOME must use the same spelling to exercise lexical ~ collapsing.
    root = Path(directory).resolve()
    (root / "home").mkdir()
    config = root / "config.toml"
    # Herdr plays a notification sound when agents change state, and sound is on
    # by default. The fixture agents change state constantly, so a test run would
    # play sounds through the owner's speakers. The config key and the env var
    # each disable it; both are set so neither alone can regress it.
    config.write_text('onboarding = false\n[terminal]\ndefault_shell = "/bin/sh"\n[keys]\ngoto = "alt+p"\n'
                      '[ui.sound]\nenabled = false\n')
    env = {k: v for k, v in os.environ.items() if not k.startswith("HERDR_")}
    # This PTY represents a color-capable terminal. The worker's CLI-wide
    # NO_COLOR setting would otherwise erase the active-view pill under test.
    env.pop("NO_COLOR", None)
    env.update(HOME=str(root / "home"), CFFIXED_USER_HOME=str(root / "home"),
               XDG_CONFIG_HOME=str(root / "config"), XDG_STATE_HOME=str(root / "state"),
               HERDR_CONFIG_PATH=str(config), HERDR_DISABLE_SOUND="1", TERM="xterm-256color",
               CODEX_HOME=str(root / "codex"), CLAUDE_CONFIG_DIR=str(root / "claude"))
    # This directory starts absent, exercising standalone Jump in all existing
    # checks. Later a fake Agents owner uses the same inherited discovery path.
    agents_state = root / "agents"
    env["HERDR_AGENTS_STATE"] = str(agents_state)
    policy_stop = threading.Event()
    policy_thread = None
    policy_gates = []
    policy_evidence = {"requests": [], "scenarios": {}}

    def cli(*command):
        result = subprocess.run([herdr, "--session", session, *command], env=env,
                                capture_output=True, text=True, timeout=12)
        assert result.returncode == 0, (command, result.stdout, result.stderr)
        return json.loads(result.stdout) if result.stdout.lstrip().startswith("{") else result.stdout

    pid, fd = pty.fork()
    if pid == 0:
        os.execve(herdr, [herdr, "--session", session], env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", args.height, args.width, 0, 0))
    transcripts = {}
    screens = {}
    kill_evidence = {}
    fixture_evidence = {}
    view_evidence = {}
    search_evidence = {}
    screen_evidence = {}
    capture_times = {}
    # Labels repeat (every origin_focus() captures "origin"), so per-label
    # transcripts can't be replayed; the whole session plus each frame's end
    # offset can, e.g. to render screenshots of any frame afterwards.
    session_bytes, frame_ends = bytearray(), []
    class Screen(pyte.Screen):
        # Herdr probes modern private DSR variants that pyte 0.8 doesn't accept.
        # Ignore those probes; ordinary cursor reports are answered below.
        def report_device_status(self, mode, **kwargs):
            if not kwargs.get("private"):
                super().report_device_status(mode)

    screen = Screen(args.width, args.height)
    stream = pyte.ByteStream(screen)

    def frame(label, seconds=0.5):
        started_at = int(time.time() * 1000)
        deadline = time.monotonic() + seconds
        output = bytearray()
        while time.monotonic() < deadline:
            ready, _, _ = select.select([fd], [], [], max(0, deadline - time.monotonic()))
            if ready:
                try:
                    chunk = os.read(fd, 262144)
                except OSError:
                    break
                if not chunk:
                    break
                output.extend(chunk)
                session_bytes.extend(chunk)
                stream.feed(chunk)
                if b"\x1b[6n" in chunk:
                    os.write(fd, b"\x1b[1;1R")
        transcripts[label] = bytes(output)
        frame_ends.append([label, len(session_bytes)])
        text = "\n".join(screen.display)
        screens[label] = text
        capture_times[label] = (started_at, int(time.time() * 1000))
        return text

    def snapshot():
        return cli("api", "snapshot")["result"]["snapshot"]

    def rpc(method, params):
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(5)
            connection.connect(host_socket)
            connection.sendall(json.dumps({"id":"jump-test", "method":method,"params":params}).encode() + b"\n")
            response = json.loads(connection.makefile("rb").readline())
            assert "error" not in response, response
            return response["result"]

    def origin_focus():
        rpc("pane.focus", {"pane_id":origin})
        rpc("tab.focus", {"tab_id":origin_tab})
        frame("origin")

    def open_picker(label):
        os.write(fd, b"\x1bp")
        text = frame(label)
        assert "Jump to session" in text and "Type a session" in text, text[-3000:]
        # Measure the rendered host border, including its two rows. This checks
        # the actual popup geometry rather than only its manifest/request value.
        lines = text.splitlines()
        top = next(i for i, line in enumerate(lines) if "┌Jump" in line)
        left = lines[top].index("┌Jump")
        bottom = next(i for i in range(top + 1, len(lines)) if lines[i][left:left+1] == "└")
        assert abs((bottom - top + 1) - args.height * 0.9) <= 2, (top, bottom, args.height)
        # 132 columns wide, or the pane area less 2 columns each side where
        # that is narrower (Herdr itself would fit it flush to both edges).
        right = lines[top].index("┐", left)
        area = cli("pane", "layout", "--pane", origin)["result"]["layout"]["area"]["width"]
        width = right - left + 1
        assert width == min(132, area - 4), (label, width, area)
        screen_evidence.setdefault(label, {}).update(popup_width=width, area_width=area)

    def popup(label):
        """Crop the actual popup; matching the host sidebar would be a false pass.

        Call immediately after capturing a frame. Pyte's current cells also let
        us distinguish the active header pill from the other visible view labels.
        The host border and the picker's one-cell side padding are excluded.
        """
        lines = screens[label].splitlines()
        top = next(i for i, line in enumerate(lines) if "┌Jump" in line)
        left = lines[top].index("┌Jump")
        right = lines[top].index("┐", left)
        bottom = next(i for i in range(top + 1, len(lines)) if lines[i][left:left+1] == "└")
        content = [line[left + 2:right - 1] for line in lines[top + 1:bottom]]
        header_cells = [screen.buffer[top + 1][x] for x in range(left + 2, right - 1)]
        active = "".join(cell.data for cell in header_cells if cell.bg != "default").strip()
        screen_evidence.setdefault(label, {}).update(popup=content, active_view=active,
                                                     content_origin=[left + 2, top + 1])
        return content, active

    def name_span(content):
        """The Name column's ` › ` positions, before it and before Task, read off
        the column heading; None when rows stack or the popup has no Name column."""
        heading = content[3] if len(content) > 3 else ""
        if not re.search(r"Workspace\s+› Tab\s+› Name\s+› Task", heading):
            return None
        return heading.index("› Name"), heading.index("› Task")

    def assert_name_cell(label, line, name, span):
        """A named row shows its name (or its start and `…`) in the column; an
        unnamed row leaves the cell blank with no `›` before it. Task stays in
        line either way."""
        before_name, before_task = span
        cell = line[before_name + 2:before_task].rstrip()
        assert line[before_task:before_task + 2] == "› ", (label, "Task column out of line", line)
        if name:
            assert line[before_name:before_name + 2] == "› ", (label, line)
            assert cell == name or (cell.endswith("…") and name.startswith(cell[:-1])), (label, name, line)
        else:
            assert line[before_name:before_name + 2] == "  " and not cell, (label, "unnamed row must be blank", line)

    def assert_search(label, query, expected):
        """Check membership and reasons inside result rows, including terminal colors.

        The popup query, selected-pane details and host sidebar repeat searchable
        text, so only the gutter identifies a result. At narrow widths the tab
        moves to that row's second line; chips remain beside the elided title.
        Where the popup has a Name column, a matched name highlights there
        instead of in a chip. These fixtures use ASCII so string offsets equal
        terminal cell offsets.
        """
        content, active = popup(label)
        span = name_span(content)
        assert (span is not None) == (len(content[0]) >= 80), (label, "Name column at wide popups only", content[3])
        total = len(search_evidence["snapshot"]["panes"])
        assert active == f"All {total}", (label, active, total)
        count = re.search(r"(?<!\d)(\d+)/(\d+)\s*$", content[1])
        assert query in content[1] and count and (int(count[1]), int(count[2])) == (len(expected), total), (label, content[1])
        positions = [i for i in range(3, len(content) - 1)
                     if re.match(r"^[ ›■] [×✓◐○·] ", content[i])]
        assert len(positions) == len(expected), (label, positions, expected)
        left, top = screen_evidence[label]["content_origin"]
        observed = []
        screen_evidence[label]["search_rows"] = observed
        # Herdr emits the ANSI palette as indexed colors. Pyte stores these
        # slots as RGB strings: yellow 3, gray 7, dark gray 8.
        yellow, gray, dark_gray = "cdcd00", "e5e5e5", "7f7f7f"
        for index, (position, row) in enumerate(zip(positions, expected)):
            line = content[position]
            cells = [screen.buffer[top + position][left + x] for x in range(len(line))]
            observed.append({"pane_id": row["pane_id"], "line": line,
                             "cells": [{"text": c.data, "fg": c.fg, "bg": c.bg, "bold": c.bold} for c in cells]})
            assert line[:4] == ("› · " if index == 0 else "  · "), (label, line)
            task_start = line.rfind(" › ") + 3 if " › " in line else 4
            title, separator, chip = line[task_start:].rstrip().partition(" · ")
            # Elision may shorten the title to make room for the explanation,
            # but the visible prefix must still identify this particular row.
            assert title == row["title"] or (title.endswith("…") and len(title) >= 8
                                            and row["title"].startswith(title[:-1])), (label, row, title)
            title_cells = cells[task_start:task_start + len(title)]
            if span:
                assert_name_cell(label, line, row["name"], span)
            if row["field"] == "path" or (row["field"] == "name" and not span):
                assert separator == " · " and chip, (label, row, line)
                assert not any(c.fg == yellow for c in title_cells), (label, "unrelated title highlight", line)
                reason_start = task_start + len(title)
                text, reason_cells = separator + chip, cells[reason_start:reason_start + len(separator + chip)]
                assert row["matched"] in chip, (label, "matched chip fragment was elided", chip, row)
                if row["field"] == "path":
                    # Chips may drop directories, including ~ (Task shares the
                    # row with the Name column, so even a full-width popup can
                    # elide); every retained piece must belong to the known path.
                    path_pattern = ".*".join(re.escape(piece) for piece in chip.split("…"))
                    assert re.fullmatch(path_pattern, row["path"]), (label, row, chip)
                else:
                    # A narrow popup may shorten the name around its match.
                    name_pattern = ".*".join(re.escape(piece) for piece in chip.split("…"))
                    assert re.fullmatch(name_pattern, row["name"]), (label, row, chip)
                context_color = gray if index == 0 else dark_gray
            elif row["field"] == "name":
                assert not separator, (label, "a name in its column needs no chip", line)
                assert not any(c.fg == yellow for c in title_cells), (label, "unrelated title highlight", line)
                start = span[0] + 2
                text, reason_cells = row["name"], cells[start:start + len(row["name"])]
                context_color = gray if index == 0 else dark_gray
            else:
                assert not separator, (label, "visible match must not duplicate a chip", line)
                if row["field"] == "title":
                    text, reason_cells = title, title_cells
                else:
                    # In the stacked layout, the visible tab is in the next
                    # line's context; never borrow the selected-pane details.
                    tab_line = line if " › " in line else content[position + 1]
                    tab_y = position if " › " in line else position + 1
                    tab_start = tab_line.index(row["tab"])
                    text = row["tab"]
                    reason_cells = [screen.buffer[top + tab_y][left + tab_start + x] for x in range(len(text))]
                    observed[-1]["context_line"] = tab_line
                context_color = None
            matched_start = text.index(row["matched"])
            matched_indices = range(matched_start, matched_start + len(row["matched"]))
            observed[-1].update(reason_text=text, matched=row["matched"],
                                reason_cells=[{"text": c.data, "fg": c.fg, "bold": c.bold} for c in reason_cells])
            for x, cell in enumerate(reason_cells):
                if x in matched_indices:
                    assert cell.fg == yellow and cell.bold, (label, text, x, cell)
                elif context_color:
                    assert cell.fg == context_color and not cell.bold, (label, text, x, cell)
                else:
                    assert cell.fg != yellow, (label, "unmatched letter highlighted", text, x, cell)
        # The selected pane's details end with its pane id, and lead with its
        # name only where its row can't show the whole name.
        identity = content[-3].strip()
        first = expected[0]
        assert identity.endswith(" · " + first["pane_id"]), (label, identity)
        if first["name"]:
            whole = span is not None and len(first["name"]) <= span[1] - span[0] - 3
            assert identity.startswith(first["name"] + " · ") != whole, (label, "details and Name column", identity)
        screen_evidence[label]["details"] = content[-4:-2]
        assert_footer(label, "browse")

    def prepare_search_fixtures():
        """Use real titles, labels and cwd fields without borrowing live panes.

        All directories are under the private HOME. Neutral sleeping shells
        avoid status/age ties and keep foreground cwd stable during searches.
        The two noise titles prove exclusions, rather than merely testing that
        the three desired rows happen to appear somewhere in the results.
        """
        specifications = [
            ("name", "CLAUDE.md task label rules", "vim-html-skill", "skills", ".config/skl/library/skills"),
            # Unnamed, like most shells: the Name column must stay blank.
            ("title", "Logseq fork with vim navigation", "", "notes", "projects/logseq"),
            ("tab", "Rill diff colors contrast", "diag-sidekick-send", "nvim", ".config/nvim"),
            ("ferry", "Review ferry kill mode", "ferry-review", "ferry", "projects/ferry"),
            ("cloudflare", "Explain the Cloudflare block | jev-like-model", "cloudflare-block", "network", "projects/jev-like-model"),
        ]
        fixtures = {}
        for key, title, name, tab, relative_path in specifications:
            cwd = root / "home" / relative_path
            cwd.mkdir(parents=True, exist_ok=True)
            pane = cli("tab", "create", "--label", tab, "--cwd", str(cwd), "--no-focus")["result"]["root_pane"]["pane_id"]
            if name:
                cli("pane", "rename", pane, name)
            cli("pane", "run", pane, "sleep 300")
            cli("pane", "report-metadata", pane, "--source", "jump:search-title", "--title", title)
            fixtures[key] = {"pane_id": pane, "title": title, "name": name, "tab": tab,
                             "path": f"~/{relative_path}", "field": key, "matched": "vim"}
        frame("search-prepared")
        current = snapshot()
        panes = {pane["pane_id"]: pane for pane in current["panes"]}
        tabs = {tab["tab_id"]: tab for tab in current["tabs"]}
        search_evidence.update(fixtures=fixtures, snapshot=current)
        for fixture in fixtures.values():
            pane = panes[fixture["pane_id"]]
            assert pane["title"] == fixture["title"] and (pane.get("label") or "") == fixture["name"], (fixture, pane)
            assert tabs[pane["tab_id"]]["label"] == fixture["tab"], (fixture, pane)
            assert pane["cwd"] == str(root / "home") + fixture["path"][1:], (fixture, pane)
            assert (pane.get("foreground_cwd") or pane["cwd"]) == pane["cwd"], (fixture, pane)
            assert not pane.get("agent"), (fixture, pane)
        return fixtures

    def compact_age(started_at, now):
        seconds = max(0, (now - started_at) // 1000)
        for divisor, suffix in ((86400, "d"), (3600, "h"), (60, "m"), (1, "s")):
            if seconds >= divisor or divisor == 1:
                return f"{seconds // divisor}{suffix}"

    def assert_rows(label, expected, marked=(), inert=()):
        """Compare complete row order, glyphs, marks and trailing ages to host data.

        Only the fixed gutter/glyph prefix identifies rows, so repeated titles
        in tabs, the selected-pane details or the query cannot satisfy a check.
        Both the one-line and stacked layouts use this same row prefix.
        """
        content, _ = popup(label)
        rows = [line for line in content[3:-1] if re.match(r"^[ ›■] [×✓◐○·] ", line)]
        assert len(rows) == len(expected), (label, rows, expected)
        span = name_span(content)
        glyphs = {"blocked": "×", "done": "✓", "working": "◐", "idle": "○", "neutral": "·"}
        for line, row in zip(rows, expected):
            task = line.rsplit(" › ", 1)[-1] if " › " in line else line[4:]
            age = re.search(r"(?:^|\s)(\d+[smhd])\s*$", task)
            title = task[:age.start()].strip() if age else task.strip()
            assert title == row["title"], (label, row, line)
            assert line[2] == glyphs[row["state"]], (label, row, line)
            if span and "name" in row:
                assert_name_cell(label, line, row["name"], span)
            assert (line[0] == "■") == (row["pane_id"] in marked), (label, row, line)
            if row["pane_id"] in inert:
                assert line[0] == " ", (label, "source must have an inert gutter", line)
            if row.get("started_at") is None:
                assert age is None, (label, "unknown/neutral age must be blank", line)
            else:
                start, end = capture_times[label]
                # Frames can straddle the one-second redraw or a unit boundary.
                allowed = {compact_age(row["started_at"], now) for now in (start - 2000, end)}
                assert age and age[1] in allowed, (label, row, line, allowed)
        screen_evidence[label]["rows"] = rows

    def assert_footer(label, mode, count=0):
        content, _ = popup(label)
        footer = content[-1].strip()
        # These are the approved hints at the actual private popup widths:
        # 70 host columns leave a 40-column popup (36 inside); 140 leaves 110
        # and 420 leaves 132.
        if mode == "review":
            noun = "pane" if count == 1 else "panes"
            assert footer in (f"y kill {count} {noun}   any other key cancels",
                              "y kill   other keys cancel"), (label, footer)
        elif mode == "kill":
            assert footer in ("↑↓ move   tab/space mark   enter review   ctrl-e filter   ctrl-space preview   esc cancel",
                              "tab mark  enter review  esc cancel"), (label, footer)
        else:
            assert footer in ("↑↓ select   enter jump   ctrl-e filter   ctrl-x kill   ctrl-space preview   ctrl-r refresh   esc close",
                              "enter  ^e filter  ^x kill  esc"), (label, footer)

    def assert_view(label, view, oracle, rows=None, query=None):
        content, active = popup(label)
        views = oracle["views"]
        assert active == f"{view} {views[view]['count']}", (label, active, view, views[view]["count"])
        if len(content[0]) >= 80:
            for name, data in views.items():
                count = re.search(rf"(?<!\S){re.escape(name)} (\d+)(?!\S)", content[0])
                assert count and int(count[1]) == data["count"], (label, name, content[0])
        expected = views[view]["rows"] if rows is None else rows
        if query is not None:
            assert query in content[1], (label, content[1])
            count = re.search(r"(?<!\d)(\d+)/(\d+)\s*$", content[1])
            assert count and (int(count[1]), int(count[2])) == (len(expected), views[view]["count"]), (label, content[1])
        assert_rows(label, expected)
        assert_footer(label, "browse")

    def assert_kill(label, count):
        content, _ = popup(label)
        shown = re.search(r"\b(\d+) marked\b", content[0])
        assert content[0].startswith("Kill") and shown and int(shown[1]) == count, (label, content[0])
        assert_footer(label, "kill")

    def assert_review(label, targets):
        content, active = popup(label)
        count = len(targets)
        noun = "pane" if count == 1 else "panes"
        assert content[0].strip() == f"Kill {count} {noun}?", (label, content[0])
        assert not active, (label, "review must hide the view strip", active)
        assert content[1].startswith("Each pane closes and its running"), (label, content[1])
        assert_rows(label, targets, marked=[row["pane_id"] for row in targets])
        assert_footer(label, "review", count)

    def prepare_state_fixtures():
        """Report real host state while keeping transcript discovery inside the test HOME.

        Create older sessions first and mix the states so insertion order cannot
        accidentally satisfy the picker order. UUIDv4 IDs have no embedded start
        time: resolving these ages requires reading the Codex/Claude fixtures.
        """
        now = int(time.time())
        specifications = [
            ("idle", "old", "claude", 5 * 3600),
            ("working", "old", "codex", 2 * 3600),
            ("done", "old", "claude", 4 * 3600),
            ("blocked", "old", "codex", 3 * 3600),
            ("idle", "new", "codex", 17 * 60),
            ("working", "new", "claude", 13 * 60),
            ("done", "new", "codex", 11 * 60),
            ("blocked", "new", "claude", 7 * 60),
            ("working", "no age", "codex", None),
            ("neutral", "shell", None, None),
        ]
        for state, suffix, vendor, age in specifications:
            title = f"P3 {state} {suffix}"
            pane = cli("tab", "create", "--label", title, "--no-focus")["result"]["root_pane"]["pane_id"]
            cli("pane", "rename", pane, title)
            cli("pane", "run", pane, "sleep 300")
            fixture = {"pane_id": pane, "title": title, "state": state,
                       "agent": vendor, "session_id": None, "started_at": None}
            fixture_evidence[pane] = fixture
            if vendor is None:
                continue
            # Done is not a reportable state. A completed, unseen working ->
            # idle transition becomes done; explicitly focusing it makes idle.
            cli("pane", "report-agent", pane, "--agent", vendor, "--state", "working", "--source", "jump:p3")
            if state != "working":
                cli("pane", "report-agent", pane, "--agent", vendor,
                    "--state", "idle" if state in ("done", "idle") else state, "--source", "jump:p3")
            cli("pane", "report-metadata", pane, "--source", "jump:p3-title", "--title", title)
            if age is not None:
                session_id = str(uuid.uuid4())
                started_at = now - age - 20
                timestamp = datetime.fromtimestamp(started_at, timezone.utc).isoformat()
                if vendor == "codex":
                    transcript = root / "home/.codex/sessions/fixture" / f"rollout-{session_id}.jsonl"
                    record = {"type": "session_meta", "payload": {"id": session_id, "timestamp": timestamp}}
                else:
                    transcript = root / "home/.claude/projects/fixture" / f"{session_id}.jsonl"
                    record = {"type": "user", "sessionId": session_id, "parentUuid": None,
                              "isSidechain": False, "timestamp": timestamp}
                transcript.parent.mkdir(parents=True, exist_ok=True)
                transcript.write_text(json.dumps(record) + "\n")
                fixture.update(session_id=session_id, started_at=started_at * 1000,
                               transcript=str(transcript.relative_to(root)), record=record)
                # Herdr accepts resumable session IDs only from the vendor's
                # official source; state reports above use a separate test hook.
                cli("pane", "report-agent-session", pane, "--agent", vendor,
                    "--source", f"herdr:{vendor}", "--agent-session-id", session_id)
            if state == "idle":
                cli("agent", "focus", pane)
                origin_focus()
        frame("state-fixtures-prepared")

    def view_expectations(label, excluded=()):
        """Save a host-derived oracle, independent of the eventual screen parser.

        Counts/membership come from agent list, checked against the snapshot the
        picker consumes. Transcript starts supply age order; snapshot topology
        breaks ties. Query text must never change these catalog-wide counts.
        """
        current = snapshot()
        agent_list = cli("agent", "list")["result"]["agents"]
        evidence = {"captured_at": int(time.time() * 1000), "snapshot": current,
                    "agent_list": agent_list}
        view_evidence[label] = evidence
        agents = {agent["pane_id"]: agent for agent in agent_list if agent.get("agent")}
        panes = {pane["pane_id"]: pane for pane in current["panes"]}
        assert set(agents) == {pane["pane_id"] for pane in panes.values() if pane.get("agent")}
        for pane_id, agent in agents.items():
            assert agent["agent_status"] == panes[pane_id]["agent_status"], (label, agent, panes[pane_id])
        for pane_id, fixture in fixture_evidence.items():
            pane = panes[pane_id]
            assert pane.get("agent") == fixture["agent"], (label, pane, fixture)
            if fixture["agent"] is not None:
                assert agents[pane_id]["agent_status"] == fixture["state"], (label, agents[pane_id], fixture)
                assert agents[pane_id]["title"] == fixture["title"]
            if fixture["session_id"] is None:
                assert not pane.get("agent_session"), (label, pane)
            else:
                expected_session = {"source": f"herdr:{fixture['agent']}", "agent": fixture["agent"],
                                    "kind": "id", "value": fixture["session_id"]}
                assert pane.get("agent_session") == agents[pane_id].get("agent_session") == expected_session, (label, pane, expected_session)
        priority = {state: index for index, state in enumerate(("blocked", "done", "working", "idle", "neutral"))}
        workspaces = {workspace["workspace_id"]: workspace["number"] for workspace in current["workspaces"]}
        tabs = {tab["tab_id"]: tab["number"] for tab in current["tabs"]}
        source_workspace = panes[origin]["workspace_id"]
        # The picker names a pane by its label, else its agent's alias.
        aliases = {agent["pane_id"]: agent.get("name") for agent in current.get("agents", [])}
        rows = []
        for index, pane in enumerate(current["panes"]):
            if pane["workspace_id"] in excluded:
                continue
            pane_id = pane["pane_id"]
            state = agents[pane_id]["agent_status"] if pane_id in agents else "neutral"
            fixture = fixture_evidence.get(pane_id, {})
            rows.append({"pane_id": pane_id, "title": fixture.get("title", pane.get("title") or pane.get("label")),
                         "name": pane.get("label") or aliases.get(pane_id) or "",
                         "state": state, "started_at": fixture.get("started_at"),
                         "workspace_id": pane["workspace_id"],
                         "topology_order": [pane["workspace_id"] != source_workspace,
                                            workspaces[pane["workspace_id"]], tabs[pane["tab_id"]], index]})
        views = {}
        for view, states in [("All", set(priority)), ("In progress", {"working"}),
                             ("Unread/Blocked", {"blocked", "done"}), ("Idle", {"idle"})]:
            members = [row for row in rows if row["state"] in states]
            members.sort(key=lambda row: (priority[row["state"]] if view == "All" else 0,
                                         -(row["started_at"] or 0), row["topology_order"]))
            views[view] = {"count": len(members), "rows": members}
        evidence["views"] = views
        return evidence

    def start_policy_owner(excluded):
        """Fake only the Agents boundary; all panes, keys and painting are real.

        The owner answers one v1 read per connection using current private host
        IDs. A lock pins visibility/failure for each reply so a refresh observes
        one complete policy, even when the input thread is still editing.
        """
        agents_state.mkdir()
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(str(agents_state / "control.sock"))
        listener.listen()
        listener.settimeout(0.1)
        state = {"excluded": list(excluded), "show": False, "unavailable": False, "gates": []}
        lock = threading.Lock()

        def serve():
            with listener:
                while not policy_stop.is_set():
                    try:
                        connection, _ = listener.accept()
                    except socket.timeout:
                        continue
                    with connection:
                        connection.settimeout(3)
                        request = json.loads(connection.makefile("rb").readline())
                        assert request == {"cmd": "workspace_policy", "version": 1,
                                           "herdr_socket": host_socket}, request
                        with lock:
                            policy_evidence["requests"].append(request)
                            selected = dict(state)
                            gate = state["gates"].pop(0) if state["gates"] else None
                        if selected["unavailable"]:
                            response = {"ok": False, "code": "invalid_policy", "error": "fixture policy unavailable"}
                        else:
                            response = {"ok": True, "version": 1, "herdr_socket": host_socket,
                                        "excluded_labels": ["ft"],
                                        "workspace_ids": sorted(row["workspace_id"] for row in snapshot()["workspaces"]),
                                        "excluded_workspace_ids": sorted(selected["excluded"]),
                                        "show_excluded": selected["show"]}
                        # Freeze the response before waiting: refresh A can
                        # finish with shown policy while B is queued with hidden
                        # policy. Each wait still fits Jump's two-second budget.
                        if gate is not None:
                            assert gate.wait(1.5), "delayed policy fixture was not released"
                        connection.sendall(json.dumps(response).encode() + b"\n")

        thread = threading.Thread(target=serve, name="fake-agents-policy")
        thread.start()
        return state, lock, thread

    def assert_policy_note(label, expected):
        content, _ = popup(label)
        occurrences = [(y, line.index(expected)) for y, line in enumerate(content) if expected in line]
        assert len(occurrences) == 1, (label, expected, content)
        origin_x, origin_y = screen_evidence[label]["content_origin"]
        y, x = occurrences[0]
        for offset in range(len(expected)):
            cell = screen.buffer[origin_y + y][origin_x + x + offset]
            # Herdr emits an indexed palette; pyte records DarkGray as RGB
            # 7f7f7f, the same value as the existing dim-chip assertions.
            assert cell.fg == "7f7f7f" and not cell.bold, (label, expected, cell)
        policy_evidence["scenarios"][label] = {"note": expected, "popup": content}

    def check_policy_views(prefix, oracle, notice):
        open_picker(prefix + "-open")
        os.write(fd, b"J9 hidden")
        for index, view in enumerate(("All", "In progress", "Unread/Blocked", "Idle")):
            if index:
                os.write(fd, b"\x05")
            label = f"{prefix}-{index}"
            frame(label)
            members = [row for row in oracle["views"][view]["rows"] if row["title"].startswith("J9 hidden")]
            assert_view(label, view, oracle, rows=members, query="J9 hidden")
            if notice:
                assert_policy_note(label, notice)
            else:
                content, _ = popup(label)
                assert not any(re.search(r"\b\d+ hidden\b|Hidden list unavailable", line) for line in content[2:]), (label, content)
            assert snapshot()["focused_pane_id"] == origin
        os.write(fd, b"\x1b")
        frame(prefix + "-closed")

    try:
        frame("startup", 2)
        initial = snapshot()
        origin = initial["panes"][0]["pane_id"]
        origin_tab = initial["panes"][0]["tab_id"]
        host_socket = cli("status", "--json")["server"]["socket"]
        assert host_socket.startswith(str(root) + "/"), "Refuse non-test host socket"
        cli("plugin", "link", str(root_repo))
        installation = cli("plugin", "action", "invoke", "shadowfax.jump.install-keybindings")
        deadline = time.monotonic() + 5
        while "shadowfax.jump.open" not in config.read_text() and time.monotonic() < deadline:
            frame("install-wait", 0.1)
        cli("config", "check")
        assert "shadowfax.jump.open" in config.read_text(), (installation, cli("plugin", "log", "list", "--plugin", "shadowfax.jump"))
        checks.append("actual Alt-p installation")
        target = cli("tab", "create", "--label", "jump-shell-target", "--no-focus")["result"]["root_pane"]["pane_id"]
        other = cli("workspace", "create", "--label", "jump-workspace", "--no-focus")["result"]["root_pane"]["pane_id"]
        cli("pane", "rename", target, "inspect-jumper-shell")
        cli("pane", "rename", other, "review-jumper-agent")
        cli("pane", "run", target, "printf '\\033[2J\\033[HJUMP_SHELL_DESTINATION\\n'")
        cli("pane", "run", other, "printf '\\033[2J\\033[HJUMP_AGENT_DESTINATION\\n'")
        cli("pane", "report-agent", other, "--agent", "codex", "--state", "working", "--source", "jump:test")
        cli("pane", "report-agent", other, "--agent", "codex", "--state", "idle", "--source", "jump:test")
        cli("pane", "report-metadata", other, "--source", "jump:title", "--title", "Refactor café review")
        frame("prepared")

        open_picker("open-shell")
        checks.append("rendered popup uses 90% height and is 132 columns wide, less where the pane area needs a 2-column margin")
        os.write(fd, b"insp jmpr shll")
        frame("shell-search")
        os.write(fd, b"\x00")
        preview_text = frame("shell-preview", 0.8)
        assert "JUMP_SHELL_DESTINATION" in preview_text, preview_text[-3000:]
        assert snapshot()["focused_pane_id"] == origin
        checks.append("search accepts immediate letters; preview reads without focusing")
        os.write(fd, b"\r")
        text = frame("shell-jump")
        assert snapshot()["focused_pane_id"] == target
        assert "JUMP_SHELL_DESTINATION" in text, text[-3000:]
        checks.append("Enter shows the exact non-agent pane across tabs")

        origin_focus()
        open_picker("open-agent")
        os.write(fd, "rfctr cafe\u0301".encode())
        frame("agent-search")
        status_before_preview = next(a["agent_status"] for a in snapshot()["agents"] if a["pane_id"] == other)
        os.write(fd, b"\x00")
        text = frame("agent-preview", 0.8)
        assert "JUMP_AGENT_DESTINATION" in text, text[-3000:]
        assert snapshot()["focused_pane_id"] == origin
        assert next(a["agent_status"] for a in snapshot()["agents"] if a["pane_id"] == other) == status_before_preview
        os.write(fd, b"\r")
        text = frame("agent-jump")
        assert snapshot()["focused_pane_id"] == other
        assert "JUMP_AGENT_DESTINATION" in text, text[-3000:]
        checks.append("reported task title and decomposed Unicode find agent across workspaces; preview preserves status")

        origin_focus()
        open_picker("open-cancel")
        os.write(fd, b"rvw jmpr")
        frame("cancel-search")
        os.write(fd, b"\x1b")
        frame("cancel")
        assert snapshot()["focused_pane_id"] == origin
        # Prove the overlay actually closed, not just that pane focus stayed put.
        os.write(fd, b"printf 'JUMP_CANCEL_RETURNS_INPUT\\n'\r")
        assert "JUMP_CANCEL_RETURNS_INPUT" in frame("cancel-input")
        checks.append("Escape closes once and restores source input")

        open_picker("open-missing")
        os.write(fd, b"zzzzmissing")
        assert "No matching panes" in frame("no-matches")
        os.write(fd, b"\r")
        frame("no-match-enter")
        assert snapshot()["focused_pane_id"] == origin
        os.write(fd, b"\x15inspect-jumper-shell")
        frame("closed-search")
        cli("pane", "close", target)
        os.write(fd, b"\r\r\r")
        text = frame("closed-target")
        # The notice word-wraps between words. Join its last three possible
        # lines with a space, excluding the host and footer, so "not found"
        # matches as words; retain both the human-readable notice and its
        # exact host error/target.
        content, _ = popup("closed-target")
        notice = " ".join(line.rstrip() for line in content[-4:-1])
        assert "retry" in notice or "not found" in notice, notice
        assert "pane_not_found" in notice and target in notice, notice
        assert snapshot()["focused_pane_id"] == origin
        os.write(fd, b"\x1b")
        frame("closed-exit")
        checks.append("empty results and disappearing target never jump elsewhere")
        # Keep destructive outcomes tied to exact API identities while checking
        # the visible marks and prompts that authorize those operations.
        origin_focus()
        cli("pane", "rename", origin, "jump-protected-source")
        victims = []
        for label in ["jump-kill-amber", "jump-kill-birch", "jump-keep-cedar"]:
            pane = cli("tab", "create", "--label", label, "--no-focus")["result"]["root_pane"]["pane_id"]
            cli("pane", "rename", pane, label)
            cli("pane", "run", pane, "sleep 300")
            victims.append(pane)
        frame("kill-prepared")
        before = {p["pane_id"] for p in snapshot()["panes"]}
        kill_evidence.update(before=sorted(before), marked=sorted(victims[:2]),
                             survivor=victims[2], source=origin)
        kill_rows = [{"pane_id": pane, "title": title, "name": title, "state": "neutral"} for pane, title in
                     zip(victims, ("jump-kill-amber", "jump-kill-birch", "jump-keep-cedar"))]
        source_row = {"pane_id": origin, "title": "jump-protected-source", "name": "jump-protected-source",
                      "state": "neutral"}
        open_picker("kill-open")
        os.write(fd, b"\x0b")
        frame("ctrl-k-browse")
        content, _ = popup("ctrl-k-browse")
        assert content[0].startswith("Jump to session"), content[0]
        assert_footer("ctrl-k-browse", "browse")
        assert snapshot()["focused_pane_id"] == origin
        checks.append("ctrl-k in browse keeps Jump to session and does not enter kill mode")
        os.write(fd, b"\x18jump-protected-source\t")
        frame("kill-source-protected")
        assert_kill("kill-source-protected", 0)
        assert_rows("kill-source-protected", [source_row], inert=[origin])
        # Capture the still-visible, unmarked source before Enter. A stray y
        # after a zero-mark Enter would edit the query and hide this evidence.
        os.write(fd, b"\r")
        frame("kill-source-zero-enter")
        assert_kill("kill-source-zero-enter", 0)
        assert_rows("kill-source-zero-enter", [source_row], inert=[origin])
        assert {p["pane_id"] for p in snapshot()["panes"]} == before
        checks.append("source stays visible and inert with 0 marked after Tab; Enter cannot kill it")

        os.write(fd, b"\x15jump-kill-amber\t")
        frame("kill-first-mark")
        assert_kill("kill-first-mark", 1)
        assert_rows("kill-first-mark", kill_rows[:1], marked=victims[:1])
        os.write(fd, b"\x0b")
        frame("ctrl-k-kill")
        assert_kill("ctrl-k-kill", 1)
        assert_rows("ctrl-k-kill", kill_rows[:1], marked=victims[:1])
        assert {p["pane_id"] for p in snapshot()["panes"]} == before
        checks.append("ctrl-k in kill mode preserves the visible mark count and marked row")
        os.write(fd, b"\r")
        frame("kill-cancel-confirmation")
        assert_review("kill-cancel-confirmation", kill_rows[:1])
        os.write(fd, b"n")
        frame("kill-cancelled")
        assert_kill("kill-cancelled", 1)
        assert_rows("kill-cancelled", kill_rows[:1], marked=victims[:1])
        assert {p["pane_id"] for p in snapshot()["panes"]} == before
        checks.append("kill review names the exact mark; n closes nothing and preserves the visible mark")
        # Esc leaves kill mode and clears the canceled mark. Re-enter and mark
        # the two exact query matches independently, leaving cedar unmarked.
        os.write(fd, b"\x1b")
        frame("kill-leave-mode")
        content, _ = popup("kill-leave-mode")
        assert content[0].startswith("Jump"), content[0]
        assert_rows("kill-leave-mode", kill_rows[:1])
        assert_footer("kill-leave-mode", "browse")
        os.write(fd, b"\x18\x15jump-kill-amber\t")
        frame("kill-mark-amber")
        assert_kill("kill-mark-amber", 1)
        assert_rows("kill-mark-amber", kill_rows[:1], marked=victims[:1])
        os.write(fd, b"\x15jump-kill-birch\t")
        frame("kill-mark-birch")
        assert_kill("kill-mark-birch", 2)
        assert_rows("kill-mark-birch", kill_rows[1:2], marked=victims[:2])
        content, _ = popup("kill-mark-birch")
        if len(content[0]) >= 80:
            assert re.search(r"\b1 hidden\b", content[0]), content[0]
        os.write(fd, b"\x15jump-kill")
        frame("kill-both-marks")
        assert_kill("kill-both-marks", 2)
        assert_rows("kill-both-marks", kill_rows[:2], marked=victims[:2])
        os.write(fd, b"\ry")
        frame("kill-two-confirmation")
        assert_review("kill-two-confirmation", kill_rows[:2])
        assert {p["pane_id"] for p in snapshot()["panes"]} == before
        checks.append("typed-ahead y is discarded before confirmation is armed")
        os.write(fd, b"y")
        frame("kill-complete", 1)
        content, _ = popup("kill-complete")
        assert "Killed 2 panes" in "\n".join(content), content
        assert content[0].startswith("Jump"), content[0]
        assert_footer("kill-complete", "browse")
        after = {p["pane_id"] for p in snapshot()["panes"]}
        assert after == before - set(victims[:2]), (before, after, victims)
        assert victims[2] in after and origin in after
        kill_evidence["after"] = sorted(after)
        checks.append("confirmed kill removes exactly two marked sleep panes; third survives")
        checks.append("kill marks, counts, exact review titles, cancellation footer, result and Escape mode exit are visible")
        os.write(fd, b"\x1b")
        frame("kill-exit")

        prepare_state_fixtures()
        baseline = view_expectations("prepared")
        checks.append("private Codex/Claude transcripts and reported session IDs; working, blocked, done, idle and neutral host states")
        # Save each view's host oracle beside its terminal capture, so rendering
        # checks can compare the same state without hard-coded pane counts.
        open_picker("states-all")
        for index, view in enumerate(("All", "In progress", "Unread/Blocked", "Idle", "All")):
            label = "states-all" if index == 0 else f"states-view-{index}"
            if index:
                os.write(fd, b"\x05")
                frame(label)
            current = view_expectations(f"view-{index}-{view}")
            assert current["views"] == baseline["views"], (view, current["views"], baseline["views"])
            assert current["snapshot"]["focused_pane_id"] == origin
            assert_view(label, view, current)
        checks.append("four view keys preserve host state; API-derived counts, membership and age/topology order recorded")
        checks.append("All and filtered rows show exact state glyphs, transcript ages, blank unknown ages and host-derived order")
        checks.append("ctrl-e cycles the active header pill and counts through all four views; browse footer keeps filter and kill hints")

        os.write(fd, b"\x1b")
        frame("states-cycle-exit")
        # With no query, Enter exposes the actual first-ranked candidate through
        # pane identity. Insertion order would choose the older fixtures, so this
        # proves transcript discovery affects ranking before inspecting any age text.
        for index, view in enumerate(("All", "In progress", "Unread/Blocked", "Idle")):
            open_picker(f"states-rank-open-{index}")
            if index:
                os.write(fd, b"\x05" * index)
                frame(f"states-rank-view-{index}")
            os.write(fd, b"\r")
            frame(f"states-rank-jump-{index}")
            expected = baseline["views"][view]["rows"][0]["pane_id"]
            assert snapshot()["focused_pane_id"] == expected, (view, expected)
            origin_focus()
        checks.append("empty-query Enter selects the youngest eligible pane in all four views using transcript ages")

        # A query for a real blocked agent must not escape In progress. After
        # switching to Unread/Blocked, the same query must jump to that exact ID.
        blocked = next(fixture for fixture in fixture_evidence.values() if fixture["title"] == "P3 blocked new")
        # One fuzzy atom keeps "new" from matching an unrelated cwd field on
        # the older blocked row. Spaces in the title are still skipped fuzzily.
        blocked_query = "P3blockednew"
        open_picker("states-query-open")
        os.write(fd, b"\x05" + blocked_query.encode() + b"\r")
        frame("states-query-outside-view")
        assert snapshot()["focused_pane_id"] == origin
        assert_view("states-query-outside-view", "In progress", baseline, rows=[], query=blocked_query)
        os.write(fd, b"\x05")
        frame("states-query-inside-view")
        queried = view_expectations("queried")
        assert queried["views"] == baseline["views"]
        assert_view("states-query-inside-view", "Unread/Blocked", queried, rows=[blocked], query=blocked_query)
        os.write(fd, b"\r")
        frame("states-query-jump")
        assert snapshot()["focused_pane_id"] == blocked["pane_id"]
        checks.append("query stays inside the active view, preserves counts and jumps to the exact matching ID")
        origin_focus()

        shell = next(fixture for fixture in fixture_evidence.values() if fixture["state"] == "neutral")
        open_picker("states-neutral-open")
        os.write(fd, b"P3 neutral shell")
        for index in range(1, 4):
            os.write(fd, b"\x05\r")
            label = f"states-neutral-excluded-{index}"
            frame(label)
            assert snapshot()["focused_pane_id"] == origin
            assert_view(label, ("All", "In progress", "Unread/Blocked", "Idle")[index],
                        baseline, rows=[], query=shell["title"])
        os.write(fd, b"\x05\r")
        frame("states-neutral-all-jump")
        assert snapshot()["focused_pane_id"] == shell["pane_id"]
        checks.append("neutral shell is excluded from all three filtered views and remains reachable in All")
        origin_focus()

        # Refresh sees a newly created target; no navigation happens until the
        # following Enter. This exercises the real shortcut and read worker.
        open_picker("refresh-open")
        os.write(fd, b"P3 refresh target")
        frame("refresh-before-create")
        added = cli("tab", "create", "--label", "P3 refresh target", "--no-focus")["result"]["root_pane"]["pane_id"]
        cli("pane", "rename", added, "P3 refresh target")
        os.write(fd, b"\x12")
        frame("refresh-after-create")
        assert snapshot()["focused_pane_id"] == origin
        refreshed = view_expectations("refreshed")
        added_row = next(row for row in refreshed["views"]["All"]["rows"] if row["pane_id"] == added)
        assert_view("refresh-after-create", "All", refreshed, rows=[added_row], query="P3 refresh target")
        os.write(fd, b"\r")
        frame("refresh-jump")
        assert snapshot()["focused_pane_id"] == added
        checks.append("ctrl-r discovers a new pane without focusing; Enter then jumps to its exact ID")

        # Search fixtures come last so they cannot alter the existing empty
        # query/view/kill oracles or push those rows off a narrow popup.
        origin_focus()
        search_fixtures = prepare_search_fixtures()
        checks.append("private search fixtures report distinct titles, labels, tabs and HOME-relative cwd paths")
        searches = [
            ("vim", [search_fixtures[key] for key in ("title", "name", "tab")]),
            ("skl/library", [dict(search_fixtures["name"], field="path", matched="skl/library")]),
            ("vim-html", [dict(search_fixtures["name"], matched="vim-html")]),
        ]
        for query, expected in searches:
            label = "search-" + query.replace("/", "-")
            open_picker(label + "-open")
            os.write(fd, query.encode())
            frame(label)
            assert_search(label, query, expected)
            assert snapshot()["focused_pane_id"] == origin
            checks.append(f"{query}: exact ranked rows; yellow matches in title, tab or Name column, else gray Name/Path chips; "
                          "unnamed rows blank; details repeat the name only without a Name column")
            os.write(fd, b"\r")
            frame(label + "-jump")
            assert snapshot()["focused_pane_id"] == expected[0]["pane_id"], (query, expected)
            origin_focus()
        checks.append("Enter from each search jumps to its exact first-ranked pane")

        # Workspace visibility comes last, keeping the old query/order/kill
        # fixtures intact. Every new probe lives in this isolated named server.
        hidden_workspace = cli("workspace", "create", "--label", "ft", "--no-focus")["result"]["root_pane"]
        excluded = hidden_workspace["workspace_id"]
        hidden_panes = []
        for index, state in enumerate(("working", "blocked", "done", "idle", "neutral")):
            pane = hidden_workspace["pane_id"] if index == 0 else cli("tab", "create", "--workspace", excluded,
                "--label", "J9 hidden " + state, "--no-focus")["result"]["root_pane"]["pane_id"]
            title = "J9 hidden " + state
            cli("pane", "rename", pane, title)
            cli("pane", "run", pane, "sleep 300")
            if state != "neutral":
                # Herdr derives done from an unseen working->idle completion;
                # it is not a reportable state. Focus only the private idle
                # fixture to distinguish it from an unread completion.
                cli("pane", "report-agent", pane, "--agent", "codex", "--state", "working", "--source", "jump:policy")
                if state != "working":
                    cli("pane", "report-agent", pane, "--agent", "codex", "--state",
                        "idle" if state in ("done", "idle") else state, "--source", "jump:policy")
                if state == "idle":
                    cli("agent", "focus", pane)
                    origin_focus()
            cli("pane", "report-metadata", pane, "--title", title, "--source", "jump:policy-title")
            hidden_panes.append(pane)
        origin_focus()
        policy_state, policy_lock, policy_thread = start_policy_owner([excluded])
        hidden_oracle = view_expectations("policy-hidden-oracle", excluded=[excluded])
        before = len(policy_evidence["requests"])
        check_policy_views("policy-hidden", hidden_oracle, "5 hidden")
        assert len(policy_evidence["requests"]) == before + 1
        checks.append("v1 policy hides excluded panes from all four views, API-derived counts and typed search; one dim hidden count")

        with policy_lock:
            policy_state["show"] = True
        shown_oracle = view_expectations("policy-shown-oracle")
        check_policy_views("policy-shown", shown_oracle, None)
        checks.append("show_excluded=true restores the same panes in every applicable view, count and search")

        # An in-flight shown response must not clear loading for a newer hidden
        # read. Exercise the real coalescing worker and a fresh y while the
        # latest read is held; a wrong implementation would leave Confirm.
        open_picker("policy-overlap-open")
        os.write(fd, b"J9 hidden neutral\x18\t")
        frame("policy-overlap-marked")
        assert_kill("policy-overlap-marked", 1)
        gate_a, gate_b = threading.Event(), threading.Event()
        policy_gates.extend((gate_a, gate_b))
        with policy_lock:
            policy_state["gates"] = [gate_a, gate_b]
        before = len(policy_evidence["requests"])
        os.write(fd, b"\x12")
        frame("policy-overlap-A", 0.15)
        assert len(policy_evidence["requests"]) == before + 1
        with policy_lock:
            policy_state["show"] = False
        os.write(fd, b"\x12\r")
        frame("policy-overlap-B-queued", 0.15)
        gate_a.set()
        deadline = time.monotonic() + 0.6
        while len(policy_evidence["requests"]) < before + 2 and time.monotonic() < deadline:
            frame("policy-overlap-wait-B", 0.05)
        assert len(policy_evidence["requests"]) == before + 2
        frame("policy-overlap-stale-completed", 0.15)
        os.write(fd, b"y")
        frame("policy-overlap-y-unarmed", 0.15)
        neutral = next(row for row in shown_oracle["views"]["All"]["rows"] if row["title"] == "J9 hidden neutral")
        assert_review("policy-overlap-y-unarmed", [neutral])
        assert set(hidden_panes) <= {pane["pane_id"] for pane in snapshot()["panes"]}
        gate_b.set()
        frame("policy-overlap-latest-completed")
        assert_kill("policy-overlap-latest-completed", 0)
        assert_policy_note("policy-overlap-latest-completed", "5 hidden")
        assert_rows("policy-overlap-latest-completed", [])
        # Send Escape separately so terminal decoding produces two key events:
        # first leave kill mode, then close the popup.
        os.write(fd, b"\x1b")
        frame("policy-overlap-browse")
        assert_footer("policy-overlap-browse", "browse")
        os.write(fd, b"\x1b")
        frame("policy-overlap-closed")
        checks.append("overlapping delayed refreshes discard the older shown result; y stays unarmed until the latest hidden policy revokes its target")

        with policy_lock:
            policy_state["show"] = True
        open_picker("policy-marks-open")
        os.write(fd, b"J9 hidden neutral\x18\t")
        frame("policy-marked")
        assert_kill("policy-marked", 1)
        with policy_lock:
            policy_state["show"] = False
        before = len(policy_evidence["requests"])
        os.write(fd, b"\x12\r")
        frame("policy-hidden-refresh")
        assert len(policy_evidence["requests"]) == before + 1
        assert_kill("policy-hidden-refresh", 0)
        assert_policy_note("policy-hidden-refresh", "5 hidden")
        assert_rows("policy-hidden-refresh", [])
        os.write(fd, b"y\r")
        frame("policy-hidden-no-kill")
        assert_kill("policy-hidden-no-kill", 0)
        assert set(hidden_panes) <= {pane["pane_id"] for pane in snapshot()["panes"]}
        os.write(fd, b"\x1b")
        frame("policy-marks-browse")
        assert_footer("policy-marks-browse", "browse")
        os.write(fd, b"\x1b")
        frame("policy-marks-closed")
        checks.append("Ctrl-r rereads policy; newly hidden marked pane leaves the review and y closes none")

        with policy_lock:
            policy_state["unavailable"] = True
        check_policy_views("policy-unavailable", shown_oracle, "Hidden list unavailable")
        checks.append("unavailable policy fails open with all view counts/search rows restored and one dim notice")

        # The invoking pane gets no exemption from the shared workspace policy.
        cli("pane", "rename", origin, "J9 invoking source")
        cli("pane", "report-metadata", origin, "--title", "J9 invoking source", "--source", "jump:policy-title")
        source_workspace = initial["panes"][0]["workspace_id"]
        with policy_lock:
            policy_state.update(unavailable=False, excluded=[source_workspace, excluded])
        source_oracle = view_expectations("policy-source-oracle", excluded=[source_workspace, excluded])
        hidden_count = sum(pane["workspace_id"] in (source_workspace, excluded) for pane in snapshot()["panes"])
        open_picker("policy-source-open")
        os.write(fd, b"J9 invoking source")
        frame("policy-source-hidden")
        assert_view("policy-source-hidden", "All", source_oracle, rows=[], query="J9 invoking source")
        assert_policy_note("policy-source-hidden", f"{hidden_count} hidden")
        os.write(fd, b"\x1b")
        frame("policy-source-closed")
        checks.append("the invoking source pane is hidden too; no source-workspace rows contribute to counts")
        print(json.dumps({"width":args.width,"height":args.height,"passed":checks}, indent=2))
    finally:
        if args.evidence:
            args.evidence.mkdir(parents=True, exist_ok=True)
            for label, raw in transcripts.items():
                (args.evidence / f"{args.width}-{label}.ansi").write_bytes(raw)
                (args.evidence / f"{args.width}-{label}.txt").write_text(screens[label])
            (args.evidence / f"{args.width}-results.json").write_text(json.dumps(checks, indent=2))
            (args.evidence / f"{args.width}-kill-identities.json").write_text(json.dumps(kill_evidence, indent=2))
            (args.evidence / f"{args.width}-state-fixtures.json").write_text(json.dumps(fixture_evidence, indent=2))
            (args.evidence / f"{args.width}-view-expectations.json").write_text(json.dumps(view_evidence, indent=2))
            (args.evidence / f"{args.width}-search-fixtures.json").write_text(json.dumps(search_evidence, indent=2))
            (args.evidence / f"{args.width}-screen-assertions.json").write_text(json.dumps(screen_evidence, indent=2, ensure_ascii=False))
            (args.evidence / f"{args.width}-policy.json").write_text(json.dumps(policy_evidence, indent=2))
            (args.evidence / f"{args.width}-session.ansi").write_bytes(bytes(session_bytes))
            (args.evidence / f"{args.width}-frames.json").write_text(json.dumps(
                {"width": args.width, "height": args.height, "frames": frame_ends}, indent=2))
        policy_stop.set()
        for gate in policy_gates:
            gate.set()
        if policy_thread is not None:
            policy_thread.join(timeout=4)
            assert not policy_thread.is_alive(), "fake policy owner did not stop"
        # Only this harness's isolated server and PTY are owned for cleanup.
        try:
            subprocess.run([herdr, "--session", session, "server", "stop"], env=env, capture_output=True, timeout=8)
        finally:
            os.close(fd)
            os.waitpid(pid, 0)
