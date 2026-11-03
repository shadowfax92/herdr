# herdr-tools

One Herdr plugin, `shadowfax.tools`, run by one program, `herdr-tools`. It
replaces seven separate plugins with unchanged behavior, one config file, and
configurable popup sizes.

| Old plugin | Mode | Crate | Popup key(s) |
|---|---|---|---|
| `herdr-jump` | `find` | `tools-find` | `find` |
| `herdr-ferry` | `move` | `tools-move` | `move` |
| `herdr-talon` | `history` | `tools-history` | `history` |
| `herdr-menu` | `menu` | `tools-menu` | `menu` |
| `herdr-comments` | `comment` | `tools-comment` | `comment-capture`, `comment-review` |
| `herdr-layouts` | `layout` | `tools-layout` | `layout-grid` |
| `herdr-scratch` | `scratch` | `tools-scratch` | `scratch-<name>` (`scratch-nvim`, `scratch-shell`) |

The decision doc is `.llm/herdr-tools/DESIGN.md` (a git-excluded symlink in each checkout). This
README is the code-level contract: the exact tools-core interface and how a mode
crate is shaped.

## Architecture

```text
 herdr ── runs ./bin/herdr-tools <mode> <command…> for every action, popup pane,
   │      startup hook and event in herdr-plugin.toml
   ▼
 src/main.rs            clap dispatch only: <mode> → tools_<mode>::run(cmd)
   │                    plus `migrate-config` (src/migrate.rs) and `--version`
   ▼
 crates/<mode>          one crate per mode: its Command enum, run(), migrate_legacy()
   │                    and everything the old plugin did (TUI, matcher, herdr calls)
   ▼
 crates/core            tools-core: Host (CLI + socket client), Invocation, Config,
                        popup sizes, Paths, legacy action ids. No mode logic.
```

- `herdr-tools/` is its own Cargo workspace. The fork's root `Cargo.toml` never
  lists it, and cargo stops its upward search here.
- The plugin dir that Herdr links is laid out, prebuilt, at
  `${XDG_DATA_HOME:-~/.local/share}/herdr/plugins/shadowfax.tools/`:
  `herdr-plugin.toml`, `bin/herdr-tools`, `README.md`, `LICENSE`, `nvim/`.
  `herdr plugin link` never builds. The manifest's `[[build]]` exists only for
  `herdr plugin install` from GitHub.
- `nvim/<mode>/` holds runtime Neovim assets (`nvim/comment/review.lua`, the
  scratch Neovim plugin under `nvim/scratch/`). Modes find them under
  `Paths::root()`. The comment and scratch ports fill these in.

### Focus and hide are Herdr's, not the plugin's

Focus and hide are native to the herdr fork: the local server owns one scope,
the client applies it to every machine's panels, keys and notifications, and
the picker is a native overlay. A plugin sees only the local server, so
herdr-tools never stores or judges scope. It only does two things with it:

- **Opens it.** The menu's `focus`, `hide` (and retired `workspace_visibility`)
  rows ask Herdr to open its picker (`scope.pick`), and `focus_toggle` asks
  the owner's client to flip focus (`scope.toggle`). The menu's server can be
  a remote machine's, whose scope no client applies, so the menu never reads
  or sets the scope itself. See `crates/menu/README.md`.
- **Filters by it.** Find's pane navigator drops panes whose tab Herdr judges
  out of scope, from `scope.get`'s per-tab verdict ([`scope`](#scope-scopers)).

Everything else stays unscoped on purpose. Move and layout list hidden
workspaces as destinations, because moving into one is legitimate. History
captures only the pane it was invoked from, an explicit action like a jump,
so it has no list to filter. On a Herdr without `scope.get` nothing is
filtered and nothing is said; the menu's scope rows read "(unavailable)".

### Directory layout

```text
herdr-tools/
  Cargo.toml  Cargo.lock      root package `herdr-tools` (the bin) + [workspace]
  src/main.rs src/migrate.rs  dispatch, migrate-config
  herdr-plugin.toml           the one manifest, one `# --- <mode> ---` block per mode
  justfile                    ci, install, real, bin, fmt
  nvim/                       runtime Neovim assets, nvim/<mode>/…
  crates/core                 tools-core (+ examples/open_popup.rs)
  crates/<mode>               tools-find, tools-move, tools-history, tools-menu,
                              tools-comment, tools-layout, tools-scratch
  tests/support/isolated_herdr.py   reusable isolated-Herdr harness
  tests/real_scaffold.py      real-herdr check of install, actions, popup sizing
```

## tools-core interface

`use tools_core::{Host, HostError, Envelope, PaneArea, Invocation, PluginContext,
Config, Paths, PopupSize, PopupDims, PopupProfile, PopupConfig, ResolvedPopup,
ScopeTab, ScopeReport, ScopeFilter};`
Every fallible call returns `anyhow::Result`.

### Crate root

```rust
pub const PLUGIN_ID: &str = "shadowfax.tools";
pub fn self_command(mode: &str) -> anyhow::Result<std::process::Command>;
```

- `PLUGIN_ID` is the manifest id. Popups and config dirs hang off it.
- `self_command(mode)` returns `Command::new(current_exe()).arg(mode)`. Use it
  wherever an old plugin re-ran `current_exe() <command>`: ferry's
  `execute-kill`, scratch's `cleanup-worker`, comments' editor callback. The
  merged binary needs the mode prefix.

### `Host`: the one Herdr client (`host.rs`)

```rust
impl Host {
    pub fn from_env() -> Self;
    pub fn new(binary: impl Into<PathBuf>, socket: Option<PathBuf>) -> Self;
    pub fn with_timeout(self, timeout: Duration) -> Self;
    pub fn binary(&self) -> &Path;
    pub fn socket_path(&self) -> Option<&Path>;
    pub fn cli<I, S>(&self, args: I) -> Result<String>            where I: IntoIterator<Item = S>, S: AsRef<OsStr>;
    pub fn cli_json<T: DeserializeOwned, I, S>(&self, args: I) -> Result<T>;
    pub fn cli_output<I, S>(&self, args: I) -> Result<std::process::Output>;
    pub fn request(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value>;
    pub fn notify(&self, title: &str, body: &str) -> Result<()>;
    pub fn reload_config(&self) -> Result<()>;
    pub fn pane_area(&self, pane_id: &str) -> Result<PaneArea>;
    pub fn client_width(&self, pane_id: &str) -> Result<u16>;
    pub fn open_popup(&self, entrypoint: &str, popup_key: &str, env: &[(&str, &str)]) -> Result<Option<ResolvedPopup>>;
    pub fn open_popup_sized(&self, entrypoint: &str, size: Option<PopupDims>, env: &[(&str, &str)]) -> Result<()>;
}
pub struct Envelope<T> { pub result: T }
pub struct PaneArea { pub x: u16, pub y: u16, pub width: u16, pub height: u16 }
pub struct HostError { pub method: String, pub code: String, pub message: String }
```

- `from_env`: the binary is `HERDR_BIN_PATH` if it names an existing file (or
  is a bare name), else `herdr` on `PATH`. The socket is `HERDR_SOCKET_PATH`
  when it is absolute.
- `new`: an explicit binary and socket, for tests and custom sessions.
- `with_timeout`: the socket's whole-request deadline (default 5 s; jump used 2 s).
- `binary`: the Herdr executable, to hand to child processes.
- `socket_path`: the socket API path, if Herdr provided one.
- `cli`: runs `herdr <args>` and returns stdout. A non-zero exit becomes a
  `HostError` built from Herdr's stderr JSON `{"error":{code,message}}`, else
  code `command_failed`.
- `cli_json`: decodes the whole stdout. Use `cli_json::<Envelope<T>>(…)?.result`
  for the usual `{"result": …}` reply.
- `cli_output`: raw output whatever the exit status, for callers that classify
  failures themselves (comments' `ui_busy` / pane-missing wording).
- `request`: one socket API call, returning `result`. It carries a correlation
  id `herdr-tools-<pid>-<n>`, a deadline and an 8 MiB reply cap. An `error`
  reply becomes a `HostError`. This is jump's implementation, including the
  macOS nonblocking fix.
- `notify`: `herdr notification show <title> --body <body>`, with the body cut
  to 220 characters. Comments' `--sound none` is a plain `cli` call.
- `reload_config`: `herdr server reload-config`.
- `pane_area`: the `pane.layout` area of the pane's client.
- `client_width`: `area.x + area.width`, the same number talon, comments and
  scratch matched profiles on. On Herdr 0.9.5 it excludes the sidebar: a
  200-column terminal reports 174.
- `open_popup`: opens plugin pane `entrypoint` as a focused popup. The size for
  `popup_key` comes from `Config::current()` and the invocation's source-pane
  client width. Unconfigured keys send no size, so Herdr uses the manifest pane's
  default per dimension. Returns the configured size, if any.
- `open_popup_sized`: the same open with an explicit size (`None` means the
  manifest default). Use it for sizes a mode computes or narrows: jump's
  per-picker width and margin, menu's item-count height. Resolve with
  `Config::current()?.popup(key, width)` first, so config still wins.
- `HostError`: `Display` is `"{method} ({code}): {message}"`. Downcast with
  `error.downcast_ref::<HostError>()` and decide on `code`, never on the text.
  `is_unknown_method()` is the one exception, and it lives here so no mode
  repeats it: Herdr 0.9.5 rejects a method it lacks while decoding the request,
  as `invalid_request` with ``unknown variant `<method>` ``, and retired methods
  answer `unknown_method`. Any other `invalid_request` (bad params) is a real
  error.

### `scope` (`scope.rs`)

```rust
pub const SCOPE_GET: &str = "scope.get";
pub struct ScopeTab { pub workspace_id: String, pub tab_id: String, pub admitted: bool }
pub struct ScopeFocus { pub active: bool, pub targets: Vec<serde_json::Value> }
pub struct ScopeState { pub focus: ScopeFocus, pub hidden: Vec<serde_json::Value> }
pub struct ScopeLocal { pub tabs: Vec<ScopeTab> }
pub struct ScopeReport { pub scope: ScopeState, pub local: ScopeLocal }
impl ScopeReport { pub fn from_result(result: Value) -> Result<Self>; pub fn focus_engaged(&self) -> bool; }
pub struct ScopeFilter { /* tab ids judged out of scope */ }
impl ScopeFilter { pub fn new(tabs: impl IntoIterator<Item = ScopeTab>) -> Self;
                   pub fn admits_tab(&self, tab_id: &str) -> bool;
                   pub fn admits_everything(&self) -> bool; }
impl Host {
    pub fn scope(&self) -> Result<Option<ScopeReport>>;
    pub fn scope_tabs(&self) -> Result<Option<Vec<ScopeTab>>>;
    pub fn scope_filter(&self) -> ScopeFilter;
}
pub fn is_unknown_method(error: &anyhow::Error) -> bool;
```

- `scope`: one `scope.get`. The reply is
  `{"type":"scope","scope":{"focus":{active,targets},"hidden":[…]},"local":{"tabs":[…]}}`.
  `Ok(None)` when this Herdr does not know the method; other failures are
  errors. Targets stay opaque JSON: modes count or forward them, never match
  them.
- `scope_tabs`: `scope`'s `local.tabs`, the server's verdict per local tab.
  `None` means no scope: filter nothing.
- `scope_filter`: what a pane list applies. It never fails: no scope, a timeout
  or a malformed reply all admit every tab, because a visibility filter must
  never cost a working popup. A tab missing from the report (created after the
  read) is admitted.
- `ScopeReport::focus_engaged`: focus is on and has targets. An active focus
  with no targets narrows nothing.
- `is_unknown_method`: `HostError::is_unknown_method` through any anyhow
  context.

### `Invocation`: why this process started (`invocation.rs`)

```rust
pub struct Invocation {
    pub context: Option<PluginContext>,   // HERDR_PLUGIN_CONTEXT_JSON
    pub action_id: Option<String>,        // HERDR_PLUGIN_ACTION_ID
    pub entrypoint_id: Option<String>,    // HERDR_PLUGIN_ENTRYPOINT_ID
    pub event: Option<serde_json::Value>, // HERDR_PLUGIN_EVENT_JSON
}
impl Invocation {
    pub fn current() -> Result<&'static Invocation>;
    pub fn from_env() -> Result<Self>;
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<OsString>) -> Result<Self>;
    pub fn from_context_json(context_json: &str) -> Result<Self>;
    pub fn source_pane(&self) -> Result<&str>;
    pub fn source_pane_or_env(&self) -> Result<String>;
    pub fn source_cwd(&self) -> Option<&Path>;
    pub fn selected_text(&self) -> Option<&str>;
}
pub struct PluginContext {
    pub workspace_id, workspace_label, workspace_cwd, tab_id, tab_label,
        focused_pane_id, focused_pane_cwd, focused_pane_agent, focused_pane_status,
        selected_text, invocation_source, correlation_id, clicked_url,
        link_handler_id: Option<String>,
    pub worktree: Option<serde_json::Value>,
}
```

- `current`: the process-wide invocation, parsed once. Malformed JSON is an
  error on every call.
- `from_env`: parses without caching.
- `from_lookup`: the same parse over an injected environment, for tests.
- `from_context_json`: an invocation holding only a context, for tests.
- `source_pane`: `focused_pane_id`, trimmed, non-empty, with no control
  characters.
- `source_pane_or_env`: `source_pane`, else `HERDR_PANE_ID` (jump and layouts
  did this).
- `source_cwd`: `focused_pane_cwd` when it is not blank.
- `selected_text`: the copy-mode selection (comments' `selection` context).

Popups cannot see their caller: Herdr strips `HERDR_PANE_ID` from them. A mode
passes the source pane in the popup's `env` (`HERDR_FERRY_SOURCE_PANE_ID`,
`HERDR_JUMP_SOURCE_PANE`, …). Keep the old variable names unless they name the
old plugin.

### `Config`: the one config file (`config.rs`)

```rust
pub const SECTIONS: &[&str] = &["popup", "find", "move", "history", "menu", "comment", "layout", "scratch"];
impl Config {
    pub fn current() -> Result<&'static Config>;
    pub fn load(path: &Path) -> Result<Self>;
    pub fn parse(source: &str) -> Result<Self>;
    pub fn path(&self) -> Option<&Path>;
    pub fn section<T: DeserializeOwned + Default>(&self, name: &str) -> Result<T>;
    pub fn section_table(&self, name: &str) -> Result<Option<&toml::Table>>;
    pub fn popup_config(&self) -> &PopupConfig;
    pub fn popup(&self, key: &str, client_width: Option<u16>) -> Option<ResolvedPopup>;
}
```

- `current`: `Paths::from_env().config_file()`, loaded once. A broken file is an
  error on every call, never silently replaced by defaults.
- `load`: a missing file is the all-defaults config.
- `parse`: text with no file behind it. An unknown top-level table is an error
  naming the valid ones, and `[popup]` is validated.
- `path`: the file the config came from.
- `section`: deserializes `[name]`, or `T::default()` when absent. Errors name
  the table and file. Strictness is the mode's choice
  (`#[serde(default, deny_unknown_fields)]`).
- `section_table`: the raw table, for field-by-field readers.
- `popup_config`: the parsed `[popup]`.
- `popup`: resolves a popup key, as described below.

### Popup sizes (`popup.rs`)

```rust
pub enum PopupSize { Cells(u16), Percent(u8) }   // TOML/JSON: 80 or "80%"; FromStr also takes "80"
pub struct PopupDims { pub width: PopupSize, pub height: PopupSize }
pub struct PopupProfile { pub name: String, pub min_client_width: Option<u16>,
                          pub max_client_width: Option<u16>, pub size: BTreeMap<String, PopupDims> }
pub struct PopupConfig { pub size: BTreeMap<String, PopupDims>, pub profile: Vec<PopupProfile> }
pub struct ResolvedPopup { pub dims: PopupDims, pub profile: Option<String> }
impl PopupProfile { pub fn matches(&self, width: u16) -> bool; }
impl PopupConfig  { pub fn validate(&self) -> Result<()>;
                    pub fn resolve(&self, key: &str, client_width: Option<u16>) -> Option<ResolvedPopup>; }
```

`PopupSize` serializes the way Herdr's manifest does: an integer is cells and
`"N%"` is a percentage. Cells must be positive and percentages 1–100. Its
`Display` gives `80` or `80%`, the `--width` spelling. For a popup key and a
client width, the size is:

1. from the first `[[popup.profile]]` whose width range matches **and** that
   sizes this key (`profile = Some(name)`);
2. else from `[popup.size].<key>` (`profile = None`);
3. else `None`, and `open_popup` sends no size, so Herdr applies the manifest
   pane's `width`/`height`.

Skipping a matching profile that lacks the key is deliberate. Every old plugin's
profiles sized all of its own popups, so one merged file picks the same profile
for each mode as its old file did, and one mode's profiles never shadow
another's. An unknown client width (the layout read failed) skips profiles.
Each entry replaces both dimensions. Partial entries are rejected.

```toml
[popup.size]
find = { width = 110, height = "90%" }
comment-capture = { width = "90%", height = "90%" }

[[popup.profile]]
name = "laptop"
max_client_width = 310
size.comment-capture = { width = "95%", height = "90%" }
size.scratch-nvim = { width = "95%", height = "95%" }
```

### `Paths` (`paths.rs`)

```rust
impl Paths {
    pub fn from_env() -> Self;
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<OsString>, current_exe: Option<&Path>) -> Self;
    pub fn config_dir(&self) -> &Path;
    pub fn config_file(&self) -> PathBuf;
    pub fn state_dir(&self) -> &Path;
    pub fn root(&self) -> Result<&Path>;
    pub fn legacy_config_root(&self) -> PathBuf;
}
```

- `from_env`: `HERDR_PLUGIN_CONFIG_DIR`, `HERDR_PLUGIN_STATE_DIR` and
  `HERDR_PLUGIN_ROOT`. In a shell it falls back to Herdr's own XDG rules:
  `$XDG_CONFIG_HOME/herdr/plugins/config/shadowfax.tools`,
  `$XDG_STATE_HOME/herdr/plugins/shadowfax.tools`, and for the root the dir
  above `bin/herdr-tools`.
- `from_lookup`: the same over an injected environment, for tests.
- `config_dir`: user-owned config.
- `config_file`: `<config_dir>/config.toml`.
- `state_dir`: private runtime state (stores, sockets, locks). This is a new
  dir, not the old plugins'. State that must survive the cutover is
  `migrate_legacy`'s to document.
- `root`: the install dir. Use it for `nvim/<mode>/…`, never `current_dir()`,
  because a popup's cwd is the source pane's.
- `legacy_config_root`: `config_dir`'s parent, holding the old
  `shadowfax.<old>/` config dirs.

### `legacy` (`legacy.rs`)

```rust
pub const ACTION_IDS: &[(&str, &str)];       // DESIGN.md's old → new table
pub const DROPPED_ACTION_IDS: &[&str];        // installer actions with no replacement
pub fn rewrite_action_id(old: &str) -> Option<&'static str>;
pub fn is_dropped_action_id(old: &str) -> bool;
pub fn rewrite_action_ids(text: &str) -> Cow<'_, str>;
```

- `rewrite_action_id`: e.g. `shadowfax.scratch.reap` → `shadowfax.tools.scratch-reap`.
- `is_dropped_action_id`: true for `install-keybindings`, menu `install` and the
  like.
- `rewrite_action_ids`: rewrites every whole old id inside a string, such as a
  menu item's `herdr plugin action invoke …`.

### Test doubles (`tools_core::testing`, feature `test-util`)

Every mode crate already has
`tools-core = { workspace = true, features = ["test-util"] }` in
`[dev-dependencies]`.

```rust
pub fn fake_host(reply: impl Fn(Value) -> Value + Send + 'static) -> (tempfile::TempDir, Host);
pub fn ok(request: &Value, result: Value) -> Value;
pub fn fake_cli(dir: &Path, body: &str) -> PathBuf;
```

- `fake_host`: jump's fake socket. `reply` gets the whole
  `{id, method, params}` request and returns the whole response, so a test can
  forge ids. The server lives until the tempdir drops. Jump's host tests port
  unchanged.
- `ok`: `{"id": request.id, "result": result}`.
- `fake_cli`: an executable `#!/bin/sh` stand-in for `herdr`. Pass it to
  `Host::new` to test CLI calls.

## A mode crate

Every `crates/<mode>` starts as a stub with this shape. Port workers fill it in
and keep the three public items:

```rust
//! The `<mode>` mode of herdr-tools … Ported from /…/my/herdr-<old>/ …
pub const MODE: &str = "<mode>";

#[derive(Debug, clap::Subcommand)]
pub enum Command { /* the old plugin's subcommands minus installers */ }

pub fn run(command: Command) -> anyhow::Result<()>;          // stub: bail!("<mode> is not ported yet")
pub fn migrate_legacy(legacy_config_root: &Path) -> anyhow::Result<toml::Table>;  // stub: empty table
```

- **Command.** `src/main.rs` mounts it as `herdr-tools <mode> <command>`. The
  stub already declares the old variants, minus installers. Keep names stable
  for manifest commands: `every_manifest_command_parses` checks every manifest
  `command` against the clap tree. Layout's `Grid { size: String }` is a
  placeholder for the old `GridSize`.
- **run.** Everything below dispatch belongs to the mode: TUI, matcher, mode
  herdr calls built on `Host::cli`/`cli_json`/`request`, popups through
  `Host::open_popup`. `main` prints errors as `herdr-tools <mode>: {error:#}`
  on stderr, which Herdr records in `herdr plugin log`. Modes that notified on
  failure (ferry, menu, comments) keep doing that inside `run`, with
  `Host::notify`.
- **migrate_legacy.** Returns this mode's fragment of the new `config.toml` from
  the old config under `legacy_config_root/shadowfax.<old>/`:
  - Its own `[<mode>]` table, plus any `[popup.size]` / `[[popup.profile]]`
    entries for its keys.
  - Where the old plugin had **built-in** default sizes or profiles that applied
    with no config, emit them explicitly. Talon applied its laptop / ultrawide
    profiles when its file set none, which is the owner's case today. Comments
    and scratch wrote a default YAML on first run, and that no longer happens:
    without config, a fresh install gets the manifest defaults.
  - `migrate-config` merges fragments. Profiles with the same name and width
    range combine their `size` maps, and any other key two modes set
    differently is an error. It also rewrites old action ids inside `[menu]`
    strings.
- **Manifest.** Edit only your `# --- <mode> ---` block in `herdr-plugin.toml`.
  Commands are `./bin/herdr-tools <mode> …`. Pane ids are the popup keys
  (`find`, `move`, `history`, `menu`, `comment-capture`, `comment-review`,
  `layout-grid`, `scratch`). Scratch's one `scratch` pane is sized per scratch
  with key `scratch-<name>`. `manifest_ids_match_the_design` pins the action
  and pane ids.
- **Dependencies.** Each stub already depends on its old plugin's crates, so
  `Cargo.lock` is complete and a port should not need a lockfile change. Drop
  what you end up not using.
- **Strings.** Titles that named an old plugin are renamed in the manifest:
  Jump → Find, Ferry → Move / "Move or kill panes", Talon → History /
  "Browse pane history". Rename user-facing strings only where they name an old
  plugin or old action id.

## Dependencies

`[workspace.dependencies]` is the union of the seven old `Cargo.toml`s.
`Cargo.lock` was seeded from herdr-jump's lock and pinned to the old locks'
versions. Where requirements disagreed, the newest compatible one won:

| Crate | Old requirements | Chosen | Locked |
|---|---|---|---|
| clap | 4.6 (jump, ferry, talon, menu), 4.5 (comments, layouts, scratch) | 4.6 | 4.6.7 |
| toml_edit | 0.24 (+`serde` in jump), 0.22 (scratch) | 0.24 + `serde` | 0.24.1 |
| uuid | 1.22, 1.18 (comments) | 1.22 | 1.26.1 |
| tempfile | 3.23, 3.20 (comments) | 3.23 | 3.27.0 |
| socket2 | 0.6 (jump), 0.6.5 (menu) | 0.6.5 | 0.6.5 |
| unicode-segmentation | 1.12 (jump), 1.13 (talon) | 1.13 | 1.13.3 |
| ratatui | 0.30.0, 0.30 (comments) | 0.30.0 | 0.30.2 |

The rest are as the old plugins had them: anyhow 1.0, ansi-to-tui 8.0.1,
chrono 0.4 (std only), crossterm 0.29, fs4 1, libc 0.2, noyalib 0.0.18,
nucleo-matcher 0.3.1, regex 1.12, serde 1.0, serde_json 1.0, sha2 0.10,
static_assertions 1.1, thiserror 2.0, toml 1.1 (locked 1.1.4, as talon and menu
had), unicode-normalization 0.1, unicode-width 0.2. Scratch moves from
toml_edit 0.22 to 0.24, and it only used `DocumentMut` in one test.

## Checks

```sh
just -f herdr-tools/justfile ci     # fmt --check, clippy --workspace --all-targets -D warnings, tests
just -f herdr-tools/justfile real   # release build + tests/real_scaffold.py in an isolated Herdr
```

`ci` is the gate before every commit. From the fork root,
`cargo metadata --no-deps --format-version 1` and `cargo fmt --check` must keep
passing: the root workspace never sees `herdr-tools/`.

The integration branch `herdr-tools` has the same name as this directory, so
`git log` and `git diff` need `herdr-tools --` or `refs/heads/herdr-tools`.
`git merge` and `git rebase` are unaffected.

## Install

```sh
just -f herdr-tools/justfile install
```

This builds release and stages `herdr-plugin.toml`, `bin/herdr-tools`,
`README.md`, `LICENSE` and `nvim/` beside the destination. It then swaps them in
by rename at `${XDG_DATA_HOME:-~/.local/share}/herdr/plugins/shadowfax.tools`,
and runs `herdr plugin link <dir>` unless `herdr plugin list --json` already
shows `shadowfax.tools` with that `plugin_root`. Linking the same id from another
dir replaces that link, and re-linking is idempotent.

It talks to whatever Herdr server the environment reaches. **Never run it
against the owner's real config from an agent.** Use the isolated recipe below.

## Config migration

```sh
herdr-tools migrate-config [--legacy-root DIR] [--force] [--stdout]
```

This merges every mode's `migrate_legacy` fragment from `DIR` (default
`~/.config/herdr/plugins/config`) and rewrites old action ids in `[menu]`. It
validates the result as a `Config`, then writes `Paths::config_file()` by
same-directory rename. It refuses to replace an existing file without
`--force`, and with `--force` it keeps the old file as
`config.toml.before-migrate-<unix secs>`. `--stdout` prints the result instead,
for the cutover's dry run. It respects `HERDR_PLUGIN_CONFIG_DIR` and
`XDG_CONFIG_HOME`, so point those at a temp dir to try it.

## Testing against a real herdr

Never use the default session or `~/.config/herdr`. Every recipe here gives
Herdr temp XDG dirs and its own session, and strips the inherited `HERDR_*`
variables. Inside a Herdr pane, `HERDR_SOCKET_PATH` points at the owner's live
server, and any `herdr` command would reach it.

### From Python (what the real tests use)

`tests/support/isolated_herdr.py` was distilled from the old plugins'
`tests/real_*.py`. It starts a PTY-attached client (default 50×200) in a temp
root, refuses any socket outside that root, drains the PTY on a background
thread, and stops the server and removes the root on exit. Set
`KEEP_HERDR_ROOT=1` to keep the root for inspection.

```python
import sys; sys.path.insert(0, "tests/support")   # from herdr-tools/
from isolated_herdr import IsolatedHerdr

with IsolatedHerdr(rows=50, cols=200) as herdr:
    plugin_dir = herdr.install()                  # `just install` into the temp XDG_DATA_HOME, linked
    pane = herdr.cli("pane", "list")["result"]["panes"][0]["pane_id"]
    herdr.cli("plugin", "action", "invoke", "shadowfax.tools.move")
    herdr.plugin_log()["result"]["logs"]          # each action's status, exit code, stdout, stderr
    herdr.write_plugin_config("[popup.size]\nmove = { width = 60, height = 20 }\n")
    herdr.run([str(plugin_dir / "bin/herdr-tools"), "move", "open"],
              {"HERDR_PLUGIN_CONTEXT_JSON": f'{{"focused_pane_id":"{pane}"}}'})
    herdr.type("\x1b")                             # keystrokes to the client
    herdr.drain(0.3)                               # client output since the last call
```

`herdr.env` is the isolated environment for any subprocess. It already includes
`HERDR_SOCKET_PATH` for the isolated server.

### From a shell (copy-paste)

```sh
export HT_ROOT="$(mktemp -d /tmp/herdr-tools-dev.XXXXXX)"
iso() { env -i HOME="$HOME" PATH="$PATH" TERM=xterm-256color \
  XDG_CONFIG_HOME="$HT_ROOT/config" XDG_STATE_HOME="$HT_ROOT/state" \
  XDG_DATA_HOME="$HT_ROOT/data" XDG_RUNTIME_DIR="$HT_ROOT/run" \
  HERDR_SESSION=ht-dev "$@"; }
mkdir -p "$HT_ROOT/config/herdr"
printf 'onboarding = false\n[terminal]\ndefault_shell = "/bin/sh"\n' > "$HT_ROOT/config/herdr/config.toml"

iso herdr server > "$HT_ROOT/server.out" 2>&1 &   # headless; or run `iso herdr` in a spare terminal to watch it
sleep 1; iso herdr status --json                   # server.socket must be under $HT_ROOT
iso herdr workspace create --cwd /tmp              # a headless server starts with no workspace
iso just -f herdr-tools/justfile install           # lays out $HT_ROOT/data/herdr/plugins/shadowfax.tools, links it
iso herdr plugin action invoke shadowfax.tools.layout-equalize
iso herdr plugin log --plugin shadowfax.tools      # status, exit_code, stderr per run

# Run a mode command with a plugin's environment:
PANE=$(iso herdr pane list | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["panes"][0]["pane_id"])')
CFG="$HT_ROOT/config/herdr/plugins/config/shadowfax.tools"; mkdir -p "$CFG"
iso env HERDR_SOCKET_PATH="$HT_ROOT/config/herdr/sessions/ht-dev/herdr.sock" \
  HERDR_PLUGIN_CONFIG_DIR="$CFG" HERDR_PLUGIN_CONTEXT_JSON="{\"focused_pane_id\":\"$PANE\"}" \
  "$HT_ROOT/data/herdr/plugins/shadowfax.tools/bin/herdr-tools" <mode> <command>

iso herdr server stop; rm -rf "$HT_ROOT"
```

A headless server sizes popups against its 120×40 default. Attach a client
(`iso herdr`) for a real terminal size.

### What the scaffold check saw (Herdr 0.9.5, macOS, 50×200 client)

`just real` runs `tests/real_scaffold.py`:

```text
PASS install + link: /private/tmp/herdr-tools-real-…/data/herdr/plugins/shadowfax.tools (herdr-tools 0.1.0); a second install keeps the link
PASS stub action: status=failed exit=1 stderr='herdr-tools move: move is not ported yet'
PASS popup move: config {'width': 60, 'height': 20, 'profile': None} -> outer 60x20 -> Herdr PTY 57x18
PASS popup history: config {'width': '50%', 'height': '40%', 'profile': 'test-narrow'} -> outer 50%x40% -> Herdr PTY 84x17
PASS popup layout-grid: config None -> outer 80x24 -> Herdr PTY 77x22
```

- **Linking prebuilt works.** `herdr plugin link` accepts the laid-out dir
  (online or offline) and runs no build. `plugin list --json` reports
  `plugin_root`, which `install` uses to skip a redundant link.
- **A failing action** surfaces in `herdr plugin log` as `status: "failed"` with
  the exit code and `herdr-tools <mode>: <error>` on stderr. The action invoke
  itself still returns success.
- **Popup sizing works through `Host::open_popup`.** The client's pane area was
  174×49 (`x = 0`, so `client_width` = 174 on a 200-column terminal). Herdr
  spawns the popup PTY at the outer size minus 2 rows and 3 columns (border plus
  scrollbar): `60×20` gave 57×18, and the profile's `50%×40%` of 174×49 gave
  84×17. An unconfigured key fell back to the manifest's 80×24 and gave 77×22.
- **Popups are not visible in `pane list`, `pane layout` or `api snapshot`** on
  0.9.5. The size check reads the server log's
  `event="pane.spawn.start" … rows=R cols=C` record for the popup, at
  `$XDG_CONFIG_HOME/herdr/sessions/<session>/herdr-server.log`. Run
  `crates/core/examples/open_popup.rs` (`<entrypoint> <popup_key> [KEY=VALUE…]`,
  with a plugin env) to open any popup the way a mode does.
- **A popup closes when its process exits**, so stub popups vanish at once and
  the next open succeeds.
