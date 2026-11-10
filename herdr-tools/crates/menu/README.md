# menu: the pane menu (`herdr-tools menu`)

Fork conversations, rearrange panes, copy the current pane's context for another
agent, pick what Herdr focuses or hides, and run your own commands from one popup. This mode was the herdr-menu
plugin (`shadowfax.menu`); its action is now `shadowfax.tools.menu`.

Bind it in Herdr's `config.toml`:

```toml
[[keys.command]]
key = "alt+q"
type = "plugin_action"
command = "shadowfax.tools.menu"
```

Then press one key:

| Key | Result |
| --- | --- |
| `l` | Fork beside the source on the left and balance its row |
| `r` | Fork beside the source on the right and balance its row |
| `t` | Fork into the top row and balance that row |
| `b` | Fork into the bottom row and balance that row |
| `o` | Rotate the last forked pane to its next available neighbor |
| `d` | Preview a new arrangement for the current tab's existing panes |
| `h` | Copy the current pane's identity and agent commands to the clipboard |
| `f` | Open Herdr's focus picker |
| `v` | Open Herdr's hide picker (the old "excluded workspaces" row) |
| `F` | Switch focus on or off, keeping what is focused |

The fork items read the focused pane's hook-reported native session ID, ask
[RTR](https://github.com/shadowfax92/rtr) to fork that exact Claude/Codex
conversation in its owning profile, and start it in a new Herdr pane. They never
guess the newest session. If hook metadata is unavailable, the new pane opens
RTR's cwd-filtered conversation picker instead.

Forking requires RTR on `PATH` (or `RTR_BIN_PATH`, which the launcher hands to
the popup); redistribution and context copying do not. Exact one-key forking
also needs Herdr's native agent hooks in each isolated RTR profile home. Use
`rtr paths --json` to enumerate the homes, then install with the matching
environment:

```sh
CODEX_HOME=/path/from/rtr herdr integration install codex

CLAUDE_CONFIG_DIR=/path/from/rtr \
CLAUDE_SECURESTORAGE_CONFIG_DIR=/path/from/rtr \
  herdr integration install claude
```

Repeat this installation for every new RTR profile home. For Codex, an RTR
`copy` mapping from `~/.codex/hooks.json` to `hooks.json` makes new profiles
inherit the hook automatically: that file points to the hook script by absolute
path, so the script does not need copying into each profile.

Without hooks the menu still works, but opens `rtr fork --here` so you can
select the source session.

## Focus and hide

Focus and hide are Herdr's own (the herdr fork's `herdr scope …`): pick
workspaces or tabs on any machine, and the machines and agents panels show only
those (focus) or leave them out (hide), with no notifications from the rest.
The scope lives in the local server until it restarts. The menu never edits it
itself; it asks your client to act on Local's scope, so both work from a pane
on any machine:

- `type = "focus"` and `type = "hide"` close the menu, then open the picker
  (`scope.pick`) in that mode on your client. The mode only sets what `space`
  marks; both marks stay one key away (`f` focus, `x` hide).
- `type = "focus_toggle"` asks your client to switch focus on or off
  (`scope.toggle`), keeping its targets, and closes the menu. Its label is the
  configured one: the menu talks to the server of the machine its pane is on,
  and on a studio pane that server's scope is not the one the client applies
  (only Local's is), so the menu shows no state and never reads or sets the
  scope itself. A refusal (`no_client`, or a Herdr without `scope.toggle`)
  shows in the menu, which stays open.

The picker opens on the client once the popup is gone: the menu starts a
detached `herdr-tools menu scope-pick <mode>`, closes its own popup
(`popup.close`) and exits, and the helper sends `scope.pick` after that. A
refused pick (for example `no_client`, with no client attached) is a
notification.

As the menu opens it reads `scope.get` once, when it has a scope row.
`scope.pick` and `scope.toggle` cannot be probed without acting, and the scope
methods ship together, so a Herdr that knows `scope.get` is taken to have all
of them. On a Herdr without scope (Herdr 0.9.5) every scope row reads
`<label> (unavailable)`, is never selectable or activated, and a red panel
below the menu explains why (scroll it with Page Up/Down, Home/End). If the
read fails for another reason the rows stay usable and a failure shows when
one is used.

### Workspace visibility (retired)

`type = "workspace_visibility"` toggled the Agents plugin's excluded-workspace
rows over Agents' control socket. Agents is retired, and Herdr's native hide
replaced that list. The item still parses so existing menus keep working, and
it now behaves exactly as `type = "hide"`: it opens Herdr's hide picker, and on
a Herdr without scope it reads `<label> (unavailable)` as before. Change the
type to `hide` at your leisure.

## Copy pane context

Press `h` to copy a Markdown handoff for the pane that opened the menu. Paste it
into another agent to identify the terminal you want it to watch. The action
copies immediately and closes the popup on success. If copying fails, its error
screen offers Enter to retry, Escape to return, and Alt-q to close.

The handoff includes capture time, session/socket routing, workspace/tab identity,
pane ID, terminal ID, pane label, terminal title, working directories, and any
recognized agent name, kind, state, and native session identity. It also supplies
commands for refreshing metadata, reading output, waiting, and prompting that
specific pane. Shell panes work too. It does not read terminal output during the
copy operation. Only the source pane is included; there is no pane picker.

`terminal_id` lets an agent correlate the same terminal after a cross-workspace
move changes its pane ID. It is not an agent command target. The recipient should
use the Herdr skill, check `HERDR_ENV=1`, refresh identity before messaging, and
use the exact copied session/socket. Copied context is not permission to send a
message.

macOS uses `pbcopy`; Linux uses an available `wl-copy`, `xclip`, or `xsel`.
Success is reported only after the clipboard command exits successfully.

For read-only inspection inside Herdr (the binary is
`~/.local/share/herdr/plugins/shadowfax.tools/bin/herdr-tools`):

```sh
herdr-tools menu copy-context --stdout
herdr-tools menu copy-context --source-pane "$HERDR_PANE_ID" --stdout
```

Omit `--stdout` to copy. The source defaults to the captured menu source or the
calling pane's `HERDR_PANE_ID`, never another client's focused pane.

## How pane forking works

1. The action records the focused pane ID and cwd before opening its popup.
2. The selected fork action reads that exact pane with `herdr pane get`.
3. Left/right insert beside the source and equalize that row. Top/bottom insert
   into the existing top/bottom row, near the source column, and equalize it.
   A **5×2** layout becomes **6+5** or **5+6**; no third row is created.
   Other rows keep their pane order, widths, and heights.
4. With only one row, top/bottom create a second full-width row: all originals
   stay together, and the new fork occupies the new row. This uses the same
   process-preserving mover as Redistribute. Grids created by the layout mode
   normally resize their existing rows in place.
5. After placement succeeds, the new shell receives
   `rtr fork <native-id> --tool <agent>`. RTR resolves
   the owning profile and uses the tool's native fork command.
6. The new pane ID is stored in `menu/last-fork.json` under
   `HERDR_PLUGIN_STATE_DIR`; rotate swaps that same running pane with the next
   available directional neighbor.

Pane IDs remain attached to their running processes during swaps, so rotating
does not restart or recreate the agent. The new fork pane receives focus after
the popup closes.

Forking also handles grids built by splitting columns vertically: it rearranges
existing terminals into rows automatically, preserving the other rows' cell
boundaries. Deeply skewed trees use the same fallback. Irregular tilings (such as
one tall pane beside two stacked panes) are automatically arranged into balanced
rows **before** creating the fork. This uses live moves within the same workspace;
pane IDs, terminal processes, cwd, scrollback, and scratch ownership survive.

Normalization orders panes top-to-bottom, then left-to-right. It prefers the
existing number of distinct pane-top positions and tries other row counts when
needed to fit the extra pane. Left/right remain beside the source; top/bottom
reuse the resulting edge row near the source column. At capacity the source can
move to the nearest reserved slot, retaining the other panes' relative order.
If only one usable column fits, top/bottom can occupy their own full-width edge
row; the originals are stacked with enough space reserved for that final split.
Full-width row layouts keep their existing behavior and unaffected geometry.
Space below 12 columns × 5 lines per resulting pane is rejected before moving.

Changing a layout during a fork stops the operation instead of resizing stale
split positions. If state persistence or RTR input fails, cleanup restores the
original geometry and removes the new shell, provided the layout has not changed
concurrently. If an irregular layout was normalized successfully, that normalized
arrangement is the rollback baseline; the previous irregular split tree is not
recreated. Existing sessions remain running. Interrupted staging leaves its
source and holding tabs available; the error screen shows their IDs and scrolls
with arrows, Page Up/Down, Home/End. An empty fork is retained when needed to keep
the source tab alive for recovery.

## Redistribute existing panes

Press `d`. Choose:

- `s` **Side by side**: one row of equal-width panes.
- `g` **Grid…**: choose the number of rows; columns are calculated automatically.

Grid starts at two rows (one for a single-pane tab). Type a row count or use
Left/Right, inspect the preview, then press Enter to apply. Escape goes back;
Alt-q closes the menu. Choosing a mode or changing the preview never moves panes.

Seven panes and two rows become **4 + 3**: both rows have equal height, with
wider panes in the shorter row. No empty eighth pane is created. Panes keep
their visual reading order, and the pane that opened the menu regains focus.
Only that tab is rearranged; all its tiled panes participate, including shells.
The highlighted preview pane is the original focused pane.

Existing terminal processes, pane IDs, and scrollback survive. Resizing may
reflow text. A zoomed tab is unzoomed only when applying. A changed layout
requires a refreshed preview and another confirmation. Layouts that would
produce panes smaller than 12 columns × 5 lines, including frames, are rejected.

Herdr cannot move panes within the same tab directly, so the menu briefly
stages them in a `menu-redistribute-…` tab and moves the original panes back into
a balanced split tree. It removes only its temporary shell after verifying that
all original panes returned. It never uses the process-replacing `layout.apply`
API. If an operation fails, original panes remain running and the error names
the source and holding tabs first: inspect those tabs and move the panes back
manually. Scroll long errors with ↑/↓, Page Up/Down, or Home/End.
Do not close the holding tab while it contains your agents. Automatic rollback
and undo are not provided.

## Configure

The menu is the `[menu]` table of the shared herdr-tools config,
`~/.config/herdr/plugins/config/shadowfax.tools/config.toml`. Without a `[menu]`
table the built-in menu ([`config.default.toml`](config.default.toml)) is used,
with a warning saying so:

```toml
[menu]
title = "agent forks"
max_height = 24

[[menu.items]]
type = "fork"
key = "l"
label = "fork current conversation left"
placement = "left"

# … r / t / b forks, a separator, rotate_fork "o", a separator,
# redistribute "d", copy_context "h", a separator, focus "f", hide "v"
# and focus_toggle "F"
```

`[menu]` fields:

| Field | Default | Meaning |
| --- | --- | --- |
| `title` | `commands` | Popup title |
| `max_height` | `24` | Maximum automatic height |
| `items` | none | The menu, in order |

The popup size is `[popup.size].menu` (or a `[[popup.profile]]` entry for
`menu`), which fixes both dimensions:

```toml
[popup.size]
menu = { width = 80, height = 32 }   # cells, or "80%"
```

Without one, the menu is 64 cells wide (68 for the built-in menu) and as tall as
its items, two rows for config warnings, and four for the frame, between 6 and
`max_height`. The built-in menu's thirteen rows and its "no `[menu]` table"
warning make it 19 rows tall. The old top-level `width`/`height` belong in `[popup.size].menu`;
inside `[menu]` they are ignored with a warning.

Item types:

| Type | Required fields | Meaning |
| --- | --- | --- |
| `fork` | `key`, `label`, `placement` | Fork left, right, top, or bottom |
| `rotate_fork` | `key`, `label` | Rotate the last fork from either pane in its recorded pair |
| `redistribute` | `key`, `label` | Preview Side by side or a rows-first Grid for existing panes |
| `copy_context` | `key`, `label` | Copy the pane that opened the menu and commands to inspect it |
| `focus` | `key`, `label` | Close the menu and open Herdr's focus picker (see [Focus and hide](#focus-and-hide)) |
| `hide` | `key`, `label` | Close the menu and open Herdr's hide picker |
| `focus_toggle` | `key`, `label` | Ask your client to switch focus on or off, keeping its targets |
| `workspace_visibility` | `key`, `label` | Retired; the same as `hide` |
| `shell` | `key`, `label`, `command` | Run a trusted user shell command |
| `separator` | none | Draw a divider |

Shell items also accept `mode = "foreground" | "detached"`, an optional
`confirm`, and an absolute or source-relative `cwd`. Commands run as
`$SHELL -lc <command>` and inherit Herdr's plugin environment, including
`HERDR_BIN_PATH`, `HERDR_PLUGIN_ROOT` (the installed plugin dir, so
`"$HERDR_PLUGIN_ROOT/bin/herdr-tools" <mode> …` runs another mode) and
`HERDR_MENU_SOURCE_PANE_ID`. Use that shell's syntax; options for builtins such
as `read` are not portable across shells.

TOML literal strings preserve backslashes. Quote the Herdr path directly
instead of escaping the double quotes:

```toml
command = '"$HERDR_BIN_PATH" server reload-config'
```

Interactive commands can prompt after the menu releases the terminal. For Fish,
use its `read -P` option:

```toml
command = 'read -P "Value: " value; test -z "$value"; or printf "%s\n" "$value"'
```

The config is trusted user code; `confirm` is a convenience guard, not a
security boundary.

### Reap scratch sessions

Use the scratch mode's plugin action for a cleanup menu item:

```toml
[[menu.items]]
key = "x"
label = "reap scratch sessions"
command = '"$HERDR_BIN_PATH" plugin action invoke shadowfax.tools.scratch-reap'
confirm = "Reap stale Herdr Scratch sessions?"
```

Going through Herdr runs the action with the plugin's own environment, and the
action reports its result as a notification.

## Menu controls

| Key | Result |
| --- | --- |
| configured accelerator | Run that action immediately |
| `Up` / `Down` | Move selection |
| `j` / `k` | Move when that character is not an accelerator |
| `Enter` | Run the selected action |
| `y` / `Enter` | Accept a shell confirmation |
| `n` / `Esc` | Cancel a shell confirmation |
| `Esc` / `Alt-q` | Close the menu |

A broken `config.toml` opens a closable error screen. Invalid entries are
skipped while valid entries remain available; duplicate accelerators keep the
first valid item.

## Migrating from herdr-menu

`herdr-tools migrate-config` reads the old
`~/.config/herdr/plugins/config/shadowfax.menu/config.toml`: `title`,
`max_height` and `items` become `[menu]`, `width`/`height` become
`[popup.size].menu`, old action ids in item commands become `shadowfax.tools.*`
ones, and commands that ran an old plugin binary by path
(`…/herdr-layouts/target/release/herdr-layouts grid-dialog …`) run
`"$HERDR_PLUGIN_ROOT/bin/herdr-tools" layout grid-dialog …` instead. No old
state needs copying: `last-fork.json` only names live panes.

## Development

From `herdr-tools/`. `just ci` runs the Rust tests. The Python regressions need
an installed Herdr and Python 3; each starts its own isolated server (temp
config/state, its own session or socket), fake RTR, and never touches your live
session:

```sh
cargo build --locked -p tools-menu --example fork_probe
python3 crates/menu/tests/headless_fork.py target/debug/examples/fork_probe
python3 crates/menu/tests/headless_fork.py target/debug/examples/fork_probe --narrow 12
python3 crates/menu/tests/headless_fork.py target/debug/examples/fork_probe --narrow 23

just bin
python3 crates/menu/tests/real_redistribution.py target/release/herdr-tools
python3 crates/menu/tests/real_fork_rows.py target/release/herdr-tools
python3 crates/menu/tests/real_menu.py    # the laid-out plugin, every item once
python3 crates/menu/tests/real_scope.py   # focus/hide rows + Find's filter; needs `herdr scope`
```

`headless_fork.py` drives the fork controller with no UI. `real_redistribution.py`
and `real_fork_rows.py` attach a PTY client and press the menu's keys.
`real_menu.py` installs and links the real plugin, migrates the owner's old
config, and drives each item kind through the `shadowfax.tools.menu` action.
`real_scope.py` needs a Herdr with `herdr scope` (set `HERDR_BIN` to a fork
build); on one without it, it prints a SKIP line and exits 0.
