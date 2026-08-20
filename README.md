# Paw2

Paw2 is an early standalone Wayland compositor written in Rust with Smithay.
It is designed to stay lightweight and gaming-focused, with Quickshell planned
as the shell layer.

## Current features

- Direct DRM/GBM rendering with libseat and libinput
- Nested development mode inside an existing Wayland or X11 session
- One automatically selected GPU with multiple configured or automatic displays
- Wayland application rendering through XDG shell and linux-dmabuf
- Rootless XWayland support for Steam and X11 games
- Horizontal scrolling columns with vertically stacked windows
- Centered client-sized launcher and dialog windows
- Dedicated fullscreen path with scanout-friendly rendering and presentation timing
- Nine independent workspaces per monitor with Waybar integration
- Configurable gaps and focused-window ring
- Client-controlled transparency and background blur protocols
- Wayland wallpaper clients through WLR layer shell
- Configurable focus, swapping, application launch, terminal, and quit controls
- Keyboard, pointer, focus, clipboard, primary selection, and drag-and-drop plumbing
- XDG desktop portals with DMA-BUF monitor sharing through xdg-desktop-portal-wlr
- Application secret storage through GNOME Keyring's Secret Service

## Install

Install Rust plus the native Smithay development dependencies for your
distribution, then build Paw2:

On Fedora, install the portal services, screen-sharing selector, and GNOME
Keyring integration:

```sh
sudo dnf install gnome-keyring gnome-keyring-pam libsecret xorg-x11-server-Xwayland \
  xdg-desktop-portal xdg-desktop-portal-gtk xdg-desktop-portal-wlr slurp
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
```

Log out and select **Paw2** in the display manager. The session entry is kept
separate from any existing Paw session.

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

```toml
[layout]
column_width = 0.6
preset_column_widths = [0.33333, 0.5, 0.66667]
center_focused_column = false

[appearance]
gaps = 15
focus_ring_width = 1
focus_ring_color = "#0b3d91"
blur_radius = 8

[input]
middle_click_paste = true

[backend]
direct_scanout = true
# drm_device = "/dev/dri/card1"
# connector = "DP-1"

[[monitors]]
connector = "DP-2"
resolution = [2560, 1440]
refresh = 120.0
position = [0, 0]
transform = "normal"

[[monitors]]
connector = "DP-1"
resolution = [2560, 1440]
refresh = 199.999
position = [2560, 0]
transform = "normal"

[autostart]
commands = [
  ["swaybg", "-i", "/absolute/path/to/wallpaper.png", "-m", "fill"],
  ["waybar"],
]

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
cycle_column_width = ["Super+backslash"]
close_window = ["Super+q", "Super+BackSpace"]
toggle_fullscreen = ["Super+f"]
focus_monitor_left = ["Super+Ctrl+Left"]
focus_monitor_right = ["Super+Ctrl+Right"]
focus_monitor_up = ["Super+Ctrl+Up"]
focus_monitor_down = ["Super+Ctrl+Down"]
move_to_monitor_left = ["Super+Ctrl+Shift+Left"]
move_to_monitor_right = ["Super+Ctrl+Shift+Right"]
move_to_monitor_up = ["Super+Ctrl+Shift+Up"]
move_to_monitor_down = ["Super+Ctrl+Shift+Down"]
quit = ["Super+Shift+Escape"]

[bindings.commands]
"Super+Return" = ["kitty", "--single-instance"]
"Super+d" = ["fuzzel"]
"Super+b" = ["firefox", "--new-window"]

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

`column_width` is the default fraction of each output used by new columns.
`preset_column_widths` controls the sizes cycled on the focused column; all
widths must be between `0.1` and `1.0`. A lone window is consumed into the
neighboring column with `consume_or_expel_left` or `consume_or_expel_right`;
a window already sharing a column is expelled in that direction. Stacked
windows divide the available height equally. Appearance sizes are logical
pixels; set the ring width or blur radius to `0` to disable that feature. Transparency and blur are
applied automatically to normal window areas that clients do not declare
opaque. The smooth multi-pass effect is clipped to the XDG window geometry;
protocol-aware clients may request a specific blur region. Monitor fields are
optional; unlisted displays use their preferred mode and are placed to the
right. Supported transforms are `normal`, `90`, `180`, `270`,
`flipped`, and their rotated variants. Empty binding lists disable an action.
Bindings, commands, layout, and appearance settings update within about half a
second after saving the file. Invalid saves leave the last working settings
active. Backend, monitor, autostart, and `middle_click_paste` changes take effect
after restarting Paw2. Disabling middle-click paste hides Wayland and X11 primary
selection; it does not consume ordinary middle-button clicks. Autostart and
binding commands are executable-and-argument arrays; use an explicit shell
entry such as `["sh", "-c", "command"]` only when shell syntax is needed.
Autostart entries run once in order when the compositor starts.

Paw2 exposes the standard `ext-workspace-v1` protocol. Add `"ext/workspaces"`
to a Waybar module list to display and click workspaces. Workspaces are local to
each monitor; previous/next bindings wrap around, and moving a window does not
switch to its destination workspace.

Native Wayland and X11 applications can request fullscreen, or you can toggle
the focused window with `Super+f`. Fullscreen uses the complete
monitor, hides bars, wallpapers, gaps, rings, and blur, but keeps intentional
overlay-layer surfaces visible. Compatible fullscreen buffers are automatically
scanned out on the DRM primary plane; overlays, a visible software cursor,
scaling, or unsupported buffers fall back to composition. Set
`backend.direct_scanout = false` and restart Paw2 if direct scanout causes
display corruption. Games that retain a smaller buffer are scaled to the
largest centered area without changing their aspect ratio. Paw2 also exposes
presentation timing, relative pointer, pointer constraints, and idle-inhibit
protocols used by native Wayland games. Paw2 starts XWayland automatically and
exports `DISPLAY`, so Steam can be launched normally with `steam`.

Screen sharing is available in the standalone session through the standard
desktop portal. Paw2 restricts its screencopy protocol to the installed
`xdg-desktop-portal-wlr` executable; applications should use the portal rather
than connecting to screencopy directly. The initial implementation captures
monitors or monitor regions using DMA-BUF and does not provide individual-window
capture.

Application passwords and tokens are stored through GNOME Keyring's standard
Secret Service. A password-based SDDM login unlocks the login keyring through
PAM; do not add `gnome-keyring-daemon` to Paw2's autostart commands. Test the
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

## Roadmap

- VRR and tearing control
- Hardware-safe fullscreen direct scanout
- Quickshell integration
- Distribution packages

## Licensing

Paw2 does not have a project license yet. The initial compositor scaffold is
adapted from Smithay's MIT-licensed Smallvil example; its notice is retained in
`LICENSES/Smithay.txt`.
