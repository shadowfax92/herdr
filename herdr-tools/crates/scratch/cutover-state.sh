#!/bin/sh
# Carry live herdr-scratch sessions over to shadowfax.tools at the cutover.
#
#   cutover-state.sh [OLD_STATE_DIR [NEW_SCRATCH_STATE_DIR]]
#   defaults: $XDG_STATE_HOME/herdr/plugins/shadowfax.scratch
#             $XDG_STATE_HOME/herdr/plugins/shadowfax.tools/scratch
#
# Run it after `herdr plugin disable shadowfax.scratch` (so no old hook can
# restart the old worker) and before scratch sessions exist under shadowfax.tools.
#
# It makes the new scratch state dir a symlink to the old one. The old private
# tmux servers keep running with their sockets where they are: tmux resolves
# TMUX_TMPDIR through realpath, so `herdr-tools scratch` reaches the same
# servers and sessions, and running Neovims keep watching the same context dir.
# Moving the dir instead would also reattach, but Neovim watches the context dir
# by its startup real path, so running editors would stop following handoffs.
#
# Afterwards the old state dir is the physical home of scratch state: never
# delete ~/.local/state/herdr/plugins/shadowfax.scratch while the link exists.
# Idempotent; refuses rather than replace scratch state shadowfax.tools made.
set -eu

state_home="${XDG_STATE_HOME:-$HOME/.local/state}"
old="${1:-$state_home/herdr/plugins/shadowfax.scratch}"
new="${2:-$state_home/herdr/plugins/shadowfax.tools/scratch}"

# Whether a scratch cleanup worker holds this lifetime lock (flock, as it takes it).
locked() {
    python3 - "$1" <<'EOF'
import fcntl, sys
try:
    lock = open(sys.argv[1], "a")
except FileNotFoundError:
    sys.exit(1)
try:
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
except BlockingIOError:
    sys.exit(0)
sys.exit(1)
EOF
}

# Asks the worker in state dir $1 to stop (it polls cleanup.stop every 200 ms)
# and waits for its lock; scratch sessions themselves are never touched.
stop_worker() {
    [ -d "$1" ] || return 0
    touch "$1/cleanup.stop"
    tries=0
    while locked "$1/cleanup-worker.lock"; do
        tries=$((tries + 1))
        if [ "$tries" -ge 100 ]; then
            echo "the cleanup worker in $1 did not stop" >&2
            exit 1
        fi
        sleep 0.1
    done
    rm -f "$1/cleanup.stop"
}

if [ -L "$new" ]; then
    if [ "$(readlink "$new")" = "$old" ]; then
        echo "already linked: $new -> $old"
        exit 0
    fi
    echo "$new is a symlink to $(readlink "$new"); refusing to change it" >&2
    exit 1
fi
if [ ! -d "$old" ]; then
    echo "no old scratch state at $old; nothing to carry over"
    exit 0
fi

stop_worker "$old"
if [ -e "$new" ]; then
    # shadowfax.tools already started its worker here. Replace the dir only if
    # it never held a tmux server socket or a context publication.
    stop_worker "$new"
    if [ -n "$(ls -A "$new"/tmux-* 2>/dev/null)" ] || [ -n "$(ls -A "$new/context" 2>/dev/null)" ]; then
        echo "$new already holds scratch sessions; refusing to replace it" >&2
        exit 1
    fi
    rm -rf "$new"
fi
mkdir -p "$(dirname "$new")"
ln -s "$old" "$new"

# cleanup-state.json keys each session by its socket path; the new mode sees
# the sockets under $new, so rekey to keep the cached terminal identities.
python3 - "$old" "$new" <<'EOF'
import json, pathlib, sys
old, new = sys.argv[1:]
cache = pathlib.Path(new, "cleanup-state.json")
if cache.exists():
    state = json.loads(cache.read_text())
    state["records"] = {
        (new + key[len(old):] if key.startswith(old + "/") else key): record
        for key, record in state.get("records", {}).items()
    }
    temporary = cache.with_suffix(".json.cutover")
    temporary.write_text(json.dumps(state) + "\n")
    temporary.chmod(0o600)
    temporary.replace(cache)
EOF
echo "linked $new -> $old"
