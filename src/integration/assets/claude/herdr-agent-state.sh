#!/bin/sh
# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# HERDR_INTEGRATION_ID=claude
# HERDR_INTEGRATION_VERSION=12

set -eu

action="${1:-}"
hook_input_file="$(mktemp "${TMPDIR:-/tmp}/herdr-claude-hook.XXXXXX")" || exit 0
trap 'rm -f "$hook_input_file"' EXIT HUP INT TERM
cat >"$hook_input_file" 2>/dev/null || true

case "$action" in
  session) ;;
  *) exit 0 ;;
esac

[ "${HERDR_ENV:-}" = "1" ] || exit 0
[ -n "${HERDR_SOCKET_PATH:-}" ] || exit 0
[ -n "${HERDR_PANE_ID:-}" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

HERDR_ACTION="$action" HERDR_HOOK_INPUT_FILE="$hook_input_file" python3 - <<'PY'
import json
import os
import random
import socket
import subprocess
import time

source = "herdr:claude"
action = os.environ.get("HERDR_ACTION", "")
pane_id = os.environ.get("HERDR_PANE_ID")
socket_path = os.environ.get("HERDR_SOCKET_PATH")
hook_input_file = os.environ.get("HERDR_HOOK_INPUT_FILE")

if not pane_id or not socket_path:
    raise SystemExit(0)

hook_input = {}
if hook_input_file:
    try:
        with open(hook_input_file, encoding="utf-8") as handle:
            content = handle.read()
        if content.strip():
            hook_input = json.loads(content)
    except Exception:
        hook_input = {}

if "CURSOR_VERSION" in os.environ or "cursor_version" in hook_input:
    raise SystemExit(0)
hook_event_name = str(hook_input.get("hook_event_name") or "")
if hook_event_name != "SessionStart":
    raise SystemExit(0)
is_subagent = bool(hook_input.get("agent_id"))
if is_subagent:
    raise SystemExit(0)

def exchange(request):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(2.0)
        client.connect(socket_path)
        client.sendall((json.dumps(request) + "\n").encode())
        with client.makefile("rb") as response:
            return json.loads(response.readline(1024 * 1024))


# Claude resets inherited identity on nested launches. Prefer the pane shell
# boundary (which survives server handoff), but a failed lookup must not hide a
# top-level session. Without it, any second agent ancestor is still evidence of
# nesting. Only an unreadable process table prevents this check altogether.
shell_pid = None
try:
    info = exchange({
        "id": "herdr:claude:owner",
        "method": "pane.process_info",
        "params": {"pane_id": pane_id},
    })
    shell_pid = int(info["result"]["process_info"]["shell_pid"])
except (ValueError, KeyError, TypeError, OSError):
    pass

processes = {}
for attempt in range(2):
    try:
        rows = subprocess.check_output(
            ["ps", "-axo", "pid=,ppid=,comm="], text=True, timeout=2.0
        )
        for row in rows.splitlines():
            fields = row.strip().split(None, 2)
            if len(fields) == 3:
                processes[int(fields[0])] = (int(fields[1]), os.path.basename(fields[2]))
        if processes:
            break
    except (ValueError, OSError, subprocess.SubprocessError):
        processes = {}
if not processes:
    raise SystemExit(0)

try:
    cli_pid = int(os.environ.get("CLAUDE_PID", ""))
except ValueError:
    cli_pid = None
if cli_pid not in processes:
    cli_pid = None
# With no usable CLI PID, the first agent above this hook is the reporter;
# only another one is its parent. A truncated ancestry is not evidence to veto.
current_pid = cli_pid if cli_pid is not None else os.getpid()
cli_name = processes[cli_pid][1].lower() if cli_pid is not None else None
reporter_seen = cli_pid is not None
visited = set()
while current_pid in processes and current_pid not in visited:
    visited.add(current_pid)
    parent_pid, name = processes[current_pid]
    is_same_cli = name.lower() == cli_name
    name = name.lower().lstrip(".")
    if name.endswith("-wrapped"):
        name = name[:-len("-wrapped")]
    is_agent = is_same_cli or name in {"claude", "claude-code", "codex"}
    if current_pid != cli_pid and is_agent:
        if reporter_seen:
            raise SystemExit(0)
        reporter_seen = True
    if current_pid == shell_pid:
        break
    current_pid = parent_pid
request_id = f"{source}:{int(time.time() * 1000)}:{random.randrange(1_000_000):06d}"
report_seq = time.time_ns()
session_id = hook_input.get("session_id")
agent_session_id = session_id if isinstance(session_id, str) and session_id else None
transcript_path = hook_input.get("transcript_path")
agent_session_path = transcript_path if isinstance(transcript_path, str) and transcript_path else None
session_start_source = hook_input.get("source") if hook_event_name == "SessionStart" else None
if not isinstance(session_start_source, str) or not session_start_source:
    session_start_source = None
if agent_session_id:
    params = {
        "pane_id": pane_id,
        "source": source,
        "agent": "claude",
        "seq": report_seq,
        "agent_session_id": agent_session_id,
    }
    if agent_session_path:
        params["agent_session_path"] = agent_session_path
    if session_start_source:
        params["session_start_source"] = session_start_source
    request = {
        "id": request_id,
        "method": "pane.report_agent_session",
        "params": params,
    }
else:
    raise SystemExit(0)

try:
    exchange(request)
except Exception:
    pass
PY
