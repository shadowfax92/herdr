"""Production-controller acceptance on a private headless Herdr server.

Build: cargo build --locked --example fork_probe
Run: python3 tests/headless_fork.py target/debug/examples/fork_probe
No TUI, popup, client socket, screenshot, or owner session is used. The shim
suppresses focus commands and injects native-session metadata plus failures.
"""
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time

probe = str(Path(sys.argv[1]).resolve())
narrow = "--narrow" in sys.argv
columns = int(sys.argv[sys.argv.index("--narrow") + 1]) if narrow else 240
herdr = shutil.which("herdr")
assert herdr


def wait_for(check):
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(0.03)
    raise AssertionError("timed out waiting for private test server")


with tempfile.TemporaryDirectory(prefix="fork-headless-", dir="/tmp") as root:
    root = Path(root)
    sock = root / "api.sock"
    config = root / "herdr.toml"
    config.write_text('onboarding = false\n[terminal]\ndefault_shell = "/bin/sh"\n'
                      f'[server]\nheadless_cols = {columns}\nheadless_rows = {20 if narrow else 60}\n')
    env = {k: v for k, v in os.environ.items() if not k.startswith("HERDR_")}
    env.update(HERDR_SOCKET_PATH=str(sock), HERDR_CONFIG_PATH=str(config),
               XDG_CONFIG_HOME=str(root / "config"), XDG_STATE_HOME=str(root / "state"),
               XDG_DATA_HOME=str(root / "data"), XDG_CACHE_HOME=str(root / "cache"))
    log = root / "commands.jsonl"
    mode_file = root / "mode"
    mode_file.write_text("")
    counter = root / "moves"
    state = root / "fork-state" / "last-fork.json"
    state.parent.mkdir()
    rtr = root / "rtr"
    rtr.write_text('#!/bin/sh\nprintf "FORK_STARTED %s\\n" "$*"\nexec sleep 300\n')
    rtr.chmod(0o700)
    shim = root / "herdr-shim"
    shim.write_text("#!/usr/bin/env python3\n" + f'''
import json, pathlib, subprocess, sys
args = sys.argv[1:]
with open({str(log)!r}, "a") as out:
    out.write(json.dumps(args) + "\\n")
mode = pathlib.Path({str(mode_file)!r}).read_text()
if args[:2] == ["tab", "focus"]:
    print('{{"result":{{}}}}')
    sys.exit(0)
if "--focus" in args:
    args[args.index("--focus")] = "--no-focus"
if args[:2] == ["pane", "move"]:
    path = pathlib.Path({str(counter)!r})
    n = int(path.read_text()) + 1 if path.exists() else 1
    path.write_text(str(n))
    if mode == "move-" + str(n):
        sys.exit("injected move failure")
if mode == "holding" and args[:2] == ["tab", "create"]:
    sys.exit("injected holding failure")
if mode == "run" and args[:2] == ["pane", "run"]:
    sys.exit("injected run failure")
result = subprocess.run([{herdr!r}, *args], capture_output=True, text=True)
if result.returncode == 0 and mode == "stale" and args[:2] == ["pane", "layout"]:
    # Another client splits between the planner's geometry and topology reads.
    subprocess.run([{herdr!r}, "pane", "split", args[-1], "--direction", "right", "--no-focus"], check=True, capture_output=True)
    pathlib.Path({str(mode_file)!r}).write_text("")
if result.returncode == 0 and mode == "concurrent" and args[:2] == ["pane", "move"]:
    # The source terminal moves again after our first staging move. The next
    # ownership check must stop rather than follow it into that unrelated tab.
    created = subprocess.run([{herdr!r}, "tab", "create", "--workspace", args[2].split(":")[0], "--no-focus"], check=True, capture_output=True, text=True)
    result_tab = json.loads(created.stdout)["result"]
    tab = result_tab["tab"]["tab_id"]
    target = result_tab["root_pane"]["pane_id"]
    subprocess.run([{herdr!r}, "pane", "move", args[2], "--tab", tab, "--target-pane", target, "--split", "right", "--ratio", "0.5", "--no-focus"], check=True, capture_output=True)
    pathlib.Path({str(mode_file)!r}).write_text("")
if result.returncode == 0 and args[:2] == ["pane", "get"]:
    value = json.loads(result.stdout)
    value["result"]["pane"].update(agent="codex", agent_session={{"source":"herdr:codex", "agent":"codex", "kind":"id", "value":"native-test-session"}})
    result.stdout = json.dumps(value)
if result.returncode == 0 and args[:2] == ["pane", "split"] and mode == "state":
    path = pathlib.Path({str(state)!r})
    path.unlink(missing_ok=True)
    path.mkdir()
sys.stdout.write(result.stdout)
sys.stderr.write(result.stderr)
sys.exit(result.returncode)
''')
    shim.chmod(0o700)

    def cli(*args, text=False):
        result = subprocess.run([herdr, *args], env=env, capture_output=True, text=True, timeout=10)
        assert result.returncode == 0, result.stderr or result.stdout
        return result.stdout if text or not result.stdout.lstrip().startswith("{") else json.loads(result.stdout)["result"]

    def api(method, params):
        with socket.socket(socket.AF_UNIX) as conn:
            conn.settimeout(5)
            conn.connect(str(sock))
            conn.sendall((json.dumps(dict(id="test", method=method, params=params)) + "\n").encode())
            result = json.loads(conn.makefile().readline())
            assert "error" not in result, result
            return result["result"]

    def layout(pane):
        return cli("pane", "layout", "--pane", pane)["layout"]

    def split(pane, direction, ratio=0.5):
        return cli("pane", "split", pane, "--direction", direction, "--ratio", str(ratio),
                   "--cwd", str(root), "--no-focus")["pane"]["pane_id"]

    def read(pane):
        return cli("pane", "read", pane, "--source", "recent-unwrapped", "--lines", "100", text=True)

    def create(shape):
        created = cli("tab", "create", "--workspace", workspace, "--cwd", str(root), "--no-focus")
        first = created["root_pane"]["pane_id"]
        second = split(first, "right", 0.37)
        if shape == "t":
            panes = [first, second, split(second, "down", 0.4)]
        elif shape == "mirror":
            panes = [first, split(first, "down", 0.65), second]
        elif shape in ("uneven", "grid"):
            panes = [first, split(first, "down", 0.4), second,
                     split(second, "down", 0.4 if shape == "grid" else 0.65)]
        elif shape == "skewed":
            panes = [first, second] + [split(first, "right", 0.9) for _ in range(9)]
        else:
            panes = [first, second]
        for pane in panes:
            cli("pane", "run", pane, f"printf 'KEEP_{pane}\\n'; exec sleep 300")
            wait_for(lambda: f"KEEP_{pane}" in read(pane))
        def sleeping(pane):
            info = cli("pane", "process-info", "--pane", pane)
            processes = info["process_info"]["foreground_processes"]
            return info if processes and all(p["name"] == "sleep" for p in processes) else None
        processes = {p: wait_for(lambda p=p: sleeping(p)) for p in panes}
        return created["tab"]["tab_id"], panes, processes

    def preserve(panes, processes):
        for pane in panes:
            after_process = cli("pane", "process-info", "--pane", pane)
            assert after_process == processes[pane], (pane, processes[pane], after_process)
            assert f"KEEP_{pane}" in read(pane), pane
        commands = [json.loads(line) for line in log.read_text().splitlines()]
        assert not any(c[:2] == ["pane", "close"] and c[2] in panes for c in commands)
        assert not any(c[:2] == ["pane", "run"] and c[2] in panes for c in commands)
        assert not any(c[:2] == ["layout", "apply"] for c in commands)

    output = open(root / "server.log", "w")
    server = subprocess.Popen([herdr, "server"], env=env, stdin=subprocess.DEVNULL, stdout=output, stderr=output)
    try:
        wait_for(lambda: sock.exists() or (server.poll() is not None and False))
        created = cli("workspace", "create", "--label", "headless-fork")
        workspace = created["workspace"]["workspace_id"]
        keeper = created["tab"]["tab_id"]
        cases = [(shape, index, direction) for shape, count in [("t", 3), ("mirror", 3), ("uneven", 4), ("grid", 4), ("row", 2)]
                 for index in range(count) for direction in ["left", "right", "top", "bottom"]]
        cases += [("skewed", 0, "right")]
        if "--smoke" in sys.argv:
            cases = cases[:1]
        if "--failures-only" in sys.argv:
            cases = []
        if "--skewed-only" in sys.argv:
            cases = [("skewed", 0, "right")]
        if narrow:
            cases = [("t", index, direction) for index in range(3) for direction in ["top", "bottom"]]
        for shape, index, direction in cases:
            tab, panes, processes = create(shape)
            source = panes[index]
            before = layout(source)
            if index == 0:
                cli("pane", "zoom", "--pane", source, "--on")
            log.write_text("")
            result = subprocess.run([probe, str(shim), str(sock), source, str(root), str(state), str(rtr), direction],
                                    env=env, capture_output=True, text=True, timeout=30)
            assert result.returncode == 0, (shape, index, direction, result.stderr)
            record = json.loads(state.read_text())
            fork = record["fork_pane_id"]
            after = layout(source)
            assert after["tab_id"] == tab and after["workspace_id"] == workspace
            assert {p["pane_id"] for p in after["panes"]} == set(panes + [fork])
            assert record["source_pane_id"] == source and record["next_direction"] == 0
            rects = {p["pane_id"]: p["rect"] for p in after["panes"]}
            s, f = rects[source], rects[fork]
            if direction in ("left", "right"):
                assert s["y"] == f["y"] and s["height"] == f["height"]
                assert f["x"] + f["width"] == s["x"] if direction == "left" else s["x"] + s["width"] == f["x"]
            else:
                edge = min(r["y"] for r in rects.values()) if direction == "top" else max(r["y"] for r in rects.values())
                assert f["y"] == edge
            assert all(r["width"] >= 12 and r["height"] >= 5 for r in rects.values())
            if narrow:
                assert all(r["width"] == columns and r["height"] == 5 for r in rects.values())
            if shape == "grid":
                for p in before["panes"]:
                    if p["rect"]["y"] != f["y"]:
                        assert rects[p["pane_id"]] == p["rect"]
            wait_for(lambda: "FORK_STARTED fork native-test-session --tool codex" in read(fork))
            preserve(panes, processes)
            cli("tab", "close", tab)
            print("PASS", shape, index, direction, flush=True)

        for mode in (["stale", "concurrent", "holding", "move-1", "move-2", "move-3", "run", "state"] if "--smoke" not in sys.argv else []):
            tab, panes, processes = create("t")
            source = panes[0]
            if mode in ("run", "state"):
                cli("pane", "zoom", "--pane", source, "--on")
            previous = b'{"schema_version":1,"source_pane_id":"old","fork_pane_id":"previous","next_direction":2}\n'
            state.write_bytes(previous)
            counter.unlink(missing_ok=True)
            mode_file.write_text(mode)
            log.write_text("")
            result = subprocess.run([probe, str(shim), str(sock), source, str(root), str(state), str(rtr), "top" if narrow else "left"],
                                    env=env, capture_output=True, text=True, timeout=30)
            assert result.returncode != 0, mode
            expected = {"stale": "Layout changed while planning", "concurrent": "changed during redistribution",
                        "holding": "injected holding failure", "run": "starting RTR", "state": "remembering the fork pane"}
            assert expected.get(mode, "injected move failure") in result.stderr, (mode, result.stderr)
            preserve(panes, processes)
            commands = [json.loads(line) for line in log.read_text().splitlines()]
            assert not any(c[:2] == ["pane", "run"] for c in commands) if mode != "run" else True
            if mode != "state":
                assert state.read_bytes() == previous
            else:
                state.rmdir()
            if mode in ("run", "state"):
                assert {p["pane_id"] for p in layout(source)["panes"]} == set(panes)
                restored = layout(source)
                assert restored["zoomed"]
                source_rect = next(p["rect"] for p in restored["panes"] if p["pane_id"] == source)
                assert source_rect["width"] == restored["area"]["width"]
                assert source_rect["y"] == restored["area"]["y"]
            mode_file.write_text("")
            # Only this private workspace's test tabs, including retained recovery tabs.
            for item in cli("tab", "list", "--workspace", workspace)["tabs"]:
                if item["tab_id"] != keeper:
                    cli("tab", "close", item["tab_id"])
            print("PASS failure", mode, flush=True)
        print(f"PASS {len(cases)} controller cases; originals retain processes and scrollback", flush=True)
    finally:
        # Stop only the child process whose socket/config this script created.
        server.terminate()
        try:
            server.wait(timeout=10)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait(timeout=5)
        output.close()
