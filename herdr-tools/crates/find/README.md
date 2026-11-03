# Herdr Jump

Search-first native Rust picker popups for Herdr. Each picker is a popup you
type into: one shared fuzzy matcher, one look, and one Enter.

| Key | Picker | Enter |
| --- | --- | --- |
| **Alt-p** | **Panes**: visible open panes, with agent states, views, kill mode and preview | Jump to the pane |
| **Alt-n** | **Paths**: every directory zoxide knows, in zoxide's order; Tab marks several | Type the full paths into the pane you came from, and copy them |

Pickers are declared in [`pickers.toml`](pickers.toml); adding one is
configuration, not code (see [Add a picker](#add-a-picker)).

## Panes (Alt-p)

Press **Alt-p**, type part of a task, pane name, path, or tab, and press **Enter**.
Search is focused immediately. Results are a flat list with aligned
**Workspace › Tab › Name › Task** columns, where Name is the pane's Herdr name
(the label on its border). Each row retains its hierarchy even when fuzzy
ranking mixes workspaces. Narrow views use two lines per result.
Under the list, the selected pane's `~` path and pane ID stay visible, led by
its name when its row can't show the whole name. Hook-reported task titles take
precedence; terminal titles and pane aliases remain searchable. Composed and
decomposed Unicode names match consistently.

The Name column is as wide as the longest name, up to 20 cells; longer names are
shortened with `…` around the letters a search matched. Unnamed panes leave it
blank. Task always keeps at least 32 cells, so the Name column narrows in a
narrower popup and goes away when it would drop below 12 cells or rows stack on
two lines.

Every result shows why it matched. The matching letters are highlighted, and
when they are in a field the row doesn't otherwise show, such as the pane's path
(or its name, without a Name column), that field follows the title, dimmed:
`CLAUDE.md task label rules · ~/.config/skl/library/skills`. The title keeps at
least half its column. A long path keeps the match and its last directory
(`…/skl/library/.wt/feat/vh-skeleton`).

Every row starts with its agent's state in Herdr's own symbols and colors:
`×` blocked, `✓` done (unread), `◐` working, `○` idle, and `·` for shells,
editors, and other panes without an agent. A dim, right-aligned column shows each
agent's session age (`45s`, `12m`, `3h`, `2d`), with the same meaning as the
Agents plugin; it stays blank when the start is unknown. With no query the list
puts what needs you first (blocked, done, working, idle, then plain panes),
youngest first within each state.

**Ctrl-e** cycles views: All → In progress → Unread/Blocked → Idle. The header
shows every view's count and highlights the active one; typing still searches
inside it, and plain shells and editors appear only in All. **Ctrl-x** enters
kill mode: **Tab** or **Space** marks panes (a red `■` and red title), the header
counts marks (including any the current view hides), and **Enter** opens a review
listing exactly what will close. **y** kills them; any other key cancels back to
kill mode with the marks intact. The pane Jump was opened from can never be marked.

Jump searches open panes in the invoking Herdr session, including shells and
editors, while following the Agents sidebar's workspace visibility. The one
hidden list lives in the Agents plugin's `config.toml`:

```toml
[workspace_visibility]
excluded_labels = ["ft"]
```

Edit that list in Agents; labels match exactly and case-sensitively. No rebuild
is needed. The one switch is Agents' `show_excluded`, controlled by Menu's
workspace visibility action or `herdr-agents workspace-visibility show|hide`.
When false, Jump removes excluded workspaces from every view, count, search,
column measurement, and kill target. When true, Jump lists those panes too.
The source pane follows the same visibility rule. A refresh removes newly hidden
panes from marks and any open kill review.

Jump reads the policy once on open and **Ctrl-r**, on its catalog worker. When
panes are omitted it shows a dim count such as `12 hidden`. If the policy cannot
be read, Jump lists all panes with a dim `Hidden list unavailable` notice. If
the Agents state directory does not exist, Jump works silently without Agents.
Discovery uses `${XDG_STATE_HOME:-$HOME/.local/state}/herdr/plugins/shadowfax.agents`,
overridden by `HERDR_AGENTS_STATE`; Jump never keeps a separate list or parses
Agents' TOML. It navigates existing panes; archived conversations and other
Herdr servers are outside its scope.

### Controls

| Key | Action |
| --- | --- |
| Type | Search titles, pane labels, agent names, tabs, workspaces, and paths (including `~`) |
| Up / Down, Ctrl-p / Ctrl-n | Select a result while keeping search focused |
| Enter | Jump to the selected pane and close |
| Ctrl-e | Next view: All → In progress → Unread/Blocked → Idle |
| Ctrl-x | Enter or leave kill mode (leaving clears marks) |
| Ctrl-space | Toggle the selected pane's terminal preview |
| Ctrl-u | Clear the query |
| Backspace | Remove the last grapheme |
| Ctrl-r | Refresh the live pane catalog |
| Escape / Ctrl-c | Close the popup |

Ctrl-k is deliberately unbound: it is commonly a global shortcut (for example,
system-wide up/down), so it may never reach the popup.

In kill mode:

| Key | Action |
| --- | --- |
| Tab / Space | Mark or unmark the selected pane and move down |
| Shift-Tab | Mark or unmark the selected pane and move up |
| Enter | Review the marked panes (never the unmarked selection) |
| y | In the review: kill exactly the listed panes |
| Any other key | In the review: cancel back to kill mode, marks kept |
| Escape | Leave kill mode and clear marks |
| Ctrl-c | Close the popup without killing |

Typing, Backspace, Ctrl-u, arrows, Ctrl-e, Ctrl-space, and Ctrl-r work in kill
mode too; Space marks instead of typing a space.

Each query word must match contiguously, stay within one word, or follow runs
starting at word boundaries (`rfkm` or `revfer` for “Review ferry kill mode”).
Words can match different fields: `herdr unread` can match a tab and its task.
Exact matches rank first, then field/word prefixes, mid-word substrings, and
accepted matches with gaps; the weakest word decides a multi-word query's class.
Ties prefer title, name, tab, workspace, then path, followed by match score,
status, youngest age, and catalog order. Accepted letters are highlighted; a
hidden name, alternate title, or path appears when it explains a match. Empty-query ordering is
unchanged: All sorts by status then age, and filtered views by age alone.

The popup uses 90% of the available terminal height and up to 132 columns (the
paths popup: 110). On a narrower terminal it opens narrower, keeping 2 columns
free on each side of Herdr's pane area. Its size is set when it opens: if the
terminal grows while it is open, reopen it to use the new room. Preview appears beside results when
wide enough and below them in a narrow terminal. Slow preview reads never block
typing or navigation. Preview
shows the most recent captured output; toggling it or changing selection reads
a fresh snapshot.

## Paths (Alt-n)

zoxide already records every directory you `cd` into. Press **Alt-n**, type a
fragment (`herdr jump`, `skl/lib`, `~/code`), and press **Enter**: the full
absolute path is typed into the pane you opened the popup from, without
pressing Enter, and the same path is copied to the clipboard. It replaces
`cd somewhere && m_path`. To pick several, press **Tab** on each (searching
in between is fine), then press **Enter** once.

```text
 Paths · 2 marked, 1 hidden                                   zoxide · 477
 › herdr jump                                                        3/477
 ──────────────────────────────────────────────────────────────────────────
 ■ ~/code/side-projects/herdr-custom-plugins/my/herdr-jump
 › ~/code/side-projects/herdr-custom-plugins/my/herdr-jump/.wt/feat/jump-zoxide
   ~/llm/code/side-projects/herdr-custom-plugins/my/herdr-jump

 '/Users/shadowfax/Library/Application Support' /Users/shadowfax/code/side-pro…
 ↑↓ select   tab mark   enter paste + copy   ctrl-r reload   esc close
```

- The list is `zoxide query --list`, in zoxide's own frecency order. Rows show
  `~` for your home directory, with parent directories dimmed so the last
  directory stands out; long paths keep the match and the last directory.
- Search uses the same matcher as Alt-p. With a query, the weakest word's match
  class ranks first, then matches in the last directory beat matches only in its
  parents (`jump` prefers `…/jump` over `…/jump/sub`), then score, then zoxide's order.
- The line under the list is exactly what Enter types. Paths that a shell
  would split or expand (spaces, quotes, `$`, globs) are typed single-quoted
  (`'/Users/me/My Drive'`), which fish, bash and zsh read back as the original.
  For a single path the clipboard gets the raw path, as `m_path` did.
- **Tab** marks the selected path and moves down; **Shift-Tab** marks it and
  moves up (on a marked path, either one unmarks it, as in fzf). Marks survive
  new searches: search, Tab, search again, Tab, Enter. Marked rows show `■`
  and a bold name, like Alt-p's kill marks, and the heading counts them
  (`2 marked, 1 hidden` when the current search hides some). Enter types every
  marked path in the order you marked them, space-separated and quoted as
  above, and copies that same text. With nothing marked, Enter takes the
  selected row.
- Typing uses Herdr's `pane.send_input`, which wraps the text in bracketed
  paste when the program asked for it, so fish (even in vi normal mode), Claude
  Code and Codex insert it as text. It never presses Enter.
- **Escape** or **Ctrl-c** closes with nothing typed or copied. If the copy or
  the typing fails, the popup stays open and says exactly what did and didn't
  happen; Enter re-arms after you edit or move.
- No zoxide on Herdr's `PATH`, an empty zoxide database, or a failing command
  each show a one-line explanation; **Ctrl-r** runs the command again.

| Key | Action |
| --- | --- |
| Type | Search paths (including `~`); Backspace and Ctrl-u edit as in Alt-p |
| Up / Down, Ctrl-p / Ctrl-n | Select a path |
| Tab / Shift-Tab | Mark (or unmark) the selected path, then move down / up |
| Enter | Type the marked paths (or the selected one) into the source pane and copy them, then close |
| Ctrl-r | Run zoxide again |
| Escape / Ctrl-c | Close without typing or copying |

## Add a picker

Every picker is a block in [`pickers.toml`](pickers.toml): a source, the shared
matcher, a row style (`kind`), and what Enter does. Any program that prints one
item per line can be a source. For example, git branches typed into the pane:

```toml
[[picker]]
id = "branches"
title = "Branches"
key = "alt+b"                    # pick a key that's free in your setup
command = ["git", "branch", "--format=%(refname:short)"]
enter = ["paste"]                # "paste", "copy", or both, in order
empty = "No branches here."
```

1. Add the block to `pickers.toml`. Commands run without a shell, in the
   directory of the pane you opened the popup from, with a 5 s deadline.
   `kind = "path"` adds `~` display, last-directory-first matching, absolute
   paths and shell quoting. `multi = true` lets Tab and Shift-Tab mark several
   items; Enter then delivers them space-separated, in the order marked.
2. Add its action to `herdr-plugin.toml` (Herdr binds keys only to manifest
   actions; it rereads manifests on every invoke, so no relink is needed):

   ```toml
   [[actions]]
   id = "branches"
   title = "Paste a branch"
   contexts = ["pane"]
   command = ["./target/release/herdr-jump", "open", "branches"]
   ```
3. Rebuild and bind the key:

   ```sh
   cargo build --release --locked
   herdr plugin action invoke shadowfax.jump.install-keybindings
   ```

`cargo test` fails if a picker and its manifest action disagree. Lines with
control characters (tabs included) or invalid UTF-8 are skipped and counted,
since typing them into a shell could press keys; blank and duplicate lines are dropped.

## Build and install locally

Requires macOS or Linux, a Rust toolchain, and a compatible Herdr 0.9+ host with
`session.snapshot`, native plugin popups, `pane.focus`, and `tab.focus`.

From this checkout:

```sh
cargo build --release --locked
herdr plugin link .
herdr plugin action invoke shadowfax.jump.install-keybindings
```

The explicit installer binds every picker's `key` from `pickers.toml`: **Alt-p**
to the pane navigator and **Alt-n** to paths. It migrates Alt-p from the
built-in `goto` binding, preserves any other `goto` shortcuts, and leaves
unrelated settings and comments intact. It refuses conflicts with other
built-in or custom actions (modifier aliases such as `option+n` count). An
unset built-in `goto` keeps its inherited shortcut, and Alt-Shift-P remains
distinct from Alt-p.
Before changing the config, it saves the exact original to a sibling
`config.toml.jump-backup-*` file, preserves the config's permissions and symlink
target, and atomically replaces the contents. Repeated installation is a no-op.
`pickers.toml` owns the keys of Jump's actions: after a picker's `key` changes
there, rerunning the installer moves that picker's existing binding to the new
key (an older install's Alt-z becomes Alt-n), so the old key stops opening it.
Extra keys you bound to a picker yourself are left alone.
Herdr reloads configuration without restarting the server.

To try Jump after linking, before replacing Alt-p:

```sh
herdr plugin action invoke shadowfax.jump.open
```

For an offline configuration edit, the binary also supports
`install-keybindings --config /path/to/config.toml --no-reload`.

To remove the shortcuts, remove the `shadowfax.jump.open` and
`shadowfax.jump.paths` command blocks and restore `goto = "alt+p"` under
`[keys]`, then reload Herdr. To unlink the local
plugin, run `herdr plugin unlink shadowfax.jump`.

## Architecture

```text
key → manifest action → herdr-jump open <picker> → native popup (picker id + source pane in env)
        → pickers::find ─┬─ source = panes → live catalog → query → selected identity → revalidate → focus
                         │                                       → mark → confirm → revalidate each → close
                         │                                       → latest preview worker → selected preview
                         └─ command (lines) → command worker → items → query → Enter → copy, paste
shared by every picker: matcher · prompt (query editing) · ui (query line, highlights, path elision) · terminal session
```

- `pickers` is the registry, parsed from the embedded `pickers.toml`. The panes
  source keeps its own navigator; every other picker is configuration on the
  line engine. A test keeps each picker and its manifest action in agreement.
- `matcher` is the one fuzzy matcher (see search rules above); `prompt` is the
  one query editor (typing, paste, Backspace, Ctrl-u). Both pickers use both.
- `lines` is the line engine's state machine (items, ranking, selection, the
  Enter latch) and `lines_ui` renders it with `ui`'s palette, query line,
  highlights and path elision. Neither performs I/O.
- `command` runs a picker's argv on the load worker in the source pane's
  directory, stdin closed, with a 5 s deadline and 4 MiB / 50,000-line caps.
- `deliver` performs Enter's effects on the foreground loop only: `copy` pipes
  the raw value to `pbcopy` (or wl-copy/xclip/xsel; `HERDR_JUMP_COPY_COMMAND`
  overrides), and `paste` types the shell-safe form with `pane.send_input`.

- `catalog` projects host metadata into flat candidates and ranks it with
  Nucleo's Unicode fuzzy matcher. Pane and terminal IDs travel with each result.
  It excludes the picker's own pane and maps herdr's agent statuses. All view
  orders blocked, done, working, idle, then neutral; each group is youngest first
  with unknown ages last. Other views sort by age alone. Query tier and score
  take precedence, with stable source-workspace/tab order resolving ties.
- `age` recovers session starts using herdr-agents' transcript rules: Codex's
  matching `session_meta.payload.timestamp`, or Claude's matching non-sidechain
  root conversation record. It searches the standard and configured agent homes
  plus `$XDG_STATE_HOME/rtr/homes/{codex,claude}/*` (defaulting to
  `$HOME/.local/state`). A valid Codex UUIDv7 supplies a fallback; mtime never does.
  Discovery shares a 20,000-entry budget per refresh, descends at most four
  directories below each transcript root, and reads at most 256 KiB per head
  and 16 MiB total. Heads stream line by line and stop at the first matching
  start, charging only actual buffered reads to the shared budget. Only timestamps
  leave the module. This runs on the catalog worker and never blocks typing;
  refresh retries previously unknown sessions.
- `app` owns input, query, selection, views, marks, confirmation, and preview
  generations. `ui` only renders that state through Ratatui; it performs no host I/O.
- `picker` owns the terminal for every picker. For panes it dispatches two
  bounded read workers: one for catalog loading, one for preview. Each worker retains at most one pending
  request and one result. Stale previews cannot replace the current selection.
  It also redraws when the wall-clock second changes, so session ages advance
  while the popup stays open.
  Entering confirmation immediately draws and flushes its frame, then drains
  queued input (honoring resize and Ctrl-c) before arming the confirmation key.
  A typed-ahead `y` cannot authorize a kill before this display handoff.
- `host` uses the inherited `HERDR_SOCKET_PATH` directly. Each newline-delimited
  JSON request validates its response ID, shares a two-second connect/write/read
  deadline, and rejects replies above 8 MiB. Nonblocking reads preserve that
  deadline without resetting socket timeouts on a peer that has already closed
  (which can reject a buffered reply with EINVAL on macOS).
- `KillExecutor` is a non-Send foreground capability. Only explicit marks can
  enter confirmation; its frozen pane/terminal/tab/workspace identities are
  revalidated with `pane.get` immediately before each `pane.close`. The source
  and picker panes are protected both when marking and when executing. A moved
  or replaced target is refused; an absent target counts as already closed.
  Host error codes survive, including `confirmation_required`, which is never
  forced or retried. After execution the picker clears marks, leaves kill mode,
  refreshes, and shows a short result such as "Killed 2 panes". Partial results
  omit zero counts and name refused panes by task title (falling back to pane ID),
  with a reason and any host refusal code such as `confirmation_required`.
- Only the foreground picker commits focus. It revalidates pane, terminal, tab,
  and workspace identity, focuses the pane, then explicitly displays its tab.
  Popup teardown does not restore the source over a completed jump. Herdr's
  public focus operations also affect other clients attached to that server.

If a target closes, moves, or changes terminal, the popup shows an error and
refreshes candidates. Enter is disarmed until editing or navigation to avoid
repeated keys accepting a different result. Herdr has no conditional focus
transaction, so identity validation and focus are separate host operations.
The same host limitation applies to close: revalidation and mutation are separate
requests, so a concurrent change inside that interval cannot be made atomic by
this client. Confirmation never expands the set the user marked.

## Verification

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --release --locked
python3 tests/slow_host.py
```

Rust tests cover every agent state, bounded transcript fixtures (all roots are
injected temporary directories), age labels, view membership/counts/order, mark
lifetimes, frozen confirmation, its display latch, protected panes, and each kill
safety invariant. Many first-line session starts share a small read budget.
Fake sockets check per-target get/close ordering, replacement refusal, missing
panes, error codes, and closed peers with buffered replies. Run repeated
`cargo test --locked` passes to check the macOS socket regression.

The opt-in acceptance test runs the actual plugin shortcut in a private Herdr
server with an isolated home, config, state, and PTY. Python and pyte are test
dependencies only; production is entirely Rust.

```sh
uv run --with pyte==0.8.2 python tests/real_herdr.py --width 140 --evidence .llm/evidence/p3
uv run --with pyte==0.8.2 python tests/real_herdr.py --width 70 --evidence .llm/evidence/p3
uv run --with pyte==0.8.2 python tests/real_herdr.py --width 420 --evidence .llm/evidence/p3
```

It checks immediate fuzzy typing, preview, exact shell and agent targets across
tabs/workspaces, source input after Escape, empty results, and a closed target
with repeated Enter. It also creates three temporary sleep panes, confirms two
independently queried marks, and checks via API snapshots that exactly those two
closed. Screen assertions check marked rows, mark counts, exact review targets,
the cancellation footer and kill result. Typed-ahead confirmation is discarded;
cancel closes nothing and keeps the mark. The source stays visibly inert with
`0 marked` after Tab, captured before Enter; Escape restores browse mode.

Private Codex and Claude transcripts provide known start times for working,
blocked, done and idle agents, alongside an agent without a session and a plain
shell. The harness checks their glyphs, compact ages and row order inside the
popup, including blank unknown ages. Each ctrl-e step checks the active header
pill, counts and membership against the private server's agent list. Queries
stay within their view, and plain shells appear only in All. Empty-query jumps
also verify age-based ranking through exact pane IDs. Ctrl-r discovers a newly
created pane before Enter jumps to it.

Evidence includes raw terminal updates, reconstructed screens, checked popup
rows, transcript fixtures, host snapshots and per-view expectations. Narrow
notices are reconstructed across wrapped lines before checking their text,
error code and target. All inherited `HERDR_*` values are stripped, transcript
homes are isolated, and the test refuses a host socket outside its temporary
directory. It removes inherited `NO_COLOR` for this virtual terminal so the
active-view pill can be checked through pyte's cell attributes.

The paths picker has its own opt-in acceptance test, also in a private Herdr
server, with a private zoxide database and a file standing in for the clipboard:

```sh
uv run --with pyte==0.8.2 python tests/real_paths.py --width 140 --evidence .llm/evidence/paths
```

It starts from an older install's Alt-z binding and checks that the installer
moves it to the key in `pickers.toml` (Alt-z then does nothing). It presses the
real Alt-n and checks the rows against `zoxide query --list`, a fragment search
with its count, Enter typing the path at a `/bin/sh` prompt without running it
plus the copy, Escape typing and copying nothing, a path with a space typed
quoted (`cd` + `pwd` round-trip), multi-select (Tab, a new search, Tab and
Shift-Tab, then one Enter: `printf` receives the three marked paths in mark
order and the clipboard holds the same text), fish in vi normal mode receiving
the path as text, and the missing-zoxide and empty-database messages. Both harnesses run their private Herdr with sound off.

The `slow_host.py` check holds host reads open while the real picker runs: Escape
must still exit during catalog loading, and Enter must jump during a stalled
preview. It also queues more than 64 confirmation keys and a resize before the
picker can process Enter, proves none can close a pane, then confirms with a new
`y`. All cases use private fake sockets and PTYs.

## License

MIT.
