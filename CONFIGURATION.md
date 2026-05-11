# configuration

herdr reads config from:

```text
~/.config/herdr/config.toml
```

Named sessions share this config file. Sessions are runtime/socket namespaces, not workspace replacements; per-session sockets and persistent runtime state are separate:

```text
~/.config/herdr/session.json
~/.config/herdr/sessions/<name>/session.json
```

Use `herdr session list`, `herdr session attach <name>`, `herdr session stop <name>`, and `herdr session delete <name>` to inspect and manage named session namespaces. Add `--json` to session commands when scripts need machine-readable output.

print the full default config with:

```bash
herdr --default-config
```

if a config value is invalid, or two navigate actions use the same keybinding, herdr falls back to a safe default and shows a startup warning in the UI.

## live reload

After editing `config.toml`, reload the running app without restarting the persistent server:

```bash
herdr server reload-config
```

You can also use the global menu inside herdr and choose `reload config`.

Reload is server-owned. In persistent mode the CLI sends a request to the running server, and the server reads, parses, validates, and applies `config.toml`.

Reloadable now:
- keybindings and prefix
- theme, custom theme colors, and legacy `ui.accent`
- `ui.confirm_close`
- `ui.agent_panel_scope`
- `ui.toast.delivery`
- server-side `ui.sound` policy; attached thin clients refresh local sound config after a successful sound-policy change
- `advanced.scrollback_limit_bytes` for panes created after reload
- `ui.sidebar_width` as the default width; current width updates only while it is still config-owned

Startup-only or special-case:
- `onboarding` does not reopen onboarding during reload
- `advanced.allow_nested` is checked before launch and needs a restart
- existing pane scrollback buffers are not resized during reload
- terminal notifications and sounds are client-local side effects and are sent to the foreground attached client

If the TOML cannot be read or parsed, reload applies nothing and keeps the current running state. If keybindings are invalid, herdr keeps the current keybindings while applying other valid reloadable settings where possible.

## onboarding

```toml
onboarding = true
```

| option | default | description |
|--------|---------|-------------|
| `onboarding` | unset | show first-run notification setup; set `false` after choosing |

notes:
- missing `onboarding` currently behaves like `true`
- set `onboarding = true` to force the setup screen again for testing
- continuing from onboarding writes `onboarding = false` and opens the normal settings UI

## keybindings

keybindings live under `[keys]`.

supported syntax:
- plain keys: `n`, `x`, `-`, `` ` ``
- modifiers: `ctrl+b`, `shift+n`, `alt+x`
- special keys: `enter`, `esc`, `tab`, `backspace`, `left`, `right`, `up`, `down`
- function keys: `f1`, `f12`
- uppercase letters also imply shift: `D` works like `shift+d`

notes:
- most reliable bindings are plain keys, `ctrl+letter`, `esc`/`tab`/`enter`, and function keys
- `alt+...` and punctuation-with-modifiers may vary depending on terminal/tmux setup
- bindings marked `unset` in the key reference are supported actions with no default key assigned
- for navigate-mode actions, duplicate keybindings are treated as config errors; later conflicting bindings fall back to defaults

example:

```toml
[keys]
prefix = "ctrl+b"
new_workspace = "n"
rename_workspace = "shift+n"
close_workspace = "shift+d"
reload_config = ""      # optional, unset by default
open_notification_target = "" # optional, unset by default
new_tab = "c"
split_vertical = "v"
split_horizontal = "-"
close_pane = "x"
rename_pane = ""        # optional, unset by default
fullscreen = "f"
resize_mode = "r"
toggle_sidebar = "b"
previous_workspace = "ctrl+alt+["
next_workspace = "ctrl+alt+]"
previous_tab = "alt+["
next_tab = "alt+]"
focus_pane_left = "alt+h"
focus_pane_down = "alt+j"
focus_pane_up = "alt+k"
focus_pane_right = "alt+l"
```

### key reference

| key | default | action |
|-----|---------|--------|
| `prefix` | `ctrl+b` | enter or leave navigate mode |
| `new_workspace` | `n` | create a new workspace |
| `rename_workspace` | `shift+n` | rename selected workspace |
| `close_workspace` | `shift+d` | close selected workspace |
| `detach` | unset | optional explicit detach shortcut in the persistent session |
| `reload_config` | unset | reload `config.toml` in the running app/server |
| `open_notification_target` | unset | jump to the currently visible notification target |
| `previous_workspace` | unset | switch to the previous workspace directly from terminal mode |
| `next_workspace` | unset | switch to the next workspace directly from terminal mode |
| `new_tab` | `c` | create a new tab |
| `rename_tab` | unset | rename the active tab |
| `previous_tab` | unset | switch to the previous tab directly from terminal mode |
| `next_tab` | unset | switch to the next tab directly from terminal mode |
| `close_tab` | unset | close the active tab |
| `focus_pane_left` | unset | focus the pane to the left directly from terminal mode |
| `focus_pane_down` | unset | focus the pane below directly from terminal mode |
| `focus_pane_up` | unset | focus the pane above directly from terminal mode |
| `focus_pane_right` | unset | focus the pane to the right directly from terminal mode |
| `split_vertical` | `v` | split pane vertically (side by side) |
| `split_horizontal` | `-` | split pane horizontally (stacked) |
| `close_pane` | `x` | close focused pane |
| `rename_pane` | unset | rename the focused pane |
| `fullscreen` | `f` | toggle focused pane fullscreen |
| `resize_mode` | `r` | enter or leave resize mode |
| `toggle_sidebar` | `b` | collapse or expand the sidebar |

### custom command keybindings

Use `[[keys.command]]` to bind a prefix-mode key to a command. Press the prefix key, then the configured key.

```toml
[[keys.command]]
key = "g"
type = "pane"
command = "lazygit"
```

`type` is optional and defaults to `shell`.

| type | behavior |
|------|----------|
| `shell` | run the command detached in the background |
| `pane` | open a temporary zoomed pane, run the command there, then close the pane when the command exits |

Commands run through `/bin/sh -lc`. Herdr sets the command working directory to the active pane cwd when available and provides context through environment variables:

| variable | value |
|----------|-------|
| `HERDR_SOCKET_PATH` | active herdr socket path |
| `HERDR_BIN_PATH` | current herdr binary path |
| `HERDR_ACTIVE_WORKSPACE_ID` | active workspace id |
| `HERDR_ACTIVE_TAB_ID` | active tab id |
| `HERDR_ACTIVE_PANE_ID` | focused pane id |
| `HERDR_ACTIVE_PANE_CWD` | focused pane cwd |

Example detached helper:

```toml
[[keys.command]]
key = "shift+g"
type = "shell"
command = "notify-send herdr 'custom command ran'"
```

## theme

herdr ships with 10 built-in color themes. set one in config:

```toml
[theme]
name = "tokyo-night"
```

### built-in themes

| name | description |
|------|-------------|
| `catppuccin` | soft pastel mocha palette (default) |
| `tokyo-night` | blue-purple aesthetic |
| `dracula` | purple/pink/green classic |
| `nord` | frosty scandinavian blues |
| `gruvbox` | warm retro browns/oranges |
| `one-dark` | atom's beloved palette |
| `solarized` | ethan schoonover's classic |
| `kanagawa` | hokusai-inspired |
| `rose-pine` | muted, elegant |
| `vesper` | high-contrast monochrome with peach and mint accents |

theme names are flexible: `tokyo-night`, `tokyonight`, and `tokyo_night` all work.

### custom overrides

override individual color tokens on top of any base theme:

```toml
[theme]
name = "dracula"

[theme.custom]
panel_bg = "reset"
accent = "#f5c2e7"
red = "rgb(255, 85, 85)"
green = "#a6e3a1"
```

all tokens are optional — only set what you want to change.

### available tokens

| token | used for |
|-------|----------|
| `accent` | highlights, active borders, navigation UI |
| `panel_bg` | floating panel, tab bar, and overlay background |
| `surface0` | selected item background |
| `surface1` | hover/active backgrounds |
| `surface_dim` | active workspace background, separators |
| `overlay0` | muted text, secondary info |
| `overlay1` | slightly brighter secondary text |
| `text` | primary text |
| `subtext0` | workspace names, dimmed labels |
| `mauve` | git branch names, special labels |
| `green` | idle/done states |
| `yellow` | busy/running states |
| `red` | waiting/needs attention states |
| `blue` | unseen notifications |
| `teal` | done notification accents |
| `peach` | interrupted/warning states |

tokens accept the same color formats as `accent`: hex (`#rrggbb`), named colors, or `rgb(r,g,b)`.

for `panel_bg`, you can also use `reset`, `default`, `none`, or `transparent` to stop herdr from painting an opaque panel background and instead use the host terminal's default background.

## ui

```toml
[ui]
sidebar_width = 26
confirm_close = true
show_agent_labels_on_pane_borders = false
agent_panel_scope = "all"
accent = "cyan"
```

### options

| option | default | description |
|--------|---------|-------------|
| `sidebar_width` | `26` | base sidebar width before auto-scaling |
| `confirm_close` | `true` | ask before closing a workspace |
| `show_agent_labels_on_pane_borders` | `false` | show detected/reported agent labels in split pane borders when no manual pane name is set |
| `agent_panel_scope` | `all` | sidebar agent list scope: `current` or `all` |
| `accent` | `cyan` | highlight and border color |

Changing the agent panel scope from the sidebar writes `agent_panel_scope` to config so it survives session resets and upgrades.

`accent` accepts:
- named colors like `cyan`, `blue`, `magenta`
- hex like `#89b4fa`
- rgb like `rgb(137,180,250)`

## toast notifications

```toml
[ui.toast]
delivery = "off"
```

### options

| option | default | description |
|--------|---------|-------------|
| `ui.toast.delivery` | `off` | where background popup notifications should appear |

available values:
- `off` — disable popup notifications
- `herdr` — show top-right in-app toasts
- `terminal` — ask the outer terminal to show a desktop notification

compatibility note:
- older configs may still use `ui.toast.enabled = true|false`
- herdr still reads that legacy key for compatibility
- if you save toast settings from inside herdr, it rewrites the setting to `ui.toast.delivery`

current behavior:
- informational only
- one notification event at a time
- shown for background agent events like `needs attention` and `finished`
- suppression is tab-aware: the active tab stays quiet, but background tabs in the same workspace can still notify
- `terminal` delivery is best-effort and depends on terminal support
- currently targets terminals such as Ghostty, Kitty, iTerm2, and WezTerm
- inside tmux, herdr wraps notification escapes with tmux passthrough

## sound

```toml
[ui.sound]
enabled = true

[ui.sound.agents]
claude = "default"
droid = "off"
```

### options

| option | default | description |
|--------|---------|-------------|
| `ui.sound.enabled` | `true` | enable background agent sounds |

per-agent values:
- `default`
- `on`
- `off`

available agent keys:
- `pi`
- `claude`
- `codex`
- `gemini`
- `cursor`
- `cline`
- `open_code`
- `github_copilot`
- `kimi`
- `droid`
- `amp`

## advanced

```toml
[advanced]
allow_nested = false
scrollback_limit_bytes = 10000000
```

### options

| option | default | description |
|--------|---------|-------------|
| `advanced.allow_nested` | `false` | allow launching herdr from inside a herdr-managed pane |
| `advanced.scrollback_limit_bytes` | `10000000` | maximum scrollback buffer size in bytes retained per pane terminal |

notes:
- by default, herdr blocks nested launches when `HERDR_ENV=1` is already present
- this is mainly an escape hatch for debugging or intentionally weird setups
- this matches Ghostty's default `scrollback-limit` value
- set `scrollback_limit_bytes = 0` to disable pane scrollback entirely
- the old `advanced.scrollback_lines` key is still accepted as a compatibility alias, but it uses the same byte-based value
- in default persistence mode, quitting the ui detaches the current client; use `herdr server stop` to stop the shared background server

## environment variables

| variable | description |
|----------|-------------|
| `HERDR_LOG` | log level filter (default: `herdr=info`) |

## logs

herdr writes local file logs under:

```text
~/.config/herdr/
```

common files:

```text
~/.config/herdr/herdr.log
~/.config/herdr/herdr-client.log
~/.config/herdr/herdr-server.log
```

notes:
- `herdr.log` is used by monolithic `--no-session` mode and some top-level startup paths
- persistent session mode mainly uses `herdr-client.log` and `herdr-server.log`
- logs rotate automatically by size and keep a few older files as `.1`, `.2`, and so on
- default logs are metadata-focused and are intended to be shareable for issue diagnosis
- `HERDR_LOG` can increase verbosity when you need a local repro or deeper debugging
