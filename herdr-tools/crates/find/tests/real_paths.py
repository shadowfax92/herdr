"""Opt-in real acceptance for Find's paths picker in a private Herdr server.

It presses Alt-n, which the private Herdr config binds to
shadowfax.tools.find-paths as the owner's does (keys live only in config.toml).

Usage (from herdr-tools/): just bin && uv run --with pyte==0.8.2 python crates/find/tests/real_paths.py [--width 140] [--height 40] [--evidence /absolute/directory]
It lays out shadowfax.tools from target/release (what `just install` lays out)
under the private XDG_DATA_HOME and links it there; zoxide and fish must be on
PATH. Everything is private: HOME, Herdr config (sound off), server socket,
zoxide database, and the clipboard (HERDR_JUMP_COPY_COMMAND writes a file). The
owner's server, clipboard and zoxide data are never touched.
"""
import argparse
import fcntl
import json
import os
from pathlib import Path
import pty
import pyte
import select
import shutil
import struct
import subprocess
import tempfile
import termios
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--width", type=int, default=140)
parser.add_argument("--height", type=int, default=40)
parser.add_argument("--evidence", type=Path)
args = parser.parse_args()
herdr_tools = Path(__file__).resolve().parents[3]
binary = herdr_tools / "target/release/herdr-tools"
herdr, zoxide, fish = shutil.which("herdr"), shutil.which("zoxide"), shutil.which("fish")
assert herdr and zoxide and fish and binary.is_file(), (herdr, zoxide, fish, binary)
session = f"jump-paths-{os.getpid()}"
checks = []
paths_key = "alt+n"
key_bytes = b"\x1b" + paths_key[4:].encode()  # what a terminal sends for Alt-<key>

with tempfile.TemporaryDirectory(prefix="jump-paths-", dir="/tmp") as directory:
    root = Path(directory)
    home = root / "home"
    clip = root / "clipboard"
    config = root / "config.toml"
    # The owner wants no sounds, ever: off in config and in the environment.
    config.write_text('onboarding = false\n[terminal]\ndefault_shell = "/bin/sh"\n[ui.sound]\nenabled = false\n\n'
                      f'[[keys.command]]\nkey = "{paths_key}"\ntype = "plugin_action"\n'
                      'command = "shadowfax.tools.find-paths"\ndescription = "Paste a zoxide path"\n')
    env = {k: v for k, v in os.environ.items() if not k.startswith("HERDR_")}
    env.pop("NO_COLOR", None)
    env.update(HOME=str(home), CFFIXED_USER_HOME=str(home),
               XDG_CONFIG_HOME=str(root / "config"), XDG_STATE_HOME=str(root / "state"),
               XDG_DATA_HOME=str(root / "data"), XDG_RUNTIME_DIR=str(root / "run"),
               HERDR_CONFIG_PATH=str(config), TERM="xterm-256color",
               HERDR_DISABLE_SOUND="1",
               _ZO_DATA_DIR=str(root / "zoxide"),
               HERDR_JUMP_COPY_COMMAND=f"cat > '{clip}'")

    # zoxide ranks by frecency: more visits rank higher.
    dirs = {"herdr": home / "code/herdr-jump",
            "worktree": home / "code/herdr-jump/.wt/feat/jump-zoxide",
            "drive": home / "My Drive/reports",
            "notes": home / "notes"}
    for name, visits in (("herdr", 4), ("notes", 3), ("worktree", 2), ("drive", 1)):
        dirs[name].mkdir(parents=True)
        for _ in range(visits):
            subprocess.run([zoxide, "add", str(dirs[name])], env=env, check=True)
    ranked = subprocess.run([zoxide, "query", "--list"], env=env, check=True,
                            capture_output=True, text=True).stdout.split("\n")[:-1]
    expected_rows = ["~/" + str(Path(p).relative_to(home)) for p in ranked]

    plugin_dir = root / "data/herdr/plugins/shadowfax.tools"

    def lay_out_plugin():
        """What `just install` lays out, from the prebuilt release binary."""
        (plugin_dir / "bin").mkdir(parents=True)
        shutil.copy2(herdr_tools / "herdr-plugin.toml", plugin_dir)
        shutil.copy2(binary, plugin_dir / "bin/herdr-tools")
        return plugin_dir

    def cli(*command):
        result = subprocess.run([herdr, "--session", session, *command], env=env,
                                capture_output=True, text=True, timeout=12)
        assert result.returncode == 0, (command, result.stdout, result.stderr)
        return json.loads(result.stdout) if result.stdout.lstrip().startswith("{") else result.stdout

    pid, fd = pty.fork()
    if pid == 0:
        os.execve(herdr, [herdr, "--session", session], env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", args.height, args.width, 0, 0))
    transcripts, screens, observed = {}, {}, {}
    # The whole PTY stream plus where each frame ended, so evidence can be
    # replayed into exact (colored) screens; per-label transcripts can repeat.
    session_bytes, frame_ends = bytearray(), []

    class Screen(pyte.Screen):
        def report_device_status(self, mode, **kwargs):
            if not kwargs.get("private"):
                super().report_device_status(mode)

    screen = Screen(args.width, args.height)
    stream = pyte.ByteStream(screen)

    def frame(label, seconds=0.6):
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
        screens[label] = "\n".join(screen.display)
        frame_ends.append([label, len(session_bytes)])
        return screens[label]

    def popup(label):
        """The picker's own rows, cropped inside the host's popup border."""
        lines = screens[label].splitlines()
        top = next((i for i, line in enumerate(lines) if "┌Find" in line), None)
        if top is None:
            return None
        left = lines[top].index("┌Find")
        right = lines[top].index("┐", left)
        bottom = next(i for i in range(top + 1, len(lines)) if lines[i][left:left + 1] == "└")
        content = [line[left + 2:right - 1].rstrip() for line in lines[top + 1:bottom]]
        observed[label] = content
        return content

    def rows(content):
        return [line[2:] for line in content[3:] if line.startswith(("› ", "■ ", "  ")) and line[2:3] in "~/…"]

    def same_rows(observed, expected):
        """Narrow popups elide leading directories (`…/feat/jump-zoxide`), keeping the leaf."""
        return len(observed) == len(expected) and all(
            seen == want or (seen.startswith("…") and want.endswith(seen[1:]))
            for seen, want in zip(observed, expected))

    def pane_lines(pane, source="recent-unwrapped"):
        # Unwrapped, so a long prompt line in a narrow pane reads as one line.
        text = cli("pane", "read", pane, "--source", source)
        return [line.rstrip() for line in text.splitlines() if line.strip()]

    def clipboard():
        return clip.read_text() if clip.exists() else None

    def open_paths(label):
        os.write(fd, key_bytes)
        frame(label, 1.0)
        content = popup(label)
        assert content and content[0].startswith("Paths"), screens[label][-3000:]
        return content

    try:
        frame("startup", 2)
        origin = cli("api", "snapshot")["result"]["snapshot"]["panes"][0]["pane_id"]
        host_socket = cli("status", "--json")["server"]["socket"]
        assert host_socket.startswith(str(root) + "/"), "Refuse non-test host socket"
        cli("plugin", "link", str(lay_out_plugin()))
        linked = next(p for p in cli("plugin", "list", "--json")["result"]["plugins"]
                      if p["plugin_id"] == "shadowfax.tools")
        assert Path(linked["plugin_root"]).resolve() == plugin_dir.resolve() and linked["enabled"], linked
        cli("server", "reload-config")
        cli("config", "check")
        frame("prepared")
        checks.append(f"laid-out shadowfax.tools linked; {paths_key} bound to shadowfax.tools.find-paths in config.toml")

        content = open_paths("open")
        assert content[0].endswith(f"zoxide · {len(expected_rows)}"), content[0]
        assert content[1] == "› Type part of a path…", content[1]
        assert same_rows(rows(content), expected_rows), (rows(content), expected_rows)
        # Hints drop whole pairs as the popup narrows (70 host columns ≈ 40 here).
        assert content[-1].strip() in ("↑↓ select   tab mark   enter paste + copy   ctrl-r reload   esc close",
                                       "↑↓ select  tab mark  enter paste + copy  ^r reload  esc close",
                                       "tab mark  enter paste + copy  esc close",
                                       "enter paste + copy  esc close"), content[-1]
        checks.append(f"{paths_key} lists every zoxide path in zoxide's order, with ~ for HOME")

        os.write(fd, b"herdr jump")
        frame("search")
        content = popup("search")
        assert content, screens["search"][-3000:]
        assert same_rows(rows(content), ["~/code/herdr-jump", "~/code/herdr-jump/.wt/feat/jump-zoxide"]), content
        assert content[1].endswith(f"2/{len(expected_rows)}"), content[1]
        assert content[3].startswith("› ~/code/herdr-jump"), content[3]
        os.write(fd, b"\r")
        frame("typed", 1.0)
        assert popup("typed") is None, "popup closes after Enter"
        assert clipboard() == str(dirs["herdr"]), clipboard()
        last = pane_lines(origin)[-1]
        assert last.endswith(str(dirs["herdr"])) and "$" in last, pane_lines(origin)
        checks.append("a fragment finds the path; Enter types it at the prompt without running it and copies it")

        os.write(fd, b"\x15")  # Ctrl-u clears the shell prompt for the next case.
        frame("cleared")
        clip.unlink()
        open_paths("cancel-open")
        os.write(fd, b"notes")
        frame("cancel-search")
        os.write(fd, b"\x1b")
        frame("cancel", 1.0)
        assert popup("cancel") is None
        assert clipboard() is None, "Escape copies nothing"
        assert pane_lines(origin)[-1].endswith("$"), pane_lines(origin)
        os.write(fd, b"printf 'PATHS_ESC_RETURNS_INPUT\\n'\r")
        assert "PATHS_ESC_RETURNS_INPUT" in frame("cancel-input")
        checks.append("Escape closes with nothing typed or copied, and input returns to the pane")

        os.write(fd, b"cd ")
        frame("cd-typed")
        open_paths("space-open")
        os.write(fd, b"drive")
        frame("space-search")
        os.write(fd, b"\r")
        frame("space-typed", 1.0)
        quoted = f"cd '{dirs['drive']}'"
        assert pane_lines(origin)[-1].endswith(quoted), pane_lines(origin)
        assert clipboard() == str(dirs["drive"]), clipboard()
        os.write(fd, b"\r")
        frame("space-cd")
        os.write(fd, b"pwd\r")
        frame("space-pwd")
        assert str(dirs["drive"]) in pane_lines(origin), pane_lines(origin)
        checks.append("a path with a space is typed shell-quoted (cd + pwd round-trips) and copied raw")

        # The owner's multi-select flow: search, Tab, search again, Tab, plus a
        # Shift-Tab on the full list, then one Enter. printf brackets each
        # argument it receives, which proves the quoting and the order.
        os.write(fd, b"printf '[%s]\\n' ")
        frame("multi-prefix")
        clip.unlink()
        open_paths("multi-open")
        os.write(fd, b"drive")
        frame("multi-drive")
        os.write(fd, b"\t")
        frame("multi-drive-marked")
        os.write(fd, b"\x15notes")
        frame("multi-notes")
        os.write(fd, b"\t")
        frame("multi-notes-marked")
        content = popup("multi-notes-marked")
        assert content and content[0].startswith("Paths · 2 marked"), content
        assert "■ ~/notes" in content, content
        os.write(fd, b"\x15")
        frame("multi-all")
        worktree_row = ranked.index(str(dirs["worktree"]))
        os.write(fd, b"\x1b[B" * worktree_row)
        frame("multi-down")
        os.write(fd, b"\x1b[Z")  # Shift-Tab: mark the worktree, move up
        frame("multi-back")
        content = popup("multi-back")
        assert content and content[0].startswith("Paths · 3 marked"), content
        # Marked rows in list (zoxide) order; narrow popups elide their parents.
        marked = [line[2:] for line in content[3:] if line.startswith("■ ")]
        three = {"~/" + str(dirs[n].relative_to(home)) for n in ("drive", "notes", "worktree")}
        want_marked = [row for row in expected_rows if row in three]
        assert same_rows(marked, want_marked), (marked, want_marked)
        typed_all = f"'{dirs['drive']}' {dirs['notes']} {dirs['worktree']}"
        assert content[-3].strip().startswith(typed_all[:20]), content[-3]  # the typed-text line
        os.write(fd, b"\r")
        frame("multi-typed", 1.0)
        assert popup("multi-typed") is None, "popup closes after Enter"
        assert pane_lines(origin)[-1].endswith(f"printf '[%s]\\n' {typed_all}"), pane_lines(origin)
        assert clipboard() == typed_all, clipboard()
        os.write(fd, b"\r")
        frame("multi-run")
        out = pane_lines(origin)
        want = [f"[{dirs[n]}]" for n in ("drive", "notes", "worktree")]
        start = out.index(want[0]) if want[0] in out else -1
        assert start >= 0 and out[start:start + 3] == want, out
        checks.append("Tab, a new search, Tab and Shift-Tab mark three paths; one Enter types them in mark "
                      "order, space-separated and quoted (printf gets 3 args), and copies the same text")

        os.write(fd, f"exec {fish} --no-config\r".encode())
        frame("fish", 2)
        os.write(fd, b"fish_vi_key_bindings\r")
        frame("fish-vi", 1.5)
        os.write(fd, b"\x1b")  # vi normal mode: raw letters here would be commands.
        frame("fish-normal", 1.0)
        open_paths("fish-open")
        os.write(fd, b"jump-zox")
        frame("fish-search")
        os.write(fd, b"\r")
        frame("fish-typed", 1.5)
        line = pane_lines(origin)[-1]
        assert str(dirs["worktree"]) in line, pane_lines(origin)
        assert clipboard() == str(dirs["worktree"]), clipboard()
        checks.append("fish in vi normal mode receives the path as text (bracketed paste), not as keys")

        probe = cli("tab", "create", "--label", "paths-entrypoint", "--no-focus")["result"]["root_pane"]["pane_id"]
        empty = root / "empty-zoxide"
        empty.mkdir()
        for label, extra, needle in (
            ("missing-zoxide", "PATH=/usr/bin:/bin", "zoxide isn't installed or isn't on Herdr's PATH"),
            ("empty-zoxide", f"_ZO_DATA_DIR={empty}", "zoxide has no directories yet. cd around and they appear here."),
        ):
            cli("pane", "run", probe, f"env {extra} HERDR_JUMP_PICKER=paths HERDR_JUMP_SOURCE_PANE={probe} {binary} find picker")
            # Messages wrap between words in a narrow pane; compare joined text.
            def shows(lines):
                return needle in " ".join(line.strip() for line in lines)
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline and not shows(pane_lines(probe, "visible")):
                time.sleep(0.2)
            observed[label] = pane_lines(probe, "visible")
            assert shows(observed[label]), observed[label]
            cli("pane", "send-keys", probe, "Escape")
            time.sleep(0.5)
        checks.append("missing zoxide and an empty database explain themselves")
        print(json.dumps({"width": args.width, "height": args.height, "passed": checks}, indent=2))
    finally:
        if args.evidence:
            args.evidence.mkdir(parents=True, exist_ok=True)
            for label, raw in transcripts.items():
                (args.evidence / f"paths-{args.width}-{label}.ansi").write_bytes(raw)
                (args.evidence / f"paths-{args.width}-{label}.txt").write_text(screens[label])
            (args.evidence / f"paths-{args.width}-results.json").write_text(json.dumps(checks, indent=2))
            (args.evidence / f"paths-{args.width}-session.ansi").write_bytes(bytes(session_bytes))
            (args.evidence / f"paths-{args.width}-frames.json").write_text(json.dumps(
                {"width": args.width, "height": args.height, "frames": frame_ends}, indent=2))
            (args.evidence / f"paths-{args.width}-observed.json").write_text(
                json.dumps({"zoxide_order": ranked, "screens": observed}, indent=2, ensure_ascii=False))
        # Only this harness's isolated server and PTY are owned for cleanup.
        try:
            subprocess.run([herdr, "--session", session, "server", "stop"], env=env, capture_output=True, timeout=8)
        finally:
            os.close(fd)
            os.waitpid(pid, 0)
