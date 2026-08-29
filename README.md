# Paw2

> **Built with AI.** Paw2 is written primarily with Claude Code and Codex, with
> human review before anything is merged. Read the code before running it on a
> machine you care about.

Paw2 is a standalone Wayland compositor written in Rust with
[Smithay](https://smithay.github.io/). It is small, tiling, and aimed at
gaming: native Wayland first, with rootless XWayland for older titles.

It is complete on its own. Workspaces, screenshots and screen sharing all go
through standard protocols, so Waybar, grim and the wlroots portal work with no
extra setup. [paw2-shell](../paw2-shell/) is an optional Quickshell desktop on
top; Paw2 neither needs nor waits for it.

Paw2 is early software. Expect rough edges.

---

## Contents

- [Features](#features)
- [Install](#install)
- [Running](#running)
- [Default keys](#default-keys)
- [Configuration](#configuration)
- [Layouts](#layouts)
- [Overview and scratchpads](#overview-and-scratchpads)
- [Gaming](#gaming)
- [Screenshots and screen sharing](#screenshots-and-screen-sharing)
- [Passwords and keyring](#passwords-and-keyring)
- [Wallpaper](#wallpaper)
- [Config drop-ins](#config-drop-ins)
- [Control socket](#control-socket)
- [Roadmap](#roadmap)
- [Licensing](#licensing)

---

## Features

**Display and rendering**
- Direct DRM/GBM rendering with libseat and libinput
- One automatic GPU, multiple displays, per-monitor fractional scaling
- Rounded corners, shadows, blur, transparency and window animations
- Per-monitor VRR for fullscreen games, plus direct scanout

**Windows**
- Scrolling, dwindle, master-stack and monocle layouts, per workspace
- Optional clickable tab groups in every layout
- Nine workspaces per monitor with a live workspace overview
- Named and numbered application scratchpads
- Floating windows, drag-to-move, drag-to-resize, drag between monitors

**Applications**
- XDG shell, linux-dmabuf, layer shell for bars and wallpapers
- Rootless XWayland for Steam and X11 games
- Clipboard, primary selection, drag-and-drop, clipboard managers
- Screenshots and OBS capture through the standard capture protocols
- GNOME Keyring secret storage

**Configuration**
- One TOML file, reloaded live on save
- Invalid settings are reported and never cost you a session
- A control socket for a shell that wants more than the protocols expose

---

## Install

### 1. Dependencies

You need a Rust toolchain plus the Smithay native dependencies. On Fedora, as
root:

```sh
dnf install gnome-keyring gnome-keyring-pam libsecret \
  xorg-x11-server-Xwayland xdg-desktop-portal xdg-desktop-portal-gtk \
  xdg-desktop-portal-wlr grim slurp
```

Other distributions need the equivalent packages: XWayland, the wlroots desktop
portal, GNOME Keyring, and `grim`/`slurp` for screenshots.

### 2. Build

```sh
cargo build --release --locked
```

### 3. Install

Run these as root from the checkout:

```sh
install -Dm755 target/release/paw2 /usr/local/bin/paw2
install -Dm644 packaging/paw2.desktop /usr/share/wayland-sessions/paw2.desktop
install -Dm644 packaging/paw2-portals.conf /usr/share/xdg-desktop-portal/paw2-portals.conf
install -Dm644 packaging/paw2-session.target /usr/lib/systemd/user/paw2-session.target
```

Then, as your normal user:

```sh
systemctl --user daemon-reload
```

Log out and pick **Paw2** in your display manager. The session entry is separate
from any existing Paw session.

If you installed a pre-`paw2-session.target` build, remove its leftovers as
root:

```sh
rm -f /usr/local/bin/paw2-session /usr/lib/systemd/user/paw2.service \
  /usr/lib/systemd/user/paw2-shutdown.target
```

---

## Running

```sh
paw2                        # direct mode (default) — needs a TTY or display manager
paw2 --nested               # nested inside an existing Wayland or X11 session
paw2 --nested kitty         # nested, launching an application inside
paw2 --help
```

Direct mode must start from a logind/seatd-managed display-manager session or a
free TTY. Nested mode is the way to develop and try changes without switching
virtual terminals.

---

## Default keys

`Super` is the modifier throughout. Everything here is remappable — see
[Bindings](#bindings).

### Windows

| Keys | Action |
| --- | --- |
| `Super+h/j/k/l` or arrows | Focus left / down / up / right |
| `Super+Shift+` those | Swap the focused window that way |
| `Super+Ctrl+` those | Resize the column or row by 5% |
| `Super+q`, `Super+BackSpace` | Close window |
| `Super+Shift+space` | Toggle floating |
| `Super+Shift+f` | Toggle fullscreen |
| `Super+backslash` | Cycle preset column widths |
| `Super+[` / `Super+]` | Consume or expel a window sideways; in master-stack, change the master count |

### Layouts, workspaces, monitors

| Keys | Action |
| --- | --- |
| `Super+Tab` | Cycle layout |
| `Super+1`–`9` | Switch workspace |
| `Super+Shift+1`–`9` | Move window to workspace |
| `Super+Page_Up` / `Page_Down` | Previous / next workspace |
| `Super+,` / `Super+.` | Focus monitor left / right |
| `Super+Shift+,` / `Super+Shift+.` | Move window to that monitor |
| `Super+o` | Workspace overview |
| `Super+Shift+o` | Scratchpad overview |

### Tabs and scratchpads

| Keys | Action |
| --- | --- |
| `Super+Shift+[` / `]` | Group the adjacent window into a tab group |
| `Super+Ctrl+[` / `]` | Previous / next tab |
| `Super+Ctrl+LeftDrag` | Drop one window onto another to group them |
| `Super+Alt+1`–`9` | Show or hide numbered scratchpad |
| `Super+Alt+Shift+1`–`9` | Assign or release the focused window to a slot |

### Mouse and session

| Keys | Action |
| --- | --- |
| `Super+LeftDrag` | Move a window, or resize from an edge or divider |
| `Super+Return` | Terminal (`kitty --single-instance` by default) |
| `Super+Shift+Escape` | Quit Paw2 |

---

## Configuration

On first run Paw2 writes `paw2/config.toml` under `$XDG_CONFIG_HOME` (normally
`~/.config/paw2/config.toml`). It is a copy of **[`EXAMPLE.toml`](EXAMPLE.toml)**
— every option Paw2 accepts, annotated, with the hardware-specific sections
commented out. Read that file for the literal values and copy from it; this
section explains what they mean.

Most settings reload **within about half a second of saving**. An invalid save
is reported and the last working configuration keeps running.

| Reloads live | Needs a restart |
| --- | --- |
| Bindings, commands, layout, appearance, animations, overview, `focus_follows_mouse`, monitor resolution / refresh / scale / transform, cursor / GTK / icon theme | Backend, monitor position, VRR, `focus_at_startup`, autostart, `middle_click_paste`, Qt style and font |

Colours are `#RRGGBB` — six hex digits, no alpha and no shorthand. Sizes are
logical pixels unless noted. Unknown keys are rejected rather than ignored, so a
typo names itself.

### `[layout]`

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled_layouts` | all four | Which layouts exist and their cycle order. At least one, no duplicates. A binding for a layout left out does nothing. |
| `column_width` | `0.6` | Fraction of the output width new columns get. `0.1`–`1.0`. |
| `preset_column_widths` | `0.33333, 0.5, 0.66667` | Widths cycled by `cycle_column_width`. Each `0.1`–`1.0`, list not empty. |
| `center_focused_column` | `false` | When `false`, the viewport moves only enough to reveal the focused column. |
| `tab_grouping` | `false` | Enables tab groups. Groups last for the session only. |

### `[appearance]`

| Key | Default | Meaning |
| --- | --- | --- |
| `gaps` | `15` | Space between windows and output edges. At most `256`. |
| `focus_ring_width` | `1` | Ring around the focused window. Cannot exceed `gaps`; `0` disables it. |
| `focus_ring_color` | `#0b3d91` | Ring colour. |
| `corner_radius` | `8` | Rounded window corners. At most `64`; `0` disables. |
| `shadow_radius` | `12` | How far the shadow reaches past the window. At most `64`; `0` disables. |
| `shadow_color` | `#000000` | Shadow colour. |
| `shadow_opacity` | `0.45` | `0.0`–`1.0`. Fully transparent also disables the shadow. |
| `shadow_offset_x` / `_y` | `0` / `4` | Between `-64` and `64`. |
| `blur` | `true` | Backdrop blur behind translucent windows. |
| `blur_radius` | `8` | At most `64`; `0` also disables it. |
| `transparency` | `true` | Compositor-set window transparency. |
| `opacity` | `0.95` | `0.1`–`1.0`. |

Transparency and blur apply to window areas that clients do not declare opaque,
clipped to the XDG window geometry. Protocol-aware clients can request their own
blur region. Compositor opacity never applies to layer surfaces such as bars and
wallpapers, or to the cursor.

### `[animations]`

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `true` | Window movement and resize animations. |
| `duration_ms` | `150` | At most `5000`. |
| `slowdown` | `1.0` | Multiplies the duration. `0.1`–`100.0`, for watching in slow motion. |

### `[overview]`

| Key | Default | Meaning |
| --- | --- | --- |
| `zoom` | `0.5` | How far workspaces shrink when the overview opens. `0.2`–`0.75`. |
| `backdrop_color` | `#1c1c1c` | Shown behind the workspace cards. |

### `[input]`

| Key | Default | Meaning |
| --- | --- | --- |
| `focus_follows_mouse` | `false` | Focus windows as the pointer enters them. Never scrolls the viewport or raises floating windows. |
| `middle_click_paste` | `true` | Exposes the Wayland and X11 primary selection. Disabling it does not swallow ordinary middle clicks. |

### `[theme]`

Optional and commented out by default. Every key may be omitted, but none may be
left blank.

| Key | Meaning |
| --- | --- |
| `cursor_theme` | Cursor theme name. Applies live. |
| `cursor_size` | `1`–`256`. Applies live. |
| `icon_theme` | Icon theme name. Applies live. |
| `gtk_theme` | GTK theme name. Applies live. |
| `qt_style` | Qt style name. Applies at the next start. |
| `font` | Interface font, name and size. Applies at the next start. |

Paw2 publishes these through GSettings and exports Qt settings to the
applications it launches. Applications that cache theme values need reopening.
Shared Qt icon and font settings need your distribution's Qt GTK platform-theme
plugin.

### `[backend]`

Hand-edited only — `paw2-shell` never writes this section, so these stay yours
for rescuing a machine that will not start.

| Key | Default | Meaning |
| --- | --- | --- |
| `direct_scanout` | `true` | Scan out compatible fullscreen buffers on the DRM primary plane. Set `false` and restart if it corrupts your display. |
| `drm_device` | automatic | Absolute path to a GPU, e.g. `/dev/dri/card1`. |
| `connector` | automatic | Drive a single named output. Cannot be combined with `[[monitors]]`. |

### `[[monitors]]`

Monitors are detected automatically and placed left to right at their preferred
mode. Add an entry only to pin something. Connectors must be unique and only one
monitor may set `focus_at_startup`.

| Key | Default | Meaning |
| --- | --- | --- |
| `connector` | required | The DRM connector name, e.g. `DP-1`, `HDMI-A-1`. |
| `focus_at_startup` | `false` | Which monitor has focus when the session begins. |
| `resolution`, `refresh`, `position` | preferred mode, left to right | Pinned mode and layout position. |
| `scale` | `1.0` | Positive number; `1.0` is 100%, `1.25` is 125%. |
| `transform` | `normal` | `normal`, `90`, `180`, `270`, `flipped`, `flipped-90`, `flipped-180`, `flipped-270`. |
| `vrr` | `automatic` | `automatic`, `always`, or `never`. |

An unavailable mode leaves the working mode in place and reports the error on
the control socket.

### `[security]`

`privileged_clients` lists the only clients allowed to bind the privileged
globals: the window list (`ext-foreign-toplevel-list-v1`), per-window capture,
and the clipboard manager interface (`ext-data-control-v1`, which sees every
copy as it happens). Everything else can still capture whole outputs, so `grim`
and `wf-recorder` work without an entry.

It ships allowing `xdg-desktop-portal-wlr`, `xdg-desktop-portal`, `wl-paste` and
`wl-copy`. Names are matched against the basename of the connecting process's
executable, so add whatever binary actually connects — for
`wl-paste --watch cliphist store` that is `wl-paste`, not `cliphist`.

### `[autostart]`

`commands` is a list of argv arrays, run once in order at startup, and empty by
default. The first element is the executable; use `["sh", "-c", "..."]` only
when you actually need shell syntax.

### `[scratchpads.NAME]`

Each entry describes one application window. None are configured by default.

| Key | Default | Meaning |
| --- | --- | --- |
| `command` | required | Argv array, launched on first use. |
| `app_id` | required | Exact match against a Wayland app ID or an X11 `WM_CLASS`. Each may be used once. |
| `edge` | `top` | `top`, `bottom`, `left` or `right`. |
| `size` | `75, 60` | Percentage pair of the monitor, each `1`–`100`. |
| `position` | centred on its edge | Optional top-left corner in percentages. Position plus size must fit within 100. |
| `margin` | `24` | Logical pixels from the edge, at most `256`. |

### Bindings

Every action takes a list of trigger strings; an empty list disables it.
[`EXAMPLE.toml`](EXAMPLE.toml) lists all 39 of them with their defaults, and
[Default keys](#default-keys) above shows what they do.

Modifiers are `Ctrl` (or `Control`), `Alt`, `Shift` and `Super` (or `logo`),
joined with `+`. The trigger is an X keysym name, or one of `WheelScrollUp`,
`WheelScrollDown`, `MouseLeft`, `MouseRight` and `MouseMiddle` — so mouse
shortcuts sit beside keyboard ones:

```toml
[bindings]
focus_left = ["Super+h", "Super+Left", "Super+WheelScrollUp"]
focus_right = ["Super+l", "Super+Right", "Super+WheelScrollDown"]
decrease_column_width = ["Ctrl+Alt+WheelScrollDown"]
increase_column_width = ["Ctrl+Alt+WheelScrollUp"]
```

Wheel shortcuts fire once per physical detent and consume matching wheel events.
Touchpad and unmatched scrolling still reach applications.

No trigger may be assigned to two actions. Only `tab_group_drag` takes a mouse
button, and it requires one.

Three tables extend the plain actions: `[bindings.commands]` maps a trigger to
an argv array, `[bindings.scratchpads]` maps one to a scratchpad name, and
`[bindings.workspace]` plus `[bindings.numbered_scratchpads]` take lists of
exactly nine, one per slot.

### If the config is broken at startup

Paw2 keeps as much as it can rather than refusing to start. It logs the reason,
falls back to `config.toml` alone (ignoring `config.d/`), and only then to the
built-in defaults. Fixing the file repairs the running session.

---

## Layouts

Each workspace remembers its own layout.

| Layout | Behaviour |
| --- | --- |
| **scrolling** | Horizontal columns with vertical stacks, scrolling past the screen edge. The default. |
| **dwindle** | Alternating horizontal and vertical splits. |
| **master_stack** | A master area sized by `column_width`, plus a stack. `Super+[` / `]` change the master count. |
| **monocle** | The focused tiled window fills the usable area. No resizing. |

**Resizing.** `Super+Ctrl` with arrows or Vim keys resizes by 5%: the column or
stack row in scrolling, the main split or a stack row in master-stack, the
nearest matching split in dwindle. `Super+left-drag` on any internal divider
resizes it, and dragging from a tile corner resizes both dividers at once.
`cycle_column_width` also cycles the master-stack split. Manual proportions are
remembered per workspace until Paw2 exits.

**Moving.** Away from a resize edge, `Super+left-drag` lifts a tiled window at
75% size. Drop it on another tile to swap them. Drop it on a different monitor
to move it there — nothing comes back the other way — landing in the column you
dropped on, or in the usual place if that workspace is empty. Floating windows
drag between monitors the same way.

**Floating.** `toggle_floating` detaches the focused window, sizes it to half
the usable monitor, and centres it. On a floating window, `Super+left-drag`
within 8 logical pixels of an edge resizes that edge; corners resize both axes;
the interior moves it. Toggling again tiles it at the pointer.

**Focus order.** Master-stack keeps vertical movement within each side and
crosses horizontally to the nearest window. Dwindle prefers the nearest aligned
window in the pressed direction and falls back to diagonal tiles. Monocle walks
tiled-window order.

**Tab groups.** With `layout.tab_grouping = true`, the grouping bindings swallow
the adjacent tiled window; repeating one while grouped expels the active tab
that way. Tabs can also be clicked, or created by dropping one tiled window onto
another with `tab_group_drag`. Floating, fullscreen and scratchpad windows are
never grouped.

**Workspaces.** Nine per monitor, exposed through the standard
`ext-workspace-v1` protocol — add `"ext/workspaces"` to a Waybar module list to
display and click them. Workspaces are local to each monitor, previous/next wrap
around, and moving a window there does not follow it.

---

## Overview and scratchpads

### Workspace overview

`Super+o` zooms the desktop out into a vertical strip of workspace cards, each a
live shrunken copy of the output — wallpaper, windows, real layout — with the
selected card centred. Only workspaces holding windows appear, plus the one you
are on. Columns scrolled off the side of a workspace stay visible past its card
edge, so a long strip reads at a glance.

| Keys | Action |
| --- | --- |
| `Up`/`Down` or `k`/`j` | Select workspace |
| `Left`/`Right` or `h`/`l` | Select column |
| `Tab` / `Shift+Tab` | Select stacked window |
| `Ctrl+Left`/`Right` or `Ctrl+h`/`l` | Select another monitor |
| `Enter` | Activate the workspace and focus the selection |
| `Escape` | Close without switching |

These work with or without `Super` held. Existing bindings still work, the wheel
moves the selection without closing, and clicking a preview focuses that window
and closes the overview.

### Scratchpads

Scratchpads stay running while hidden. `Super+left-drag` moves them and resizes
from an edge, and Paw2 remembers separate geometry per monitor in
`$XDG_STATE_HOME/paw2/scratchpads.json` (normally
`~/.local/state/paw2/scratchpads.json`).

Numbered slots hold any focused window: `Super+Alt+Shift+1`–`9` assign or
release a slot, `Super+Alt+1`–`9` show or hide it.

`Super+Shift+o` opens the scratchpad overview — live previews of every
configured and numbered scratchpad used this session, in a single strip that
scrolls with the selection. Arrow or Vim keys, the scroll wheel, `Enter`,
`Escape`, or the mouse. `[overview] scratchpad_position` puts the strip
`centered`, `top`, `bottom` (scrolling sideways), `left` or `right` (scrolling
up and down).

The strip keeps clear of bars and docks that claim an exclusive zone, stands off
that edge by `appearance.gaps`, and wears the same border as the focused window.
It shows as many whole previews as fit and scrolls the rest into view.

To reset saved placement, send `{"cmd":"reset-scratchpad","name":"terminal"}` to
the [control socket](#control-socket).

---

## Gaming

**Fullscreen.** Applications can request it, or toggle the focused window with
`Super+Shift+f`. Fullscreen takes the whole monitor and hides bars, wallpapers,
gaps, rings and blur, while keeping intentional overlay-layer surfaces visible.
Games that keep a smaller buffer are scaled to the largest centred area without
changing aspect ratio.

**Direct scanout.** Compatible fullscreen buffers go straight onto the DRM
primary plane. Overlays, a visible software cursor, scaling or unsupported
buffers fall back to composition. Disable with `backend.direct_scanout = false`
if it corrupts your display.

**VRR.** Set per `[[monitors]]` entry. `automatic` (the default) enables it
while a fullscreen window owns the output, `always` leaves it on, `never`
disables it. Monitors without VRR support stay synchronised normally. Changes
need a restart.

**Native Wayland games** get presentation timing, relative pointer, pointer
constraints, idle inhibit, explicit DRM syncobj fences, FIFO barriers, commit
timing and content-type hints.

**XWayland** starts automatically and `DISPLAY` is exported, so Steam launches
normally with `steam`. XWayland is rootless; X11 games are managed like any
other window.

---

## Screenshots and screen sharing

Screenshots use the standard `ext-image-copy-capture` protocols, so `grim` works
directly inside a Paw2 session:

```sh
grim screenshot.png                # whole desktop
grim -o DP-1 screenshot.png        # one output
grim -g "$(slurp)" screenshot.png  # a region
grim -T screenshot.png             # a single window
grim -c screenshot.png             # include the cursor
```

OBS and sandboxed applications go through `xdg-desktop-portal-wlr` and PipeWire.
Paw2 provides the capture protocols; the portal owns PipeWire and the source
chooser. Monitor selection works with `slurp`. Picking individual windows needs a
dmenu-style xdp-wlr chooser configured.

Clipboard managers work through `ext-data-control`, listed in
[`[security]`](#security).

---

## Passwords and keyring

Application passwords and tokens are stored through GNOME Keyring's Secret
Service. A password-based SDDM login unlocks the login keyring through PAM and
Paw2 finishes initialising the Secret Service component.

Do not add a second `gnome-keyring-daemon` to autostart.

Test it from a terminal inside Paw2:

```sh
printf '%s' 'paw2-test-secret' | secret-tool store --label='Paw2 keyring test' application paw2-test
secret-tool lookup application paw2-test
secret-tool clear application paw2-test
```

---

## Wallpaper

Any layer-shell wallpaper client works. To try one before adding it to
autostart, run it from inside Paw2:

```sh
swaybg -i /absolute/path/to/wallpaper.png -m fill
```

A wallpaper is what makes transparency and blur visible.

---

## Config drop-ins

Every `*.toml` in `~/.config/paw2/config.d/` applies over `config.toml` in
filename order, and is watched exactly like `config.toml` is.

**A drop-in takes over each section it names, whole.** Naming one key of
`[appearance]` means the drop-in now supplies that entire section: the keys it
leaves out revert to Paw2's built-in defaults, and `config.toml`'s
`[appearance]` is ignored until the drop-in stops naming it.

```toml
# ~/.config/paw2/config.d/shell.toml
[appearance]
gaps = 12          # this section is now the drop-in's — corner_radius,
                   # blur and the rest revert to built-in values
```

Sections no drop-in names are untouched. Two drop-ins naming the same section
resolve by filename; the later one wins.

Whole-section ownership is what lets a settings GUI keep coupled values
consistent — `focus_ring_width` cannot exceed `gaps`, and that rule is
unkeepable when two writers each hold one side. It is just as useful by hand:
keep machine-specific monitor layout in a drop-in, or drop a file in to try
something.

```sh
rm -r ~/.config/paw2/config.d    # revert every change made through a settings UI
```

That returns each section to `config.toml` as you wrote it, and is the supported
way to take the compositor back.

If a drop-in is invalid, Paw2 keeps the config it is running and names every
file in the merge — a type error surfaces on the merged document and cannot be
blamed on one file.

---

## Control socket

Paw2 listens on `$PAW2_SOCKET`, in `$XDG_RUNTIME_DIR` beside the Wayland socket
and no more privileged than it. JSON, one message per line.

It sends two messages on connect, and again whenever something changes
(coalesced to at most one per event-loop iteration):

- `snapshot` — outputs with workspace occupancy, active layout and display
  modes; windows with title, app ID, output and workspace.
- `config` — the merged configuration and any error from the last load.

It accepts five commands:

```json
{"cmd": "focus-workspace", "output": "DP-1", "index": 2}
{"cmd": "focus-window", "id": 7}
{"cmd": "set-layout", "layout": "dwindle"}
{"cmd": "reset-scratchpad", "name": "terminal"}
{"cmd": "quit"}
```

There is deliberately no "set config" command. Settings go through `config.d/`,
so there is one path into the configuration and one thing deciding what is
valid.

Watch it with:

```sh
socat -u UNIX-CONNECT:$PAW2_SOCKET -
```

---

## Optional shell

[paw2-shell](../paw2-shell/) is a Quickshell desktop — bar, launcher,
notifications, settings — that talks to Paw2 through the control socket and
writes settings as `config.d/` drop-ins. Neither is required; Paw2 behaves
identically with nothing connected.

---

## Roadmap

- Tearing control, once Smithay's async page-flip support is stable
- Hardware-safe fullscreen direct scanout
- `wlr-output-management` for external display configuration tools
- Session lock and idle notification
- Distribution packages

---

## Licensing

Paw2 does not have a project license yet. The initial compositor scaffold is
adapted from Smithay's MIT-licensed Smallvil example; its notice is kept in
`LICENSES/Smithay.txt`. The bundled Noto Sans font is covered by
`LICENSES/NotoSans.txt`.
