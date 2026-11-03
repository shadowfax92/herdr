"""Carry live herdr-scratch sessions across the cutover to shadowfax.tools.

Usage (from herdr-tools/):
    python3 crates/scratch/tests/cutover_real.py OLD_HERDR_SCRATCH bin/herdr-tools {move,link}

OLD_HERDR_SCRATCH is the old plugin's binary (build it from a copy of
.../my/herdr-scratch). Everything runs in an IsolatedHerdr; the owner's state is
never touched. The old binary opens a live Neovim scratch and starts its cleanup
worker, exactly as the old plugin does; then the cutover recipe runs:

    link: crates/scratch/cutover-state.sh (the recipe): stop the old worker,
          symlink <state>/scratch to the old state dir, rekey cleanup-state.json.
    move: the rejected alternative: stop the old worker, `mv` the old state dir
          to <state>/scratch and leave a symlink at the old path.

and the new binary must reattach the same Neovim process, run its own worker,
reap the session once its pane closes, and start a server on the same path once
the old one exits. Only `link` keeps the running Neovim following context
handoffs: Neovim watches the context dir by the real path it had at startup.
"""
import fcntl
import json
import os
import shlex
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
HERDR_TOOLS = HERE.parents[2]
sys.path.insert(0, str(HERDR_TOOLS / "tests" / "support"))
from isolated_herdr import IsolatedHerdr  # noqa: E402

old_binary, new_binary = (str(Path(arg).resolve()) for arg in sys.argv[1:3])
mode = sys.argv[3]
assert mode in ("move", "link"), mode


SCRIPT = HERE.parent / "cutover-state.sh"


def cutover(old, new):
    """`link` runs the cutover's own script; `move` is the rejected alternative."""
    if mode == "link":
        for attempt in range(2):  # the script is idempotent
            result = subprocess.run(["sh", str(SCRIPT), str(old), str(new)],
                                    capture_output=True, text=True, timeout=30)
            assert result.returncode == 0, result.stdout + result.stderr
            print(f"cutover-state.sh: {result.stdout.strip()}", flush=True)
        records = json.loads((new / "cleanup-state.json").read_text())["records"]
        assert records and all(key.startswith(f"{new}/") for key in records), records
        return
    # Stop the old worker: it re-checks cleanup.stop every 200 ms and releases
    # cleanup-worker.lock when it exits.
    (old / "cleanup.stop").touch()
    wait_for(lambda: not locked(old / "cleanup-worker.lock"), "the old worker to stop")
    (old / "cleanup.stop").unlink()
    new.parent.mkdir(parents=True, exist_ok=True)
    assert not new.exists() and not new.is_symlink(), new
    os.rename(old, new)
    os.symlink(new, old)


def wait_for(predicate, label, seconds=10):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.05)
    raise AssertionError("Timed out: " + label)


def locked(lock):
    if not lock.exists():
        return False
    with lock.open("a") as f:
        try:
            fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        return False


def run(*args, env, check=True):
    result = subprocess.run(args, env=env, capture_output=True, text=True, timeout=10)
    if check:
        assert result.returncode == 0, (args, result.stdout, result.stderr)
    return result.stdout.strip()


def main():
    with IsolatedHerdr(prefix="co-") as herdr:
        root = herdr.root
        plugins = root / "state/herdr/plugins"
        old, new = plugins / "shadowfax.scratch", plugins / "shadowfax.tools/scratch"
        old_config = root / "config/herdr/plugins/config/shadowfax.scratch"
        new_config = root / "config/herdr/plugins/config/shadowfax.tools"
        work_a, work_b = root / "a", root / "b"
        for path in (old_config, new_config, work_a, work_b):
            path.mkdir(parents=True)
        rpc, init = root / "nvim", root / "init.lua"
        init.write_text(
            "vim.opt.rtp:append(" + json.dumps(str(HERDR_TOOLS / "nvim/scratch")) + ")\n"
            "_G.context_events = {}\n"
            'vim.api.nvim_create_autocmd("User", { pattern = "HerdrScratchContext", callback = function(ev) table.insert(_G.context_events, ev.data) end })\n')
        nvim = json.dumps(["nvim", "-u", str(init), "--listen", str(rpc)])
        (old_config / "config.yaml").write_text(
            'default_popup: { width: "90%", height: "95%" }\n'
            f"scratches:\n  nvim:\n    command: {nvim}\n    tmux_mode: workspace\n    tmux_prefix: ctrl+a\n    key: alt+i\n")
        (new_config / "config.toml").write_text(
            f'[scratch.scratches.nvim]\ncommand = {nvim}\ntmux_mode = "workspace"\ntmux_prefix = "ctrl+a"\nkey = "alt+i"\n')

        cli = lambda *args: herdr.cli(*args, timeout=10)
        workspace = cli("workspace", "list")["result"]["workspaces"][0]["workspace_id"]
        pane = cli("tab", "create", "--workspace", workspace, "--cwd", str(root), "--no-focus")["result"]["root_pane"]["pane_id"]
        set_root = lambda path: cli("pane", "report-metadata", pane, "--source", "grove", "--token", "grove_worktree=" + str(path))
        base = {k: v for k, v in herdr.env.items() if not k.startswith("TMUX")}
        base.update(HERDR_SCRATCH_SOURCE_PANE=pane, HERDR_SCRATCH_NAME="nvim", HERDR_SCRATCH_PREFIX="C-a", SHELL="/bin/sh")
        old_env = base | dict(HERDR_PLUGIN_STATE_DIR=str(old), HERDR_PLUGIN_CONFIG_DIR=str(old_config))
        new_env = base | dict(HERDR_PLUGIN_STATE_DIR=str(new.parent), HERDR_PLUGIN_CONFIG_DIR=str(new_config))
        uid = os.geteuid()
        old_socket = old / f"tmux-{uid}/shadowfax-herdr-workspace"
        new_socket = new / f"tmux-{uid}/shadowfax-herdr-workspace"
        outer = root / "outer"
        tmux = lambda socket, *args, **kw: run("tmux", "-S", str(socket), "-f", "/dev/null", *args, env=base, **kw)
        import hashlib
        session = f"hs/nvim/{hashlib.sha256(herdr.socket.encode()).hexdigest()[:12]}/{pane.replace(':', '-')}"
        rpc_eval = lambda expr: run("nvim", "--server", str(rpc), "--remote-expr", expr, env=base)
        observed = lambda: json.loads(rpc_eval("luaeval('vim.json.encode({cwd=vim.fn.getcwd(), events=_G.context_events, pid=vim.fn.getpid()})')"))

        def show(binary, env, socket):
            argv = [binary] + (["scratch"] if binary == new_binary else []) + ["run-popup"]
            assignments = [f"{key}={value}" for key, value in env.items() if key.startswith("HERDR_")]
            tmux(outer, "new-window", "-d", shlex.join(["env", *assignments, *argv]))
            wait_for(lambda: tmux(socket, "list-clients", "-t", "=" + session, check=False), "attached")

        def hide(socket):
            tmux(socket, "detach-client", "-s", "=" + session)
            wait_for(lambda: not tmux(socket, "list-clients", "-t", "=" + session, check=False), "detached")

        try:
            old_socket.parent.mkdir(parents=True, mode=0o700)
            for socket in (outer, old_socket):
                tmux(socket, "new-session", "-d", "-s", "fixture", "sleep 300")
            # Before: the old plugin owns a live Neovim scratch and its worker.
            set_root(work_a)
            show(old_binary, old_env, old_socket)
            wait_for(rpc.exists, "Neovim RPC")
            before = observed()
            assert before["cwd"] == str(work_a.resolve()), before
            hide(old_socket)
            run(old_binary, "cleanup-start", env=old_env)
            wait_for(lambda: locked(old / "cleanup-worker.lock"), "the old worker")
            print(f"BEFORE: old plugin session {session} nvim pid {before['pid']}; old worker running", flush=True)

            cutover(old, new)
            print(f"CUTOVER ({mode}): {sorted(p.name for p in new.iterdir())}", flush=True)

            # After: the new binary reattaches the same process and the running
            # Neovim still receives the context handoff.
            set_root(work_b)
            events = len(observed()["events"])
            show(new_binary, new_env, new_socket)
            try:
                after = wait_for(lambda: (lambda o: o if len(o["events"]) > events else None)(observed()),
                                 "the running Neovim's context event", seconds=5)
                followed = after["cwd"] == str(work_b.resolve()) and after["events"][-1]["changed"] is True
            except AssertionError:
                after, followed = observed(), False
            assert after["pid"] == before["pid"], (before, after)
            published = json.loads((new / "context" / (session.replace("/", "-") + ".json")).read_text())
            assert published["root"] == str(work_b), published
            env_line = tmux(new_socket, "show-environment", "-t", "=" + session, "HERDR_SCRATCH_CONTEXT")
            assert env_line.startswith(f"HERDR_SCRATCH_CONTEXT={new}/context/"), env_line
            hide(new_socket)
            # Neovim watches the context dir by its real path from startup. A
            # symlinked new dir keeps that path live; a moved dir does not.
            if mode == "link":
                assert followed, after
                print(f"PASS reattach: same nvim pid {after['pid']}; the running Neovim followed the "
                      f"context handoff to {after['cwd']}", flush=True)
            else:
                assert not followed, after
                print(f"PASS reattach: same nvim pid {after['pid']}; DEGRADED: the running Neovim no longer "
                      f"follows context handoffs (cwd stays {after['cwd']}) until it restarts", flush=True)

            run(new_binary, "scratch", "cleanup-start", env=new_env)
            wait_for(lambda: locked(new / "cleanup-worker.lock"), "the new worker")
            report = json.loads(run(new_binary, "scratch", "cleanup", env=new_env))
            assert report["owned_sessions"] == 1 and not report["errors"], report
            cli("pane", "close", pane)
            wait_for(lambda: session not in tmux(new_socket, "list-sessions", "-F", "#{session_name}"),
                     "the new worker to reap the closed pane's session", seconds=75)
            context = new / "context" / (session.replace("/", "-") + ".json")
            wait_for(lambda: not context.exists(), "the reaped session's context file to go")
            print("PASS cleanup: the new worker owns the moved server and reaped the closed source", flush=True)

            # The old server exits with its last session. tmux 3.6a leaves its
            # socket file behind (the old plugin saw the same), so sweeps report
            # it unreadable until a toggle starts a server on that path again.
            tmux(new_socket, "kill-session", "-t", "fixture")
            wait_for(lambda: tmux(new_socket, "list-sessions", check=False) == "", "the old server to exit")
            assert json.loads(run(new_binary, "scratch", "cleanup", env=new_env))["errors"]
            tmux(new_socket, "new-session", "-d", "-s", "fixture", "sleep 300")
            report = json.loads(run(new_binary, "scratch", "cleanup", env=new_env))
            assert not report["errors"], report
            print("PASS exit: after the old server exits, a server starts on the same path and sweeps are clean", flush=True)
        finally:
            subprocess.run([new_binary, "scratch", "cleanup-stop"], env=new_env, capture_output=True, timeout=5)
            subprocess.run([old_binary, "cleanup-stop"], env=old_env, capture_output=True, timeout=5)
            for socket in (outer, old_socket, new_socket):
                subprocess.run(["tmux", "-S", str(socket), "kill-server"], env=base, capture_output=True, timeout=5)
            wait_for(lambda: not locked(new / "cleanup-worker.lock") and not locked(old / "cleanup-worker.lock"),
                     "workers to stop")


if __name__ == "__main__":
    main()
