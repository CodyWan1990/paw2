# Paw2

Paw2 is an early standalone Wayland compositor written in Rust with Smithay.
It is designed to stay lightweight and gaming-focused.

It is complete on its own — workspaces, windows and screen capture all go
through standard protocols, so Waybar, grim and the wlroots portal work with no
setup. [paw2-shell](../paw2-shell/) is an *optional* Quickshell desktop on top
of it, and Paw2 neither needs nor waits for it.

## Current features

- Direct DRM/GBM rendering with libseat and libinput
- Nested development mode inside an existing Wayland or X11 session
- One automatically selected GPU with multiple configured or automatic displays
- Wayland application rendering through XDG shell and linux-dmabuf
- Rootless XWayland support for Steam and X11 games
- Per-workspace scrolling, dwindle, master-stack, and monocle tiling layouts
- Optional clickable tab groups for tiled windows in every layout
- Centered client-sized launcher and dialog windows
- Dedicated fullscreen path with scanout-friendly rendering and presentation timing
- Automatic per-monitor VRR for fullscreen games
- Live per-monitor fractional scaling
- Nine independent workspaces per monitor with Waybar integration
- Per-monitor workspace overview with live window previews
- Named Wayland and XWayland application scratchpads with live previews
- Configurable gaps and focused-window ring
- Client-controlled transparency and background blur protocols
- Wayland wallpaper clients through WLR layer shell
- Configurable focus, swapping, application launch, terminal, and quit controls
- Keyboard, pointer, focus, clipboard, primary selection, and drag-and-drop plumbing
- Standard monitor/window screenshots and DMA-BUF OBS capture through PipeWire
- Clipboard manager support through ext-data-control
- Application secret storage through GNOME Keyring's Secret Service
- A control socket for a shell that wants more than the protocols expose

## Install

Install Rust plus the native Smithay development dependencies for your
distribution, then build Paw2:

On Fedora, install the build dependencies, portal services, screenshot tool,
and GNOME Keyring integration:

```sh
sudo dnf install gnome-keyring gnome-keyring-pam libsecret \
  xorg-x11-server-Xwayland xdg-desktop-portal xdg-desktop-portal-gtk \
  xdg-desktop-portal-wlr grim slurp
```

```sh
cargo build --release --locked
```

Install the binary and display-manager session system-wide by running these
commands as root from the checkout:

```sh
install -Dm755 target/release/paw2 /usr/local/bin/paw2
install -Dm644 packaging/paw2.desktop /usr/share/wayland-sessions/paw2.desktop
install -Dm644 packaging/paw2-portals.conf /usr/share/xdg-desktop-portal/paw2-portals.conf
install -Dm644 packaging/paw2-session.target /usr/lib/systemd/user/paw2-session.target
```

If you installed an earlier screen-capture build, remove its obsolete session
wrapper and user units as root:

```sh
rm -f /usr/local/bin/paw2-session /usr/lib/systemd/user/paw2.service \
  /usr/lib/systemd/user/paw2-shutdown.target
```

Run `systemctl --user daemon-reload` as your regular user, then log out and
select **Paw2** in the display manager. The session entry is kept separate from
any existing Paw session.

For development without changing virtual terminals, run Paw2 nested and
optionally start an application inside it:

```sh
cargo run -- --nested
cargo run -- --nested kitty
```

Direct mode is the default and must be started from a logind/seatd-managed
display-manager session or a free TTY:

```sh
paw2
```

## Configuration

On first run, Paw2 creates `paw2/config.toml` under `$XDG_CONFIG_HOME`, or
`~/.config` when `XDG_CONFIG_HOME` is unset. Invalid settings are reported and
never replaced silently.

Every `*.toml` in `config.d/` beside it applies over `config.toml` in filename
order and is watched the same way. A drop-in takes over each section it names
whole, so deleting `config.d/` returns everything to your file — see
[configuration drop-ins](#configuration-drop-ins).

```toml
[layout]
enabled_layouts = ["scrolling", "dwindle", "master_stack", "monocle"]
column_width = 0.6
preset_column_widths = [0.33333, 0.5, 0.66667]
center_focused_column = false
tab_grouping = false

[appearance]
gaps = 15
focus_ring_width = 1
focus_ring_color = "#0b3d91"
blur_radius = 8

[overview]
zoom = 0.5
backdrop_color = "#1c1c1c"

[input]
focus_follows_mouse = false
middle_click_paste = true

[theme]
cursor_theme = "Oxygen_Zion"
cursor_size = 24
icon_theme = "Papirus-Dark"
gtk_theme = "Breeze"
qt_style = "Breeze"
font = "Monofur Nerd Font Propo 12"

[backend]
direct_scanout = true
# drm_device = "/dev/dri/card1"
# connector = "DP-1"

[[monitors]]
connector = "DP-2"
focus_at_startup = true
resolution = [2560, 1440]
refresh = 120.0
position = [0, 0]
scale = 1.0
transform = "normal"
vrr = "automatic"

[[monitors]]
connector = "DP-1"
resolution = [2560, 1440]
refresh = 199.999
position = [2560, 0]
scale = 1.0
transform = "normal"

[autostart]
commands = [
  ["swaybg", "-i", "/absolute/path/to/wallpaper.png", "-m", "fill"],
  ["waybar"],
]

[scratchpads.terminal]
command = ["kitty", "--class", "paw2-terminal"]
app_id = "paw2-terminal"
edge = "top"
size = [75, 60]
margin = 24

[bindings]
focus_left = ["Super+h", "Super+Left"]
focus_right = ["Super+l", "Super+Right"]
focus_up = ["Super+k", "Super+Up"]
focus_down = ["Super+j", "Super+Down"]
swap_left = ["Super+Shift+h", "Super+Shift+Left"]
swap_right = ["Super+Shift+l", "Super+Shift+Right"]
swap_up = ["Super+Shift+k", "Super+Shift+Up"]
swap_down = ["Super+Shift+j", "Super+Shift+Down"]
consume_or_expel_left = ["Super+bracketleft"]
consume_or_expel_right = ["Super+bracketright"]
tab_group_left = ["Super+Shift+bracketleft"]
tab_group_right = ["Super+Shift+bracketright"]
previous_tab = ["Super+Ctrl+bracketleft"]
next_tab = ["Super+Ctrl+bracketright"]
tab_group_drag = ["Super+Ctrl+MouseLeft"]
cycle_column_width = ["Super+backslash"]
cycle_layout = ["Super+Tab"]
switch_layout_scrolling = []
switch_layout_dwindle = []
switch_layout_master_stack = []
switch_layout_monocle = []
decrease_column_width = ["Super+Ctrl+h", "Super+Ctrl+Left"]
increase_column_width = ["Super+Ctrl+l", "Super+Ctrl+Right"]
decrease_row_height = ["Super+Ctrl+k", "Super+Ctrl+Up"]
increase_row_height = ["Super+Ctrl+j", "Super+Ctrl+Down"]
toggle_floating = ["Super+Shift+space"]
close_window = ["Super+q", "Super+BackSpace"]
toggle_fullscreen = ["Super+Shift+f"]
toggle_overview = ["Super+o"]
toggle_scratchpad_overview = ["Super+Shift+o"]
focus_monitor_left = ["Super+comma"]
focus_monitor_right = ["Super+period"]
focus_monitor_up = []
focus_monitor_down = []
move_to_monitor_left = ["Super+Shift+comma"]
move_to_monitor_right = ["Super+Shift+period"]
move_to_monitor_up = ["Super+Ctrl+Shift+Up"]
move_to_monitor_down = ["Super+Ctrl+Shift+Down"]
quit = ["Super+Shift+Escape"]

[bindings.commands]
"Super+Return" = ["kitty", "--single-instance"]
"Super+d" = ["fuzzel"]
"Super+b" = ["firefox", "--new-window"]

[bindings.scratchpads]
"Super+grave" = "terminal"

[bindings.workspace]
switch = [
  ["Super+1"], ["Super+2"], ["Super+3"],
  ["Super+4"], ["Super+5"], ["Super+6"],
  ["Super+7"], ["Super+8"], ["Super+9"],
]
move = [
  ["Super+Shift+1"], ["Super+Shift+2"], ["Super+Shift+3"],
  ["Super+Shift+4"], ["Super+Shift+5"], ["Super+Shift+6"],
  ["Super+Shift+7"], ["Super+Shift+8"], ["Super+Shift+9"],
]
previous = ["Super+Page_Up"]
next = ["Super+Page_Down"]
move_previous = ["Super+Shift+Page_Up"]
move_next = ["Super+Shift+Page_Down"]
```

Set `layout.tab_grouping = true` to enable session-only groups. The grouping
bindings swallow the adjacent tiled window; repeating one while grouped expels
the active tab in that direction. Tabs can also be selected with a left click
or grouped by dropping one tiled window directly onto another with the
configured `tab_group_drag` chord. Floating, fullscreen, and scratchpad windows
are never grouped.

Each `[scratchpads.NAME]` entry describes one application window. `command` is
an argv array launched on first use, while `app_id` exactly matches either a
native Wayland app ID or an X11 `WM_CLASS`. `edge` accepts `top`, `bottom`,
`left`, or `right`; `size` is a percentage pair. An optional `position = [x,
y]` sets the top-left corner in percentages, otherwise Paw2 centres the window
against its configured edge using `margin` logical pixels.

Scratchpads stay running while hidden. `Super+left-drag` moves them and resizes
from an edge, and Paw2 remembers separate geometry for each monitor in
`$XDG_STATE_HOME/paw2/scratchpads.json` (normally
`~/.local/state/paw2/scratchpads.json`). The scratchpad overview shows live
previews of every scratchpad used during the current session; use the arrow or
Vim keys, Enter, Escape, or the mouse. A shell can reset saved placement by
sending `{"cmd":"reset-scratchpad","name":"terminal"}` to `$PAW2_SOCKET`.

Each workspace remembers its layout. `enabled_layouts` controls which layouts
can be selected and their cycling order; it must contain at least one unique
layout. A direct binding for a disabled layout does nothing. Dwindle alternates
horizontal and vertical splits, master-stack uses `column_width` for its master
area, and monocle fills the usable area with the focused tiled window on top.
`toggle_overview` zooms the desktop out into a vertical strip of workspace
cards, each one a live shrunken copy of the output - its wallpaper, its
windows, its real layout - with the selected card centred and the backdrop
showing around them. Only workspaces holding windows appear, plus the one you
are on. Columns that scroll off the side of a workspace stay visible past the
edge of its card, so a long strip reads at a glance. `Up`/`Down` or `k`/`j`
select workspaces, while `Left`/`Right` or `h`/`l` select columns and
`Tab`/`Shift+Tab` select stacked windows. These controls work with or without
`Super` held.
`Ctrl+Left`/`Right` or `Ctrl+h`/`l` select another monitor. `Enter` activates
the selected workspace and focuses its selected window; `Escape` closes without
switching. Existing configured bindings still work, the mouse wheel moves the
selection without closing, and clicking a window preview focuses that window
and closes the overview.
The default scrolling layout keeps horizontal columns with vertical stacks.
`column_width` is the default fraction of each output used by new columns.
`preset_column_widths` controls the sizes cycled on the focused column; all
widths must be between `0.1` and `1.0`. A lone window is consumed into the
neighboring column with `consume_or_expel_left` or `consume_or_expel_right`;
a window already sharing a column is expelled in that direction. Away from a
resize edge, `Super+left-drag` lifts a tiled window at 75% size in every layout.
Releasing over another tile on the same workspace swaps their positions.
Releasing on a different monitor **moves** the window there — nothing comes back
the other way — landing in the column you dropped on, or in the usual place when
that workspace is empty. Existing floating windows can also be dragged between
monitors.
`toggle_floating` detaches only the focused window from a stack, sizes it to
half the usable monitor width and height, and centers it. On floating windows,
`Super+left-drag` within 8 logical pixels of an edge resizes that edge; corners
resize both axes, while the interior moves the window. Toggling again tiles it
at the pointer position.
The `Super+Ctrl` arrow or Vim bindings resize the focused column or stacked row
by 5%; vertical monitor focus remains available through `focus_monitor_up` and
`focus_monitor_down` if configured. In master-stack they resize the main split
or a focused stack row; in dwindle they resize the nearest matching split around
the focused window. `cycle_column_width` also cycles the master-stack split.
`Super+left-drag` resizes any internal master-stack or dwindle divider. Monocle
intentionally has no resize operation. Manual proportions are remembered per workspace until
Paw2 exits. In scrolling, master-stack, and dwindle, dragging from a tiled
window corner resizes both dividers at once. In master-stack, `Super+[` and
`Super+]` decrease and increase the number of master windows. Directional focus
and swap stay within each side vertically, and cross to the nearest window on
the other side horizontally. Dwindle focus and swap prefer the nearest aligned
window in the pressed direction, falling back to diagonal tiles. Monocle
traverses tiled-window order.
Overview `zoom` is how far the workspaces shrink when it opens, between `0.2`
and `0.75`; `backdrop_color` is what shows behind the cards, where the desktop
no longer reaches. Appearance sizes are logical
pixels; set the ring width or blur radius to `0` to disable that feature. Transparency and blur are
applied automatically to normal window areas that clients do not declare
opaque. The smooth multi-pass effect is clipped to the XDG window geometry;
protocol-aware clients may request a specific blur region. Monitor fields are
optional; unlisted displays use their preferred mode and are placed to the
right. Scale is a positive number where `1.0` is 100% and `1.25` is 125%.
Supported transforms are `normal`, `90`, `180`, `270`,
`flipped`, and their rotated variants. Empty binding lists disable an action.
Binding strings also accept `WheelScrollUp` and `WheelScrollDown`, so optional
mouse-wheel shortcuts can sit beside keyboard shortcuts. For example:

```toml
[bindings]
focus_left = ["Super+h", "Super+Left", "Super+WheelScrollUp"]
focus_right = ["Super+l", "Super+Right", "Super+WheelScrollDown"]
decrease_column_width = ["Ctrl+Alt+WheelScrollDown"]
increase_column_width = ["Ctrl+Alt+WheelScrollUp"]
```

Wheel shortcuts run once per physical detent and consume matching mouse-wheel
events; touchpad and unmatched scrolling continue to applications.
Bindings, commands, layout, appearance, `focus_follows_mouse`, monitor
resolution, refresh, scale, and transform update within about half a second after
saving the file. Invalid saves leave the last working settings active.
Unavailable monitor modes leave the working mode active and report the error
through the control socket. At startup Paw2 keeps as much as it can rather than
exiting: it
logs the reason, then falls back to `config.toml` on its own — ignoring the
generated `config.d/` drop-ins — and only to the built-in defaults if
`config.toml` is itself invalid. A mistake never costs a session, and fixing the
file repairs the running one. Disabling a layout currently in use moves that workspace to
the first enabled layout. Focus follows mouse never scrolls the workspace
viewport or raises floating windows. Backend, monitor position, VRR, startup
focus, autostart, and `middle_click_paste` changes take effect after
restarting Paw2. Cursor changes apply live to the compositor and applications
using Wayland's cursor-shape protocol, while GTK and icon changes are broadcast
through GSettings. Legacy applications that cache them may need to be reopened.
Qt style and font changes take effect after a restart. Palette-generated
`gtk.css` also needs applications to be reopened. Paw2 synchronizes configured
cursor, icon, GTK, and font values through GSettings and exports Qt settings to
launched applications. Shared Qt icon and font settings require the Qt GTK
platform-theme plugin supplied by your distribution. Disabling middle-click
paste hides Wayland and X11 primary selection; it does not consume ordinary
middle-button clicks.
Autostart and binding commands are executable-and-argument arrays; use an
explicit shell entry such as `["sh", "-c", "command"]` only when shell syntax is
needed. Autostart entries run once in order when the compositor starts.

Paw2 exposes the standard `ext-workspace-v1` protocol. Add `"ext/workspaces"`
to a Waybar module list to display and click workspaces. Workspaces are local to
each monitor; previous/next bindings wrap around, and moving a window does not
switch to its destination workspace.

Native Wayland and X11 applications can request fullscreen, or you can toggle
the focused window with `Super+Shift+f`. Fullscreen uses the complete
monitor, hides bars, wallpapers, gaps, rings, and blur, but keeps intentional
overlay-layer surfaces visible. Compatible fullscreen buffers are automatically
scanned out on the DRM primary plane; overlays, a visible software cursor,
scaling, or unsupported buffers fall back to composition. Set
`backend.direct_scanout = false` and restart Paw2 if direct scanout causes
display corruption. Games that retain a smaller buffer are scaled to the
largest centered area without changing their aspect ratio. Paw2 also exposes
presentation timing, relative pointer, pointer constraints, and idle-inhibit
protocols used by native Wayland games. It also supports explicit DRM syncobj
fences, FIFO barriers, commit timing, and content-type hints. Paw2 starts
XWayland automatically and exports `DISPLAY`, so Steam can be launched normally
with `steam`.

VRR is configured per `[[monitors]]` entry. `automatic` enables it while a
fullscreen window controls the output, `always` leaves it enabled, and `never`
disables it. Unconfigured monitors default to `automatic`. Unsupported monitors
remain synchronized normally. Changing this setting requires restarting Paw2.

Screenshots use the standard `ext-image-copy-capture` protocols. `grim` can
capture the whole desktop, an output or region, an individual window with `-T`,
and the cursor with `-c`. These protocols are available to applications running
in the local Paw2 session.

OBS and sandboxed applications use `xdg-desktop-portal-wlr` and PipeWire. Paw2
provides the standard capture protocols while the portal owns PipeWire and the
source chooser. Monitor selection works with `slurp`; choosing individual
windows requires configuring a dmenu-style xdp-wlr chooser.

Application passwords and tokens are stored through GNOME Keyring's standard
Secret Service. A password-based SDDM login unlocks the login keyring through
PAM, and Paw2 finishes initializing its Secret Service component. Do not add a
second `gnome-keyring-daemon` command to Paw2's autostart list. Test the
integration from a Paw2 terminal with a temporary value:

```sh
printf '%s' 'paw2-test-secret' | secret-tool store --label='Paw2 keyring test' application paw2-test
secret-tool lookup application paw2-test
secret-tool clear application paw2-test
```

To test a wallpaper without adding it to autostart, run a layer-shell wallpaper
client from inside Paw2:

```sh
swaybg -i /absolute/path/to/wallpaper.png -m fill
```

The wallpaper makes application transparency and background blur visible.

## Optional shell

[paw2-shell](../paw2-shell/) is a Quickshell desktop — bar, launcher,
notifications, settings — that talks to Paw2 two ways. Neither is required, and
Paw2 behaves identically with nothing connected.

### Configuration drop-ins

Every `*.toml` in `~/.config/paw2/config.d/` applies over `config.toml`, in
filename order, and the directory is watched exactly like `config.toml` is.

**A drop-in takes over each section it names, whole.** Naming one key of
`[appearance]` means the drop-in now supplies that entire section; the keys it
leaves out fall back to Paw2's built-in defaults, and `config.toml`'s
`[appearance]` is ignored until the drop-in stops naming it:

```toml
# ~/.config/paw2/config.d/shell.toml
[appearance]
gaps = 12          # this section is now the drop-in's — corner_radius,
                   # blur and the rest revert to their built-in values
```

Sections no drop-in names are untouched, and two drop-ins naming the same
section resolve by filename: the later one owns it.

Section ownership is what lets a settings GUI keep coupled values consistent —
`focus_ring_width` cannot exceed `gaps`, and a rule like that is unkeepable when
one writer holds each side. It is equally useful without a GUI: keep
machine-specific monitor layout in a drop-in and the rest in `config.toml`, or
drop a file in to try something.

**`rm -r ~/.config/paw2/config.d` reverts every change made through the settings
UI**, returning each section to `config.toml` exactly as you wrote it. That is
the supported way to hand the compositor back to your own file.

If a drop-in is invalid, Paw2 keeps the config it is running and names every
file in the merge, since a type error surfaces on the merged document and
cannot be blamed on one of them.

### Control socket

Paw2 listens on `$PAW2_SOCKET` (in `$XDG_RUNTIME_DIR`, alongside the Wayland
socket, and no more privileged than it). JSON, one message per line.

It sends a `snapshot` — outputs with their workspace occupancy, active layout
and display modes; windows with their title, app id, output and workspace — and
a `config` message carrying the merged configuration and any error from the last
load. Both arrive on connect, and afterwards whenever something changes,
coalesced to at most one per event-loop iteration.

It accepts four commands:

```json
{"cmd": "focus-workspace", "output": "DP-1", "index": 2}
{"cmd": "focus-window", "id": 7}
{"cmd": "set-layout", "layout": "dwindle"}
{"cmd": "quit"}
```

Deliberately no "set config" command: settings go through `config.d/` above, so
there is one path into the configuration and one thing deciding what is valid.

Watch it with `socat -u UNIX-CONNECT:$PAW2_SOCKET -`.

## Roadmap

- Tearing control once Smithay's async page-flip support is stable
- Hardware-safe fullscreen direct scanout
- Wlr-output-management for external display configuration tools
- Session lock and idle notification
- Distribution packages

## Licensing

Paw2 does not have a project license yet. The initial compositor scaffold is
adapted from Smithay's MIT-licensed Smallvil example; its notice is retained in
`LICENSES/Smithay.txt`.
