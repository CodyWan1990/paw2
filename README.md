# Paw2

> **Built with AI.** Paw2 is written primarily with Claude Code and Codex, with
> human review before anything is merged. Read the code before running it on a
> machine you care about.

Paw2 is a small Wayland compositor built for gaming. Native Wayland games are
the first priority, while rootless XWayland keeps older games and Steam titles
useful too.

This is beta software. It is already usable, but a compositor sits underneath
your whole desktop session: save your work before trying it and expect rough
edges.

## What it can do

- Scrolling, dwindle, master-stack, and monocle tiling layouts
- Nine workspaces per monitor, floating windows, tabs, and scratchpads
- Multiple displays with fractional scaling and per-monitor VRR
- Direct scanout for compatible fullscreen games
- Rootless XWayland for Steam and legacy games
- Rounded corners, shadows, blur, transparency, and animations
- Screenshots and screen sharing through standard Wayland protocols
- Live-reloading TOML configuration
- An optional [Quickshell desktop](../paw2-shell/) for the bar, launcher,
  notifications, and settings

Paw2 does not require its optional shell. Waybar, layer-shell wallpaper tools,
`grim`, and `xdg-desktop-portal-wlr` can be used directly.

## Before you install

Paw2 currently expects a Linux system with DRM/KMS, logind or seatd, and a
working graphics stack. Fedora is the tested path; packages for other
distributions are still on the roadmap.

On Fedora, install the build and runtime dependencies:

```sh
sudo dnf install cargo gcc pkgconf-pkg-config \
  libdisplay-info-devel libglvnd-devel libinput-devel libseat-devel \
  libxkbcommon-devel mesa-libgbm-devel systemd-devel wayland-devel \
  gnome-keyring gnome-keyring-pam libsecret \
  xorg-x11-server-Xwayland xdg-desktop-portal xdg-desktop-portal-gtk \
  xdg-desktop-portal-wlr grim slurp
```

If you use another distribution, install the equivalent development libraries
plus XWayland, `xdg-desktop-portal-wlr`, GNOME Keyring, `grim`, and `slurp`.

## Build and install

Build a release binary from the repository root:

```sh
cargo build --release --locked
```

Then install Paw2 and its session files:

```sh
sudo install -Dm755 target/release/paw2 /usr/local/bin/paw2
sudo install -Dm644 packaging/paw2.desktop /usr/share/wayland-sessions/paw2.desktop
sudo install -Dm644 packaging/paw2-portals.conf /usr/share/xdg-desktop-portal/paw2-portals.conf
sudo install -Dm644 packaging/paw2-session.target /usr/lib/systemd/user/paw2-session.target
sudo install -Dm644 EXAMPLE.toml /usr/share/doc/paw2/EXAMPLE.toml
sudo install -Dm644 LICENSE /usr/share/licenses/paw2/LICENSE
sudo install -Dm644 LICENSES/Smithay.txt /usr/share/licenses/paw2/Smithay.txt
sudo install -Dm644 LICENSES/NotoSans.txt /usr/share/licenses/paw2/NotoSans.txt
sudo install -Dm644 THIRD_PARTY_LICENSES.html /usr/share/licenses/paw2/THIRD_PARTY_LICENSES.html
systemctl --user daemon-reload
```

Log out, choose **Paw2** in your display manager, and sign back in.

To try Paw2 safely inside your current desktop first:

```sh
paw2 --nested
```

You can also launch one application with the nested compositor:

```sh
paw2 --nested kitty
```

## First steps

On first launch, Paw2 creates `~/.config/paw2/config.toml` with a small set of
fallback shortcuts. The fallback deliberately leaves visual effects and most
actions disabled.

Copy settings and bindings from [`EXAMPLE.toml`](EXAMPLE.toml) as needed. It
documents every available option and its default. Most changes reload within
about half a second; invalid changes are reported while the last working
configuration stays active.

The built-in fallback shortcuts are:

| Shortcut | Action |
| --- | --- |
| `Super+h/j/k/l` or arrow keys | Move focus |
| `Super+Shift+h/j/k/l` or arrow keys | Swap the focused window |
| `Super+1` through `Super+9` | Change workspace |
| `Super+Shift+1` through `Super+Shift+9` | Move a window to a workspace |
| `Super+LeftDrag` | Move or resize a window |
| `Super+Return` | Open Kitty |
| `Super+q` or `Super+Backspace` | Close the focused window |
| `Super+Shift+Escape` | End the Paw2 session |

## Gaming notes

Native Wayland clients receive presentation timing, relative-pointer and
pointer-constraint support, idle inhibition, explicit sync fences, FIFO
barriers, commit timing, and content-type hints.

XWayland starts automatically and exports `DISPLAY`, so Steam can be launched
normally. X11 games are rootless and managed like other windows.

VRR defaults to automatic mode: it turns on while a fullscreen window owns an
output. Direct scanout is also enabled by default. If direct scanout causes
display corruption on your hardware, add this to your config and restart:

```toml
[backend]
direct_scanout = false
```

## Screenshots and screen sharing

Paw2 supports the standard capture protocols used by `grim`:

```sh
grim screenshot.png
grim -o DP-1 screenshot.png
grim -g "$(slurp)" screenshot.png
```

OBS and sandboxed applications use `xdg-desktop-portal-wlr` and PipeWire.
Monitor selection works through `slurp`; window selection needs a dmenu-style
xdp-wlr chooser.

## Security

Paw2 restricts interfaces that can reveal window titles, clipboard contents,
or individual window images. The default allowlist includes the desktop portal
needed for screen sharing but no clipboard manager. See the `[security]`
section in [`EXAMPLE.toml`](EXAMPLE.toml) before granting another program that
access.

This is a useful boundary, not an application sandbox. Only run software you
trust inside your desktop session.

## Roadmap

- Tearing control when Smithay's asynchronous page-flip support is stable
- Hardware-safe fullscreen direct scanout
- External display tools through `wlr-output-management`
- Session locking and idle notifications
- Distribution packages

## License

Paw2 is available under the [MIT License](LICENSE). The license includes the
standard “as is” disclaimer of warranty and limitation of liability.

Paw2 began from Smithay's MIT-licensed Smallvil example and bundles Noto Sans
under the SIL Open Font License 1.1. Their required notices are kept in
[`LICENSES/`](LICENSES/). Rust dependencies are recorded in `Cargo.lock`,
checked by `cargo deny check licenses`, and listed with their license text in
[`THIRD_PARTY_LICENSES.html`](THIRD_PARTY_LICENSES.html).
