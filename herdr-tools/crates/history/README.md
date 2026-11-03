# history

**Browse focused terminal history and copy values with short keyboard hints.**

The `history` mode of [herdr-tools](../../README.md), formerly the Talon plugin
(`shadowfax.talon`). Behavior is unchanged; only the action id, the config file
and the popup sizing moved.

Bind the action, then press the key in any terminal pane. History freezes up to
1,000 recent rendered terminal rows from that pane, unwraps soft wraps, and opens
the result in a centered native popup. Detected paths, URLs, hashes, IPs, and
other useful values receive compact hints; type one to copy its value and close
immediately.

The popup also works as a keyboard-driven history viewer when no value is detected:

- **Focused by design** — only the pane where History was invoked is captured, whether it is tiled or maximized.
- **Clean reflow** — long logical lines wrap to the popup width and reflow in place after a resize.
- **Exact visual copy** — characterwise and linewise selections preserve real source newlines without inventing newlines at soft wraps.
- **Viewport-local hints** — only visible targets receive labels, keeping hints short even across a large history capture.
- **Searchable history** — `/`, `n`, and `N` make older output quick to find.
- **Responsive popup** — `[popup.size]` and `[[popup.profile]]` size it per client width.
- **Frozen and private** — the source can keep running while a one-shot owner-only snapshot is open.

Clipboard copying uses `pbcopy` on macOS, [wl-copy](https://github.com/bugaevc/wl-clipboard) on Wayland, or [xclip](https://github.com/astrand/xclip)/[xsel](https://github.com/kfish/xsel) on X11. Install the matching Linux utility. WSL needs a graphical clipboard session such as WSLg. History preserves Unicode and trailing newlines and reports unavailable clipboard services in the popup.

## Keybinding

herdr-tools installs no keybindings. Add this to `~/.config/herdr/config.toml`
(Talon's installer bound the same key to `shadowfax.talon.launch`):

```toml
[[keys.command]]
key = "prefix+g"
type = "plugin_action"
command = "shadowfax.tools.history"
description = "Browse pane history"
```

`prefix+g` opens the popup; it does not toggle an open popup. Herdr sends every key to the active modal, so copy a value or close History with `q`, `Esc`, or `Ctrl-c` before invoking the shortcut again.

| Command | Run by |
| --- | --- |
| `herdr-tools history open` | action `shadowfax.tools.history` (Talon's `launch`) |
| `herdr-tools history picker` | popup pane `history` |

## Workflow

History opens at the newest captured output. The footer always shows the active mode and its important controls.

### Browse

| Key | Result |
| --- | --- |
| visible hint | Copy that target and close |
| arrows or `h`, `j`, `k`, `l` | Move through wrapped history |
| `PageUp`, `PageDown` | Move one page |
| `Ctrl-u`, `Ctrl-d` | Move half a page |
| `g`, `G` | Jump to the beginning or end |
| `0`, `$` | Move to the beginning or end of a logical line |
| `v`, `V` | Begin character or line selection |
| `/` | Search the capture |
| `n`, `N` | Go to the next or previous search match |
| `Esc`, `q`, or `Ctrl-c` | Close |

Hints are reassigned deterministically when the viewport changes. Partial hint input is cleared by navigation, so the labels on screen are always the complete active set.

### Select and copy

After `v` or `V`, use the same movement keys to extend the selection.

| Key | Result |
| --- | --- |
| `y` | Copy the exact selection and close |
| `Esc` | Cancel the selection and return to Browse |

A selection can cross any number of wrapped visual rows. History copies a newline only where the original captured output had one.

### Search

Press `/`, type a case-sensitive query, then press `Enter`. The first match at or below the current logical line is previewed as the query changes. After accepting, use `n` and `N` to cycle with wraparound. `Esc` cancels the search and restores the original cursor.

If copying fails, History keeps the popup open and shows the error in the footer so the selection is not lost.

## Recognized targets

History includes patterns for:

- URLs, IP addresses, UUIDs, hexadecimal values, long numbers, and SHA-like hashes
- absolute, relative, and home-relative paths, including `path:line` values
- Git SSH remotes, status paths, branch names, and diff paths
- common Kubernetes resource names

Repeated values share one target. Overlapping matches resolve deterministically: the leftmost match wins, then earlier pattern priority. Custom patterns are ordered before built-ins.

## Configure

Settings live in the one herdr-tools config,
`~/.config/herdr/plugins/config/shadowfax.tools/config.toml`: hints and patterns
under `[history]`, the popup size under `[popup]` with popup key `history`. The
file is read on every launch, so edits do not require a Herdr reload.
`herdr-tools migrate-config` carries Talon's old
`shadowfax.talon/config.toml` over.

### Hints and patterns

The default hint alphabet is `asdfwerzxcuioptbm`. An alphabet must contain at least two unique lowercase ASCII letters and cannot contain the normal-mode keys `g`, `h`, `j`, `k`, `l`, `n`, `q`, `v`, or `y`. History migrates Talon 0.1's exact default (`asdfwerzxvjkluopghtyb`) automatically; other alphabets with reserved keys are refused at launch.

`enabled_builtin_patterns` accepts:

```text
ip, uuid, sha, digit, url, path, hex,
kubernetes, git-status, git-status-branch, diff
```

Add ordered regular expressions with `[[history.patterns]]`. When a regex has a
named `match` capture, History labels and copies only that capture.

### Popup size

With no `[popup]` entry for `history`, the popup uses the manifest's 90% × 90%.
`[popup.size].history` sets a base size; `[[popup.profile]]` entries are checked
in order on the client's width in cells, and the first matching profile that
sizes `history` wins. Sizes are a cell count (`120`) or a percentage from 1% to
100% (`"90%"`).

Talon applied the laptop / partial-ultrawide / full-ultrawide profiles below
whenever its file set none; `migrate-config` writes them out explicitly. With
them, widths from 351 through 399 cells use the base size.

### Example

<!-- example-config -->
This example is loaded by the `matcher` tests, so it always parses:

```toml
[history]
alphabet = "asdfwerzxcuioptbm"
enabled_builtin_patterns = [
  "ip",
  "uuid",
  "sha",
  "digit",
  "url",
  "path",
  "hex",
  "kubernetes",
  "git-status",
  "git-status-branch",
  "diff",
]

[[history.patterns]]
name = "git-ssh"
regex = 'git@[a-zA-Z0-9.-]+:[a-zA-Z0-9/.-]+\.git'

[[history.patterns]]
name = "user-uuid"
regex = '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}'

[[history.patterns]]
name = "user-sha"
regex = '[0-9a-f]{7,40}'

[[history.patterns]]
name = "file-line"
regex = '/[^ \t\r\n]+:[0-9]+'

[[history.patterns]]
name = "hostname"
regex = '(?i)\b(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,63}\b'

[[history.patterns]]
name = "ticket"
regex = "TKT-[0-9]+"

[[history.patterns]]
name = "captured-ticket"
regex = "ticket=(?<match>TKT-[0-9]+)"

[popup.size]
history = { width = "90%", height = "90%" }

[[popup.profile]]
name = "laptop"
max_client_width = 310
size.history = { width = "95%", height = "90%" }

[[popup.profile]]
name = "partial-ultrawide"
max_client_width = 350
size.history = { width = "90%", height = "90%" }

[[popup.profile]]
name = "full-ultrawide"
min_client_width = 400
size.history = { width = "70%", height = "90%" }
```

## How it works

`history open` reads plain and ANSI `recent-unwrapped` output from the invoking pane. Plain text supplies stable match and selection coordinates; ANSI spans preserve terminal styling when the two representations align. A custom terminal-cell wrap table maps every visual row back to its logical source range, so resize, highlight, search, and selection all use one coordinate model.

The open action records the frozen capture in a private one-shot handoff under the plugin state dir (`<state>/history/runs/`) and opens the picker as a focused Herdr popup. The picker claims and removes the handoff, writes successful completions through the host clipboard utility, then exits so Herdr dismisses the popup. Herdr routes every key to an open modal popup, so close History with `q`, `Esc`, or `Ctrl-c`; a launch attempted while another modal is open fails safely without closing it.

## Limits

- Herdr caps the capture at 1,000 recent rendered terminal rows before unwrapping soft wraps.
- The snapshot is frozen; output produced after launch is intentionally absent.
- History does not change the source pane's scroll or zoom state and does not reuse Herdr's native Copy mode.
- Completion copies to the clipboard only. It does not paste, open, submit, or execute the selected text.
- Selection is keyboard-driven; mouse selection is not implemented.

## Tests

```sh
just -f herdr-tools/justfile ci                          # every Rust test, this crate's included
python3 herdr-tools/crates/history/tests/real_history.py # end to end in an isolated Herdr
```

`real_history.py` installs into a throwaway Herdr (temp XDG dirs, its own
session) and fakes `pbcopy`, so it never touches the live Herdr or clipboard.
It is not wired into `just real`, which runs only the scaffold check.

## Attribution

History (formerly Talon) is behaviorally inspired by [tmux-fingers](https://github.com/Morantron/tmux-fingers). The implementation and built-in target categories are independent; see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
