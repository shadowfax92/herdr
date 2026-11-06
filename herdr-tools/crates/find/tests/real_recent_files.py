"""Opt-in real acceptance for Find's recent-files list in a private Herdr server.

Recent files are the Ctrl-o list of the Alt-n paths popup. It presses Alt-n
(shadowfax.tools.find-paths) and Ctrl-o, and Alt-Shift-n
(shadowfax.tools.find-recent-files, the same popup opened on files); keys live
only in the private config.toml. It checks the rows against a private
recent-files log and zoxide database, that Enter types a file's path, that
Ctrl-o goes back to directories, and that a slow recent-files never delays the
directories.

Usage (from herdr-tools/): just bin && uv run --with pyte==0.8.2 python crates/find/tests/real_recent_files.py [--width 140] [--height 40]
It lays out shadowfax.tools from target/release under a private XDG_DATA_HOME
and links it there. Everything is private: HOME, Herdr config (sound off),
server socket, the recent-files log, the zoxide database, and the clipboard
(HERDR_JUMP_COPY_COMMAND writes a file). recent-files has no data-dir flag; it
keeps its log under $HOME, so the private HOME holds the fixture log and the
owner's is never read.

The server's PATH has every directory holding a recent-files removed, and the
private ~/go/bin holds a wrapper around the real CLI (a stub with the same
`list` contract when none is installed), so the rows also prove the picker
finds a CLI that is off Herdr's PATH, as `go install` leaves it. The wrapper
sleeps while $HOME/slow exists, to show the directories don't wait for it.
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
args = parser.parse_args()
herdr_tools = Path(__file__).resolve().parents[3]
binary = herdr_tools / "target/release/herdr-tools"
herdr, zoxide = shutil.which("herdr"), shutil.which("zoxide")
assert herdr and zoxide and binary.is_file(), (herdr, zoxide, binary)
real_cli = shutil.which("recent-files") or next(
    (str(p) for p in [Path.home() / "go/bin/recent-files"] if os.access(p, os.X_OK)), None)
session = f"find-recent-{os.getpid()}"
checks = []
key = "alt+shift+n"
key_bytes = b"\x1bN"  # what a legacy terminal sends for Alt-Shift-n
paths_key, paths_bytes = "alt+n", b"\x1bn"
ctrl_o = b"\x0f"
slow_seconds = 3

with tempfile.TemporaryDirectory(prefix="find-recent-", dir="/tmp") as directory:
    root = Path(directory).resolve()
    home = root / "home"
    clip = root / "clipboard"
    config = root / "config.toml"
    config.write_text('onboarding = false\n[terminal]\ndefault_shell = "/bin/sh"\n[ui.sound]\nenabled = false\n\n'
                      f'[[keys.command]]\nkey = "{key}"\ntype = "plugin_action"\n'
                      'command = "shadowfax.tools.find-recent-files"\ndescription = "Paste a recent file"\n\n'
                      f'[[keys.command]]\nkey = "{paths_key}"\ntype = "plugin_action"\n'
                      'command = "shadowfax.tools.find-paths"\ndescription = "Paste a path"\n')
    # No directory on the server's PATH may hold a recent-files: the CLI must be
    # found through the private ~/go/bin fallback.
    path = [d for d in os.environ["PATH"].split(":") if d and not (Path(d) / "recent-files").exists()]
    env = {k: v for k, v in os.environ.items() if not k.startswith("HERDR_")}
    env.pop("NO_COLOR", None)
    env.update(HOME=str(home), CFFIXED_USER_HOME=str(home), PATH=":".join(path),
               XDG_CONFIG_HOME=str(root / "config"), XDG_STATE_HOME=str(root / "state"),
               XDG_DATA_HOME=str(root / "data"), XDG_RUNTIME_DIR=str(root / "run"),
               HERDR_CONFIG_PATH=str(config), TERM="xterm-256color",
               HERDR_DISABLE_SOUND="1",
               _ZO_DATA_DIR=str(root / "zoxide"),
               HERDR_JUMP_COPY_COMMAND=f"cat > '{clip}'")

    def install_cli(into):
        """Puts a recent-files in `into`/go/bin: the real CLI, else a stub,
        behind a wrapper that sleeps while `into`/slow exists."""
        bin_dir = into / "go/bin"
        bin_dir.mkdir(parents=True)
        cli_path = real_cli
        if not cli_path:
            # The CLI's `list` contract: newest first, one path per line.
            stub = bin_dir / "recent-files-stub"
            stub.write_text(
                '#!/bin/sh\n[ "$*" = "list --days 90" ] || exit 2\n'
                'log="$HOME/.local/share/recent-files/history.tsv"\n'
                '[ -f "$log" ] || exit 0\nsort -t "$(printf \'\\t\')" -k1,1nr "$log" | cut -f2-\n')
            stub.chmod(0o755)
            cli_path = str(stub)
        (bin_dir / "recent-files").write_text(
            f'#!/bin/sh\nif [ -e "$HOME/slow" ]; then sleep {slow_seconds}; fi\nexec \'{cli_path}\' "$@"\n')
        (bin_dir / "recent-files").chmod(0o755)

    # The log, oldest line first as the CLI appends; `gone.txt` was deleted.
    files = {"todo": home / "notes/todo.md", "main": home / "code/app/main.rs",
             "report": home / "My Drive/report final.txt", "gone": home / "gone.txt",
             "old": home / "code/app/old.rs"}
    now = int(time.time())
    log = home / ".local/share/recent-files/history.tsv"
    log.parent.mkdir(parents=True)
    ages = {"todo": 10, "main": 20, "report": 30, "gone": 40, "old": 50}
    log.write_text("".join(f"{now - ages[n]}\t{files[n]}\n" for n in sorted(ages, key=ages.get, reverse=True)))
    for name, file in files.items():
        if name != "gone":
            file.parent.mkdir(parents=True, exist_ok=True)
            file.write_text(name)
    install_cli(home)
    newest_first = ["todo", "main", "report", "old"]
    expected_rows = ["~/" + str(files[n].relative_to(home)) for n in newest_first]
    # The directories list: zoxide ranks by frecency, more visits first.
    dirs = {"app": home / "code/app", "notes": home / "notes"}
    for name, visits in (("app", 3), ("notes", 1)):
        for _ in range(visits):
            subprocess.run([zoxide, "add", str(dirs[name])], env=env, check=True)
    ranked = subprocess.run([zoxide, "query", "--list"], env=env, check=True,
                            capture_output=True, text=True).stdout.split("\n")[:-1]
    dir_rows = ["~/" + str(Path(p).relative_to(home)) for p in ranked]
    assert dir_rows == ["~/code/app", "~/notes"], dir_rows

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
    screens = {}

    class Screen(pyte.Screen):
        def report_device_status(self, mode, **kwargs):
            if not kwargs.get("private"):
                super().report_device_status(mode)

    screen = Screen(args.width, args.height)
    stream = pyte.ByteStream(screen)

    def frame(label, seconds=0.6):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            ready, _, _ = select.select([fd], [], [], max(0, deadline - time.monotonic()))
            if ready:
                try:
                    chunk = os.read(fd, 262144)
                except OSError:
                    break
                if not chunk:
                    break
                stream.feed(chunk)
                if b"\x1b[6n" in chunk:
                    os.write(fd, b"\x1b[1;1R")
        screens[label] = "\n".join(screen.display)
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
        return [line[left + 2:right - 1].rstrip() for line in lines[top + 1:bottom]]

    def rows(content):
        return [line[2:] for line in content[3:] if line.startswith(("› ", "■ ", "  ")) and line[2:3] in "~/…"]

    def pane_lines(pane, source="recent-unwrapped"):
        text = cli("pane", "read", pane, "--source", source)
        return [line.rstrip() for line in text.splitlines() if line.strip()]

    def clipboard():
        return clip.read_text() if clip.exists() else None

    def open_recent(label):
        """Alt-Shift-n: the paths popup, already on files."""
        os.write(fd, key_bytes)
        frame(label, 1.0)
        content = popup(label)
        assert content and content[0].startswith("Paths") and content[0].endswith("recent-files"), \
            screens[label][-3000:]
        return content

    def press(data, label, seconds=0.6):
        os.write(fd, data)
        frame(label, seconds)
        content = popup(label)
        assert content, screens[label][-3000:]
        return content

    try:
        frame("startup", 2)
        origin = cli("api", "snapshot")["result"]["snapshot"]["panes"][0]["pane_id"]
        host_socket = cli("status", "--json")["server"]["socket"]
        assert host_socket.startswith(str(root) + "/"), "Refuse non-test host socket"
        cli("plugin", "link", str(lay_out_plugin()))
        cli("server", "reload-config")
        cli("config", "check")
        frame("prepared")
        checks.append(f"{paths_key} bound to shadowfax.tools.find-paths and {key} to "
                      "shadowfax.tools.find-recent-files; recent-files "
                      f"{'linked from ' + real_cli if real_cli else 'stubbed (not installed)'}")

        # Alt-n opens on directories, counting both lists; Ctrl-o shows the
        # files and goes back, the query kept across both switches.
        content = press(paths_bytes, "alt-n", 1.0)
        assert content[0].startswith(f"Paths   Dirs {len(dir_rows)}   Files {len(expected_rows)} "), content[0]
        assert content[0].endswith("zoxide"), content[0]
        assert rows(content) == dir_rows, (rows(content), dir_rows)
        assert "ctrl-o files" in content[-1] or "^o files" in content[-1], content[-1]
        content = press(ctrl_o, "alt-n-files")
        assert content[0].endswith("recent-files"), content[0]
        assert rows(content) == expected_rows, (rows(content), expected_rows)
        assert "ctrl-o dirs" in content[-1] or "^o dirs" in content[-1], content[-1]
        content = press(b"app", "alt-n-files-app")
        assert rows(content) == ["~/code/app/main.rs", "~/code/app/old.rs"], content
        content = press(ctrl_o, "alt-n-back")
        assert content[0].endswith("zoxide") and content[1].startswith("› app"), content
        assert rows(content) == ["~/code/app"], content
        content = press(ctrl_o, "alt-n-files-again")
        assert rows(content) == ["~/code/app/main.rs", "~/code/app/old.rs"], content
        os.write(fd, b"\r")
        frame("alt-n-typed", 1.0)
        assert popup("alt-n-typed") is None, "popup closes after Enter"
        assert clipboard() == str(files["main"]), clipboard()
        last = pane_lines(origin)[-1]
        assert last.endswith(str(files["main"])) and "$" in last, pane_lines(origin)
        checks.append(f"{paths_key} shows the zoxide dirs with both counts; ctrl-o shows the files, keeps "
                      "the query, goes back to dirs and returns; Enter pastes and copies the file path")
        os.write(fd, b"\x15")
        frame("cleared")
        clip.unlink()

        content = open_recent("open")
        assert f" Files {len(expected_rows)} " in content[0], content[0]
        assert rows(content) == expected_rows, (rows(content), expected_rows)
        content = press(ctrl_o, "open-dirs")
        assert rows(content) == dir_rows, content
        content = press(ctrl_o, "open-files")
        assert rows(content) == expected_rows, content
        checks.append(f"{key} opens the same popup on files: the log's existing files newest first with ~ "
                      "for HOME, the deleted one dropped, the CLI found off PATH in ~/go/bin; ctrl-o shows dirs")

        os.write(fd, b"main")
        frame("search")
        content = popup("search")
        assert content and content[3] == "› ~/code/app/main.rs", content
        os.write(fd, b"\r")
        frame("typed", 1.0)
        assert popup("typed") is None, "popup closes after Enter"
        assert clipboard() == str(files["main"]), clipboard()
        last = pane_lines(origin)[-1]
        assert last.endswith(str(files["main"])) and "$" in last, pane_lines(origin)
        checks.append("a file-name fragment finds the file; Enter types its path without running it and copies it")

        # Two marks, one Enter: the path with a space arrives quoted, in mark order.
        os.write(fd, b"\x15printf '[%s]\\n' ")
        frame("multi-prefix")
        clip.unlink()
        open_recent("multi-open")
        os.write(fd, b"report")
        frame("multi-report")
        os.write(fd, b"\t\x15todo")
        frame("multi-todo")
        os.write(fd, b"\t")
        frame("multi-marked")
        content = popup("multi-marked")
        assert content and content[0].startswith("Paths · 2 marked"), content
        os.write(fd, b"\r")
        frame("multi-typed", 1.0)
        typed = f"'{files['report']}' {files['todo']}"
        assert pane_lines(origin)[-1].endswith(f"printf '[%s]\\n' {typed}"), pane_lines(origin)
        assert clipboard() == typed, clipboard()
        os.write(fd, b"\r")
        frame("multi-run")
        out = pane_lines(origin)
        want = [f"[{files['report']}]", f"[{files['todo']}]"]
        start = out.index(want[0]) if want[0] in out else -1
        assert start >= 0 and out[start:start + 2] == want, out
        checks.append("Tab marks two files across searches; Enter types both, quoted, in mark order, and copies them")

        # A slow recent-files never delays the directories.
        (home / "slow").touch()
        content = press(paths_bytes, "slow-open", 1.0)
        assert rows(content) == dir_rows, content
        assert " Files … " in content[0], content[0]
        frame("slow-loaded", slow_seconds + 1.5)
        content = popup("slow-loaded")
        assert content and f" Files {len(expected_rows)} " in content[0], content
        os.write(fd, b"\x1b")
        frame("slow-closed")
        (home / "slow").unlink()
        checks.append(f"with recent-files taking {slow_seconds}s, {paths_key} lists dirs at once "
                      "(Files counts … until it lands)")

        # The empty state, for an empty log and for no CLI at all. The second is
        # only provable where the fixed system dirs hold no recent-files either.
        probe = cli("tab", "create", "--label", "recent-entrypoint", "--no-focus")["result"]["root_pane"]["pane_id"]
        empty_log = root / "home-empty-log"
        install_cli(empty_log)
        no_cli = root / "home-no-cli"
        no_cli.mkdir()
        needle = "No recent files yet. nvim and m_path add them when the recent-files CLI is installed."
        cases = [("empty-log", empty_log)]
        if not any(Path(p).exists() for p in ("/opt/homebrew/bin/recent-files", "/usr/local/bin/recent-files")):
            cases.append(("no-cli", no_cli))
        for label, case_home in cases:
            cli("pane", "run", probe, f"env HOME={case_home} PATH=/usr/bin:/bin HERDR_JUMP_PICKER=recent-files "
                                      f"HERDR_JUMP_SOURCE_PANE={probe} {binary} find picker")

            def shows(lines):
                return needle in " ".join(line.strip() for line in lines)
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline and not shows(pane_lines(probe, "visible")):
                time.sleep(0.2)
            assert shows(pane_lines(probe, "visible")), (label, pane_lines(probe, "visible"))
            cli("pane", "send-keys", probe, "Escape")
            time.sleep(0.5)
        checks.append("the empty-state message shows for " + " and ".join(label for label, _ in cases))
        print(json.dumps({"width": args.width, "height": args.height, "passed": checks}, indent=2))
    finally:
        # Only this harness's isolated server and PTY are owned for cleanup.
        try:
            subprocess.run([herdr, "--session", session, "server", "stop"], env=env, capture_output=True, timeout=8)
        finally:
            os.close(fd)
            os.waitpid(pid, 0)
