use std::{
    collections::BTreeMap,
    env,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;
use smithay::{
    input::keyboard::{Keysym, ModifiersState, keysyms, xkb},
    utils::Transform,
};

/// Drop-in directory beside `config.toml`. Every `*.toml` in it applies over the
/// user's config in filename order, which is how a settings GUI changes values
/// without ever writing - or reformatting - a hand-edited file.
///
/// A drop-in takes over each section it names *whole* (see [`replace_sections`]),
/// so deleting this directory returns every section to `config.toml` exactly as
/// its author left it.
const OVERLAY_DIR: &str = "config.d";

const DEFAULT_CONFIG: &str = r##"# Paw2 configuration

[layout]
# Layouts available to cycling, in cycle order.
enabled_layouts = ["scrolling", "dwindle", "master_stack", "monocle"]
# Default fraction of the output width used by each column.
column_width = 0.6
# Widths cycled on the focused column.
preset_column_widths = [0.33333, 0.5, 0.66667]
# When false, the viewport moves only enough to reveal the focused column.
center_focused_column = false
# Group tiled windows behind a clickable tab bar.
tab_grouping = false

[appearance]
# Logical pixels between windows and output edges.
gaps = 15
focus_ring_width = 1
focus_ring_color = "#0b3d91"
# Rounded window corners in logical pixels; zero disables them.
corner_radius = 8
# Drop shadow behind windows. The radius is how far it reaches past the window
# in logical pixels; zero, or a fully transparent shadow, disables it.
shadow_radius = 12
shadow_color = "#000000"
shadow_opacity = 0.45
shadow_offset_x = 0
shadow_offset_y = 4
# Smooth backdrop blur behind translucent windows.
blur = true
blur_radius = 8
# Compositor-set window opacity. Applies to windows only, never to layer
# surfaces such as bars and wallpapers, and never to the cursor.
transparency = true
opacity = 0.95

[animations]
enabled = true
duration_ms = 150
# Multiplier for watching animations in slow motion; 3.0 runs them 3x slower.
slowdown = 1.0

[overview]
# How far the workspaces shrink when the overview opens. 0.5 is half size.
zoom = 0.5
# Shown behind the workspace cards, where the desktop no longer reaches.
backdrop_color = "#1c1c1c"

[input]
# Focus windows as the pointer enters them without moving the workspace viewport.
focus_follows_mouse = false
# Expose Wayland primary selection for middle-click paste.
middle_click_paste = true

# Desktop theme values are optional. Cursor, GTK and icon changes apply live;
# Qt style and font changes apply at startup.
#[theme]
#cursor_theme = "Oxygen_Zion"
#cursor_size = 24
#icon_theme = "Papirus-Dark"
#gtk_theme = "Breeze"
#qt_style = "Breeze"
#font = "Monofur Nerd Font Propo 12"

# Hand-edited only. paw2-shell never writes this section, so drm_device and
# connector stay yours for fixing a machine that will not start.
[backend]
# Automatically scan out compatible fullscreen buffers on the DRM primary plane.
direct_scanout = true
# drm_device = "/dev/dri/card1"
# connector = "DP-1"

# Monitors are auto-detected and placed left to right. Add a [[monitors]] entry
# only to pin a connector's mode, position, scale, rotation, or VRR policy; see the
# README for the full example.
#[[monitors]]
#connector = "DP-1"
#focus_at_startup = true
#resolution = [2560, 1440]
#refresh = 120.0
#position = [0, 0]
#scale = 1.0
#transform = "normal"
#vrr = "automatic"

[security]
# Clients allowed to bind the privileged globals: the window list
# (ext-foreign-toplevel-list-v1), per-window capture, and the clipboard manager
# interface (ext-data-control-v1, which sees every copy as it happens).
# Everything else can still capture whole outputs, so grim and wf-recorder work
# without an entry. Matched against the basename of the connecting process's
# executable, so a clipboard history tool needs whatever binary actually
# connects - for `wl-paste --watch cliphist store` that is wl-paste, not cliphist.
privileged_clients = [
  "xdg-desktop-portal-wlr",
  "xdg-desktop-portal",
  "wl-paste",
  "wl-copy",
]

[autostart]
# Commands are argv arrays and run once when Paw2 starts.
commands = [
  # ["swaybg", "-i", "/absolute/path/to/wallpaper.png", "-m", "fill"],
  # ["waybar"],
]

# Named application scratchpads launch on first use. `app_id` matches a
# Wayland app_id or X11 WM_CLASS exactly.
#[scratchpads.terminal]
#command = ["kitty", "--class", "paw2-terminal"]
#app_id = "paw2-terminal"
#edge = "top"
#size = [75, 60]
#position = [12.5, 5]
#margin = 24

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
# "Super+d" = ["fuzzel"]
# "Super+b" = ["firefox", "--new-window"]

[bindings.scratchpads]
# "Super+grave" = "terminal"

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
"##;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    SwapLeft,
    SwapRight,
    SwapUp,
    SwapDown,
    ConsumeOrExpelLeft,
    ConsumeOrExpelRight,
    TabGroupLeft,
    TabGroupRight,
    PreviousTab,
    NextTab,
    TabGroupDrag,
    CycleColumnWidth,
    CycleLayout,
    SwitchLayout(LayoutMode),
    DecreaseColumnWidth,
    IncreaseColumnWidth,
    DecreaseRowHeight,
    IncreaseRowHeight,
    ToggleFloating,
    CloseWindow,
    ToggleFullscreen,
    ToggleOverview,
    ToggleScratchpad(usize),
    ToggleScratchpadOverview,
    FocusMonitorLeft,
    FocusMonitorRight,
    FocusMonitorUp,
    FocusMonitorDown,
    MoveToMonitorLeft,
    MoveToMonitorRight,
    MoveToMonitorUp,
    MoveToMonitorDown,
    SwitchWorkspace(usize),
    MoveToWorkspace(usize),
    PreviousWorkspace,
    NextWorkspace,
    MoveToPreviousWorkspace,
    MoveToNextWorkspace,
    LaunchCommand(usize),
    Quit,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LayoutMode {
    #[default]
    Scrolling,
    Dwindle,
    MasterStack,
    Monocle,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ScratchpadEdge {
    #[default]
    Top,
    Bottom,
    Left,
    Right,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScratchpadConfig {
    pub name: String,
    pub command: Vec<String>,
    pub app_id: String,
    pub edge: ScratchpadEdge,
    pub size: [f64; 2],
    pub position: Option<[f64; 2]>,
    pub margin: i32,
}

impl LayoutMode {
    /// The name this layout is written as in the config, which is also the name
    /// the control socket reports and accepts.
    pub fn name(self) -> &'static str {
        match self {
            Self::Scrolling => "scrolling",
            Self::Dwindle => "dwindle",
            Self::MasterStack => "master_stack",
            Self::Monocle => "monocle",
        }
    }
}

/// Every field defaults, so a `config.d/` drop-in that owns `[layout]` may name
/// only the keys it has a control for. The values match `DEFAULT_CONFIG`, which
/// `parses_the_default_config` holds them to.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct LayoutConfig {
    pub enabled_layouts: Vec<LayoutMode>,
    pub column_width: f64,
    pub preset_column_widths: Vec<f64>,
    pub center_focused_column: bool,
    pub tab_grouping: bool,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            enabled_layouts: default_enabled_layouts(),
            column_width: 0.6,
            preset_column_widths: default_preset_column_widths(),
            center_focused_column: false,
            tab_grouping: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AppearanceConfig {
    pub gaps: i32,
    pub focus_ring_width: i32,
    pub focus_ring_color: [f32; 4],
    pub corner_radius: i32,
    pub shadow_radius: i32,
    /// Premultiplied, so the shader can blend it without a second multiply.
    pub shadow_color: [f32; 4],
    pub shadow_offset_x: i32,
    pub shadow_offset_y: i32,
    pub blur: bool,
    pub blur_radius: i32,
    pub transparency: bool,
    pub opacity: f32,
}

impl AppearanceConfig {
    /// Blur needs both the switch and a non-zero radius to be worth compiling.
    pub fn blur_enabled(&self) -> bool {
        self.blur && self.blur_radius > 0
    }

    /// A shadow needs somewhere to fall and something to fall in.
    pub fn shadow_enabled(&self) -> bool {
        self.shadow_radius > 0 && self.shadow_color[3] > 0.0
    }

    /// Window opacity actually in effect; exactly 1.0 while transparency is off.
    pub fn window_opacity(&self) -> f32 {
        if self.transparency { self.opacity } else { 1.0 }
    }
}

/// Position and fade timing. `slowdown` is folded into `duration` at parse time
/// so only one value reaches the renderer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnimationConfig {
    pub enabled: bool,
    pub duration: Duration,
}

/// How the overview draws itself: how far the workspaces shrink, and what shows
/// through around them once they no longer cover the output.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverviewConfig {
    pub zoom: f64,
    pub backdrop_color: [f32; 4],
}

impl AnimationConfig {
    /// Animations need both the switch and a non-zero duration to be visible.
    pub fn active(&self) -> bool {
        self.enabled && !self.duration.is_zero()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct InputConfig {
    pub focus_follows_mouse: bool,
    pub middle_click_paste: bool,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            focus_follows_mouse: false,
            middle_click_paste: true,
        }
    }
}

/// Which clients may bind the globals that expose other clients' windows.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct SecurityConfig {
    pub privileged_clients: Vec<String>,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            privileged_clients: vec![
                "xdg-desktop-portal-wlr".into(),
                "xdg-desktop-portal".into(),
                "wl-paste".into(),
                "wl-copy".into(),
            ],
        }
    }
}

#[derive(Debug)]
pub struct Config {
    pub layout: LayoutConfig,
    pub appearance: AppearanceConfig,
    pub animations: AnimationConfig,
    pub overview: OverviewConfig,
    pub input: InputConfig,
    pub theme: ThemeConfig,
    pub backend: BackendConfig,
    pub security: SecurityConfig,
    pub monitors: Vec<MonitorConfig>,
    pub autostart: Vec<Vec<String>>,
    pub scratchpads: Vec<ScratchpadConfig>,
    pub binding_commands: Vec<Vec<String>>,
    bindings: Vec<(Binding, Action)>,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeConfig {
    pub cursor_theme: Option<String>,
    pub cursor_size: Option<u16>,
    pub icon_theme: Option<String>,
    pub gtk_theme: Option<String>,
    pub qt_style: Option<String>,
    pub font: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct BackendConfig {
    pub direct_scanout: bool,
    pub drm_device: Option<PathBuf>,
    pub connector: Option<String>,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            direct_scanout: true,
            drm_device: None,
            connector: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MonitorConfig {
    pub connector: String,
    pub focus_at_startup: bool,
    pub resolution: Option<[u16; 2]>,
    pub refresh: Option<f64>,
    pub position: Option<[i32; 2]>,
    pub scale: f64,
    pub transform: Transform,
    pub vrr: VrrPolicy,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum VrrPolicy {
    Never,
    #[default]
    Automatic,
    Always,
}

impl VrrPolicy {
    pub fn enabled(self, fullscreen: bool) -> bool {
        match self {
            Self::Never => false,
            Self::Automatic => fullscreen,
            Self::Always => true,
        }
    }
}

impl Config {
    pub fn startup_monitor(&self) -> Option<&str> {
        self.monitors
            .iter()
            .find(|monitor| monitor.focus_at_startup)
            .map(|monitor| monitor.connector.as_str())
    }

    pub fn action(&self, modifiers: &ModifiersState, keysym: Keysym) -> Option<Action> {
        self.triggered(modifiers, BindingTrigger::Key(keysym))
            .filter(|action| self.layout.tab_grouping || !action.is_tab_grouping())
    }

    pub fn wheel_action(
        &self,
        modifiers: &ModifiersState,
        direction: ScrollDirection,
    ) -> Option<Action> {
        self.triggered(modifiers, BindingTrigger::Wheel(direction))
            .filter(|action| self.layout.tab_grouping || !action.is_tab_grouping())
    }

    pub fn tab_group_drag(&self, modifiers: &ModifiersState, button: u32) -> bool {
        self.layout.tab_grouping
            && self.triggered(modifiers, BindingTrigger::Button(button))
                == Some(Action::TabGroupDrag)
    }

    fn triggered(&self, modifiers: &ModifiersState, trigger: BindingTrigger) -> Option<Action> {
        self.bindings.iter().find_map(|(binding, action)| {
            (binding.trigger == trigger && binding.matches_modifiers(modifiers)).then_some(*action)
        })
    }

    pub fn apply_runtime(&mut self, updated: Self) -> bool {
        let restart_required = self.backend != updated.backend
            || monitors_require_restart(&self.monitors, &updated.monitors)
            || self.autostart != updated.autostart
            || self.input.middle_click_paste != updated.input.middle_click_paste
            || self.security != updated.security
            || theme_requires_restart(&self.theme, &updated.theme);
        self.layout = updated.layout;
        self.appearance = updated.appearance;
        self.animations = updated.animations;
        self.input.focus_follows_mouse = updated.input.focus_follows_mouse;
        self.theme.cursor_theme = updated.theme.cursor_theme;
        self.theme.cursor_size = updated.theme.cursor_size;
        self.theme.icon_theme = updated.theme.icon_theme;
        self.theme.gtk_theme = updated.theme.gtk_theme;
        apply_live_monitor_fields(&mut self.monitors, &updated.monitors);
        self.binding_commands = updated.binding_commands;
        self.scratchpads = updated.scratchpads;
        self.bindings = updated.bindings;
        restart_required
    }
}

impl Action {
    fn is_tab_grouping(self) -> bool {
        matches!(
            self,
            Self::TabGroupLeft
                | Self::TabGroupRight
                | Self::PreviousTab
                | Self::NextTab
                | Self::TabGroupDrag
        )
    }
}

fn theme_requires_restart(current: &ThemeConfig, updated: &ThemeConfig) -> bool {
    current.qt_style != updated.qt_style || current.font != updated.font
}

fn monitors_require_restart(current: &[MonitorConfig], updated: &[MonitorConfig]) -> bool {
    let common_order = |left: &[MonitorConfig], right: &[MonitorConfig]| {
        left.iter()
            .filter(|monitor| {
                right
                    .iter()
                    .any(|candidate| candidate.connector == monitor.connector)
            })
            .map(|monitor| monitor.connector.clone())
            .collect::<Vec<_>>()
    };
    if common_order(current, updated) != common_order(updated, current) {
        return true;
    }

    current.iter().chain(updated).any(|monitor| {
        let old = current
            .iter()
            .find(|candidate| candidate.connector == monitor.connector);
        let new = updated
            .iter()
            .find(|candidate| candidate.connector == monitor.connector);
        monitor_restart_fields(old) != monitor_restart_fields(new)
    })
}

fn monitor_restart_fields(monitor: Option<&MonitorConfig>) -> (bool, Option<[i32; 2]>, VrrPolicy) {
    monitor.map_or((false, None, VrrPolicy::Automatic), |monitor| {
        (monitor.focus_at_startup, monitor.position, monitor.vrr)
    })
}

/// Copy only the monitor fields supported at runtime. Existing entries retain
/// restart-only state; a live-only entry can still be added for an active output.
fn apply_live_monitor_fields(current: &mut Vec<MonitorConfig>, updated: &[MonitorConfig]) {
    for monitor in current.iter_mut() {
        if let Some(updated) = updated
            .iter()
            .find(|candidate| candidate.connector == monitor.connector)
        {
            monitor.resolution = updated.resolution;
            monitor.refresh = updated.refresh;
            monitor.scale = updated.scale;
            monitor.transform = updated.transform;
        } else {
            monitor.resolution = None;
            monitor.refresh = None;
            monitor.scale = 1.0;
            monitor.transform = Transform::Normal;
        }
    }
    for monitor in updated {
        if current
            .iter()
            .any(|candidate| candidate.connector == monitor.connector)
        {
            continue;
        }
        current.push(MonitorConfig {
            connector: monitor.connector.clone(),
            focus_at_startup: false,
            resolution: monitor.resolution,
            refresh: monitor.refresh,
            position: None,
            scale: monitor.scale,
            transform: monitor.transform,
            vrr: VrrPolicy::Automatic,
        });
    }
    current.retain(|monitor| {
        updated
            .iter()
            .any(|candidate| candidate.connector == monitor.connector)
            || monitor_restart_fields(Some(monitor)) != monitor_restart_fields(None)
    });
}

/// Merge `overlay` into `base`, key by key.
///
/// This is how `config.toml` lands on the compiled defaults: tables merge, so a
/// hand-written section only has to name what it changes. Everything else
/// replaces, arrays included: `enabled_layouts`, `preset_column_widths`, the
/// keybind lists and `[[monitors]]` are complete statements rather than
/// additions, and appending to them would produce configs nobody asked for.
///
/// `config.d/` drop-ins do not come through here - see [`replace_sections`].
fn merge(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base), toml::Value::Table(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

/// Overlay a `config.d/` drop-in onto `base`, replacing each section it names
/// whole rather than merging into it key by key.
///
/// A drop-in is generated by a settings UI, and a section it writes is a section
/// it owns. Mixing its `[appearance]` into the user's key by key leaves a
/// coupled rule like `focus_ring_width <= gaps` with two writers and no single
/// one able to keep it - which is exactly how a settings slider once refused
/// every later edit. One section, one owner. It also means deleting `config.d/`
/// hands every section back to `config.toml` precisely as its author left it.
fn replace_sections(base: &mut toml::Value, drop_in: toml::Value) {
    if let (toml::Value::Table(base), toml::Value::Table(drop_in)) = (base, drop_in) {
        for (section, value) in drop_in {
            base.insert(section, value);
        }
    }
}

/// Watches every file feeding the merged config and re-reads them as a set.
pub struct ConfigWatcher {
    path: PathBuf,
    /// Each layer's text in merge order. Compared verbatim so that saving a
    /// file without changing it does not churn the compositor.
    sources: Vec<String>,
    /// Layer count and newest mtime. The cheap gate the twice-a-second poll
    /// stops at when nothing changed; the count is what catches a drop-in being
    /// added or deleted without any surviving file being touched.
    stamp: Option<(usize, std::time::SystemTime)>,
    /// The merged document behind the live `Config`. Kept so the IPC layer can
    /// hand a shell the effective config without a second copy of the defaults.
    document: toml::Value,
}

impl ConfigWatcher {
    /// The files that feed the merge, lowest precedence first: `config.toml`,
    /// then every `config.d/*.toml` in filename order.
    ///
    /// The drop-in directory is what lets a settings GUI own one generated file
    /// without ever writing the config a user hand-edits.
    fn layers(&self) -> Vec<PathBuf> {
        let mut layers = vec![self.path.clone()];
        let Some(directory) = self.path.parent().map(|parent| parent.join(OVERLAY_DIR)) else {
            return layers;
        };
        // A missing drop-in directory is the normal case, not an error.
        let Ok(entries) = fs::read_dir(directory) else {
            return layers;
        };
        let mut overlays = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "toml")
            })
            .collect::<Vec<_>>();
        overlays.sort();
        layers.extend(overlays);
        layers
    }

    /// `None` when any layer's metadata is unreadable, which falls through to
    /// the full read rather than guessing - the behaviour this had before.
    fn stamp(layers: &[PathBuf]) -> Option<(usize, std::time::SystemTime)> {
        let mut newest = std::time::SystemTime::UNIX_EPOCH;
        for layer in layers {
            let modified = fs::metadata(layer).and_then(|data| data.modified()).ok()?;
            newest = newest.max(modified);
        }
        Some((layers.len(), newest))
    }

    /// Read every layer and merge it over the compiled defaults.
    ///
    /// Starting from `DEFAULT_CONFIG` rather than an empty document is what
    /// makes a partial drop-in legal: `RawConfig` denies unknown fields and
    /// requires `layout` and `bindings`, and the base layer always supplies
    /// them. It also means the merged document is the complete effective
    /// config, with no second copy of the defaults anywhere.
    fn read(layers: &[PathBuf]) -> io::Result<(toml::Value, Vec<String>)> {
        let mut document =
            toml::from_str(DEFAULT_CONFIG).expect("the compiled default config is valid TOML");
        let mut sources = Vec::with_capacity(layers.len());
        for (index, layer) in layers.iter().enumerate() {
            let source = fs::read_to_string(layer)
                .map_err(|error| invalid_data(layer, format!("could not read config: {error}")))?;
            let overlay = toml::from_str(&source)
                .map_err(|error: toml::de::Error| invalid_data(layer, error.to_string()))?;
            // `config.toml` is the base layer and merges key by key, so a partial
            // hand-written section still inherits the compiled defaults around it.
            // Everything after it is a `config.d/` drop-in, which owns whole
            // sections - see `replace_sections`.
            if index == 0 {
                merge(&mut document, overlay);
            } else {
                replace_sections(&mut document, overlay);
            }
            sources.push(source);
        }
        Ok((document, sources))
    }

    /// The merged config as it was last parsed successfully.
    pub fn document(&self) -> &toml::Value {
        &self.document
    }

    pub fn reload(&mut self) -> io::Result<Option<Config>> {
        let layers = self.layers();
        let stamp = Self::stamp(&layers);
        if stamp.is_some() && stamp == self.stamp {
            return Ok(None);
        }
        self.stamp = stamp;

        let (document, sources) = Self::read(&layers)?;
        if sources == self.sources {
            return Ok(None);
        }
        self.sources = sources;
        let config = parse_value(document.clone()).map_err(|error| blame(&layers, error))?;
        // Only now: `document()` is what Paw2 is running, and a rejected save
        // must leave the last config that parsed on show, not its own values
        // under a banner saying they were refused.
        self.document = document;
        Ok(Some(config))
    }

    /// Parse `config.toml` alone, ignoring `config.d/`.
    ///
    /// A drop-in is generated by the shell's settings UI. A value it rejects
    /// would otherwise cost the session the whole config, `autostart` included,
    /// so the shell that would show the error is the one thing that cannot
    /// start. Keeping the hand-written layer breaks that circle.
    ///
    /// `stamp` and `sources` stay as the failed [`Self::reload`] left them, so
    /// the next edit to the drop-in retries the full merge.
    fn base_only(&mut self) -> io::Result<Config> {
        let layers = [self.path.clone()];
        let (document, _) = Self::read(&layers)?;
        let config = parse_value(document.clone()).map_err(|error| blame(&layers, error))?;
        self.document = document;
        Ok(config)
    }
}

/// Name every layer that fed a merge in the error from validating it.
///
/// Validation runs on the merged document, which carries no record of which
/// layer supplied a given key. Naming them all beats confidently blaming
/// config.toml for a drop-in's typo.
fn blame(layers: &[PathBuf], error: String) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "{}: {error}",
            layers
                .iter()
                .map(|layer| layer.display().to_string())
                .collect::<Vec<_>>()
                .join(" + ")
        ),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Binding {
    ctrl: bool,
    alt: bool,
    shift: bool,
    logo: bool,
    trigger: BindingTrigger,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BindingTrigger {
    Key(Keysym),
    Wheel(ScrollDirection),
    Button(u32),
}

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollDirection {
    Up,
    Down,
}

impl Binding {
    fn parse(value: &str) -> Result<Self, String> {
        let mut parts = value.split('+').peekable();
        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;
        let mut logo = false;
        let mut trigger = None;

        while let Some(part) = parts.next() {
            if part.is_empty() {
                return Err(format!("invalid empty component in binding `{value}`"));
            }

            if parts.peek().is_none() {
                trigger = Some(match part.to_ascii_lowercase().as_str() {
                    "wheelscrollup" => BindingTrigger::Wheel(ScrollDirection::Up),
                    "wheelscrolldown" => BindingTrigger::Wheel(ScrollDirection::Down),
                    "mouseleft" => BindingTrigger::Button(BTN_LEFT),
                    "mouseright" => BindingTrigger::Button(BTN_RIGHT),
                    "mousemiddle" => BindingTrigger::Button(BTN_MIDDLE),
                    _ => {
                        let keysym = xkb::keysym_from_name(part, xkb::KEYSYM_CASE_INSENSITIVE);
                        if keysym.raw() == keysyms::KEY_NoSymbol {
                            return Err(format!("unknown key `{part}` in binding `{value}`"));
                        }
                        BindingTrigger::Key(keysym)
                    }
                });
                break;
            }

            let modifier = match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => &mut ctrl,
                "alt" => &mut alt,
                "shift" => &mut shift,
                "super" | "logo" => &mut logo,
                _ => return Err(format!("unknown modifier `{part}` in binding `{value}`")),
            };
            if std::mem::replace(modifier, true) {
                return Err(format!("duplicate modifier `{part}` in binding `{value}`"));
            }
        }

        Ok(Self {
            ctrl,
            alt,
            shift,
            logo,
            trigger: trigger.ok_or_else(|| format!("binding `{value}` has no trigger"))?,
        })
    }

    fn matches_modifiers(&self, modifiers: &ModifiersState) -> bool {
        self.ctrl == modifiers.ctrl
            && self.alt == modifiers.alt
            && self.shift == modifiers.shift
            && self.logo == modifiers.logo
            && !modifiers.iso_level3_shift
            && !modifiers.iso_level5_shift
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    layout: LayoutConfig,
    #[serde(default)]
    appearance: RawAppearance,
    #[serde(default)]
    input: InputConfig,
    #[serde(default)]
    theme: ThemeConfig,
    #[serde(default)]
    backend: BackendConfig,
    #[serde(default)]
    security: SecurityConfig,
    #[serde(default)]
    monitors: Vec<RawMonitor>,
    #[serde(default)]
    autostart: RawAutostart,
    #[serde(default)]
    animations: RawAnimations,
    #[serde(default)]
    overview: RawOverview,
    #[serde(default)]
    scratchpads: BTreeMap<String, RawScratchpad>,
    bindings: RawBindings,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawScratchpad {
    command: Vec<String>,
    app_id: String,
    edge: ScratchpadEdge,
    size: [f64; 2],
    position: Option<[f64; 2]>,
    margin: u16,
}

impl Default for RawScratchpad {
    fn default() -> Self {
        Self {
            command: Vec::new(),
            app_id: String::new(),
            edge: ScratchpadEdge::Top,
            size: [75.0, 60.0],
            position: None,
            margin: 24,
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawAppearance {
    gaps: u16,
    focus_ring_width: u16,
    focus_ring_color: String,
    corner_radius: u16,
    shadow_radius: u16,
    shadow_color: String,
    shadow_opacity: f64,
    shadow_offset_x: i16,
    shadow_offset_y: i16,
    blur: bool,
    blur_radius: u16,
    transparency: bool,
    opacity: f64,
}

impl Default for RawAppearance {
    fn default() -> Self {
        Self {
            gaps: 15,
            focus_ring_width: 1,
            focus_ring_color: "#0b3d91".into(),
            corner_radius: 8,
            shadow_radius: 12,
            shadow_color: "#000000".into(),
            shadow_opacity: 0.45,
            shadow_offset_x: 0,
            shadow_offset_y: 4,
            blur: true,
            blur_radius: 8,
            transparency: true,
            opacity: 0.95,
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawAnimations {
    enabled: bool,
    duration_ms: u16,
    slowdown: f64,
}

impl Default for RawAnimations {
    fn default() -> Self {
        Self {
            enabled: true,
            duration_ms: 150,
            slowdown: 1.0,
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawOverview {
    zoom: f64,
    backdrop_color: String,
}

impl Default for RawOverview {
    fn default() -> Self {
        Self {
            zoom: 0.5,
            backdrop_color: "#1c1c1c".into(),
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawBindings {
    /// Action name to key list, validated against `DEFAULT_BINDINGS`.
    #[serde(flatten)]
    actions: BTreeMap<String, Vec<String>>,
    commands: BTreeMap<String, Vec<String>>,
    scratchpads: BTreeMap<String, String>,
    workspace: RawWorkspaceBindings,
}

/// Every directly bindable action: its config key and its out-of-the-box keys,
/// space separated. An action the config does not mention keeps the keys listed
/// here, so a config written before that action existed still gets it.
#[rustfmt::skip]
const DEFAULT_BINDINGS: &[(&str, Action, &str)] = &[
    ("focus_left",             Action::FocusLeft,            "Super+h Super+Left"),
    ("focus_right",            Action::FocusRight,           "Super+l Super+Right"),
    ("focus_up",               Action::FocusUp,              "Super+k Super+Up"),
    ("focus_down",             Action::FocusDown,            "Super+j Super+Down"),
    ("swap_left",              Action::SwapLeft,             "Super+Shift+h Super+Shift+Left"),
    ("swap_right",             Action::SwapRight,            "Super+Shift+l Super+Shift+Right"),
    ("swap_up",                Action::SwapUp,               "Super+Shift+k Super+Shift+Up"),
    ("swap_down",              Action::SwapDown,             "Super+Shift+j Super+Shift+Down"),
    ("consume_or_expel_left",  Action::ConsumeOrExpelLeft,   "Super+bracketleft"),
    ("consume_or_expel_right", Action::ConsumeOrExpelRight,  "Super+bracketright"),
    ("tab_group_left",         Action::TabGroupLeft,          "Super+Shift+bracketleft"),
    ("tab_group_right",        Action::TabGroupRight,         "Super+Shift+bracketright"),
    ("previous_tab",           Action::PreviousTab,           "Super+Ctrl+bracketleft"),
    ("next_tab",               Action::NextTab,               "Super+Ctrl+bracketright"),
    ("tab_group_drag",         Action::TabGroupDrag,          "Super+Ctrl+MouseLeft"),
    ("cycle_column_width",     Action::CycleColumnWidth,     "Super+backslash"),
    ("cycle_layout",           Action::CycleLayout,          "Super+Tab"),
    ("switch_layout_scrolling", Action::SwitchLayout(LayoutMode::Scrolling), ""),
    ("switch_layout_dwindle", Action::SwitchLayout(LayoutMode::Dwindle), ""),
    ("switch_layout_master_stack", Action::SwitchLayout(LayoutMode::MasterStack), ""),
    ("switch_layout_monocle", Action::SwitchLayout(LayoutMode::Monocle), ""),
    ("decrease_column_width",  Action::DecreaseColumnWidth,  ""),
    ("increase_column_width",  Action::IncreaseColumnWidth,  ""),
    ("decrease_row_height",    Action::DecreaseRowHeight,    ""),
    ("increase_row_height",    Action::IncreaseRowHeight,    ""),
    ("toggle_floating",        Action::ToggleFloating,       "Super+Shift+space"),
    ("close_window",           Action::CloseWindow,          "Super+q Super+BackSpace"),
    ("toggle_fullscreen",      Action::ToggleFullscreen,     "Super+Shift+f"),
    ("toggle_overview",        Action::ToggleOverview,       "Super+o"),
    ("toggle_scratchpad_overview", Action::ToggleScratchpadOverview, "Super+Shift+o"),
    ("focus_monitor_left",     Action::FocusMonitorLeft,     "Super+comma"),
    ("focus_monitor_right",    Action::FocusMonitorRight,    "Super+period"),
    ("focus_monitor_up",       Action::FocusMonitorUp,       ""),
    ("focus_monitor_down",     Action::FocusMonitorDown,     ""),
    ("move_to_monitor_left",   Action::MoveToMonitorLeft,    "Super+Shift+comma"),
    ("move_to_monitor_right",  Action::MoveToMonitorRight,   "Super+Shift+period"),
    ("move_to_monitor_up",     Action::MoveToMonitorUp,      "Super+Ctrl+Shift+Up"),
    ("move_to_monitor_down",   Action::MoveToMonitorDown,    "Super+Ctrl+Shift+Down"),
    ("quit",                   Action::Quit,                 "Super+Shift+Escape"),
];

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawWorkspaceBindings {
    switch: Vec<Vec<String>>,
    r#move: Vec<Vec<String>>,
    previous: Vec<String>,
    next: Vec<String>,
    move_previous: Vec<String>,
    move_next: Vec<String>,
}

impl Default for RawWorkspaceBindings {
    fn default() -> Self {
        Self {
            switch: (1..=9)
                .map(|number| vec![format!("Super+{number}")])
                .collect(),
            r#move: (1..=9)
                .map(|number| vec![format!("Super+Shift+{number}")])
                .collect(),
            previous: vec!["Super+Page_Up".into()],
            next: vec!["Super+Page_Down".into()],
            move_previous: vec!["Super+Shift+Page_Up".into()],
            move_next: vec!["Super+Shift+Page_Down".into()],
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMonitor {
    connector: String,
    #[serde(default)]
    focus_at_startup: bool,
    resolution: Option<[u16; 2]>,
    refresh: Option<f64>,
    position: Option<[i32; 2]>,
    #[serde(default = "default_monitor_scale")]
    scale: f64,
    #[serde(default = "default_transform")]
    transform: String,
    #[serde(default)]
    vrr: VrrPolicy,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAutostart {
    #[serde(default)]
    commands: Vec<Vec<String>>,
}

fn default_preset_column_widths() -> Vec<f64> {
    vec![0.33333, 0.5, 0.66667]
}

fn default_enabled_layouts() -> Vec<LayoutMode> {
    vec![
        LayoutMode::Scrolling,
        LayoutMode::Dwindle,
        LayoutMode::MasterStack,
        LayoutMode::Monocle,
    ]
}

fn default_transform() -> String {
    "normal".into()
}

fn default_monitor_scale() -> f64 {
    1.0
}

/// Load the config, plus the watcher that keeps it current and the reason the
/// user's config was not fully used, if it was not.
///
/// A broken config is not fatal. At runtime an invalid save already leaves the
/// last working settings active; at startup there are no working settings to
/// keep, so Paw2 drops layers instead. Exiting here would mean a display
/// manager returns the user to the greeter with the reason on a stderr nobody
/// will read - a whole session lost to a typo. Starting means they get a
/// desktop, the error reaches the log and the shell, and the watcher picks up
/// the fix within half a second.
///
/// Layers are dropped in the order that keeps the most of what a person wrote:
/// the full merge, then `config.toml` alone without the shell's generated
/// `config.d/`, then the compiled defaults.
pub fn load() -> io::Result<(Config, ConfigWatcher, Option<String>)> {
    let path = config_path()?;
    create_default(&path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "{}: could not create default config: {error}",
                path.display()
            ),
        )
    })?;
    let mut watcher = ConfigWatcher {
        path,
        sources: Vec::new(),
        stamp: None,
        document: toml::Value::Table(toml::Table::new()),
    };
    // A fresh watcher has no stamp and no cached sources, so neither change
    // gate can short-circuit here; this reads, and seeds both for the poll.
    let error = match watcher.reload() {
        Ok(config) => {
            return Ok((
                config.expect("a fresh watcher always reports the config as changed"),
                watcher,
                None,
            ));
        }
        Err(error) => error.to_string(),
    };
    match watcher.base_only() {
        Ok(config) => {
            tracing::error!(%error, "ignoring config.d/ and starting from config.toml");
            Ok((config, watcher, Some(error)))
        }
        Err(_) => {
            tracing::error!(%error, "starting with the default configuration");
            // What Paw2 is running, so a shell has something to show beside the
            // error rather than an empty document.
            watcher.document =
                toml::from_str(DEFAULT_CONFIG).expect("the compiled default config is valid TOML");
            Ok((defaults(), watcher, Some(error)))
        }
    }
}

/// The compiled defaults, as a `Config`. Also the base layer of every merge,
/// so `parse(DEFAULT_CONFIG)` is already covered by the tests.
fn defaults() -> Config {
    parse_value(toml::from_str(DEFAULT_CONFIG).expect("the compiled default config is valid TOML"))
        .expect("the compiled default config is valid")
}

fn config_path() -> io::Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        && path.is_absolute()
    {
        return Ok(path.join("paw2/config.toml"));
    }

    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|path| path.join(".config/paw2/config.toml"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "neither XDG_CONFIG_HOME nor HOME is set",
            )
        })
}

fn create_default(path: &Path) -> io::Result<()> {
    let directory = path.parent().unwrap();
    // Created even when the config already exists, so a settings GUI always has
    // somewhere to write and the drop-in mechanism is visible in a file manager.
    fs::create_dir_all(directory.join(OVERLAY_DIR))?;
    if path.exists() {
        return Ok(());
    }

    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => file.write_all(DEFAULT_CONFIG.as_bytes()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

/// Parse a single config document. Production goes through `ConfigWatcher`,
/// which merges several; this is the one-document form the tests are written
/// against.
#[cfg(test)]
fn parse(source: &str) -> Result<Config, String> {
    parse_value(toml::from_str(source).map_err(|error: toml::de::Error| error.to_string())?)
}

fn parse_value(document: toml::Value) -> Result<Config, String> {
    let raw: RawConfig = document
        .try_into()
        .map_err(|error: toml::de::Error| error.to_string())?;
    if raw.layout.enabled_layouts.is_empty() {
        return Err("layout.enabled_layouts must not be empty".into());
    }
    if raw
        .layout
        .enabled_layouts
        .iter()
        .enumerate()
        .any(|(index, layout)| raw.layout.enabled_layouts[..index].contains(layout))
    {
        return Err("layout.enabled_layouts must not contain duplicates".into());
    }
    if !raw.layout.column_width.is_finite() || !(0.1..=1.0).contains(&raw.layout.column_width) {
        return Err("layout.column_width must be between 0.1 and 1.0".into());
    }
    if raw.layout.preset_column_widths.is_empty()
        || raw
            .layout
            .preset_column_widths
            .iter()
            .any(|width| !width.is_finite() || !(0.1..=1.0).contains(width))
    {
        return Err("layout.preset_column_widths must contain values between 0.1 and 1.0".into());
    }
    if raw.appearance.gaps > 256 {
        return Err("appearance.gaps must be at most 256".into());
    }
    if raw.appearance.focus_ring_width > raw.appearance.gaps {
        return Err("appearance.focus_ring_width cannot exceed appearance.gaps".into());
    }
    if raw.appearance.blur_radius > 64 {
        return Err("appearance.blur_radius must be at most 64".into());
    }
    if raw.appearance.corner_radius > 64 {
        return Err("appearance.corner_radius must be at most 64".into());
    }
    if raw.appearance.shadow_radius > 64 {
        return Err("appearance.shadow_radius must be at most 64".into());
    }
    if !raw.appearance.shadow_opacity.is_finite()
        || !(0.0..=1.0).contains(&raw.appearance.shadow_opacity)
    {
        return Err("appearance.shadow_opacity must be between 0.0 and 1.0".into());
    }
    for (name, offset) in [
        ("shadow_offset_x", raw.appearance.shadow_offset_x),
        ("shadow_offset_y", raw.appearance.shadow_offset_y),
    ] {
        if !(-64..=64).contains(&offset) {
            return Err(format!("appearance.{name} must be between -64 and 64"));
        }
    }
    if !raw.appearance.opacity.is_finite() || !(0.1..=1.0).contains(&raw.appearance.opacity) {
        return Err("appearance.opacity must be between 0.1 and 1.0".into());
    }
    if raw.animations.duration_ms > 5000 {
        return Err("animations.duration_ms must be at most 5000".into());
    }
    if !raw.animations.slowdown.is_finite() || !(0.1..=100.0).contains(&raw.animations.slowdown) {
        return Err("animations.slowdown must be between 0.1 and 100.0".into());
    }
    for (name, value) in [
        ("cursor_theme", &raw.theme.cursor_theme),
        ("icon_theme", &raw.theme.icon_theme),
        ("gtk_theme", &raw.theme.gtk_theme),
        ("qt_style", &raw.theme.qt_style),
        ("font", &raw.theme.font),
    ] {
        if value.as_ref().is_some_and(|value| value.trim().is_empty()) {
            return Err(format!("theme.{name} cannot be empty"));
        }
    }
    if raw
        .theme
        .cursor_size
        .is_some_and(|size| !(1..=256).contains(&size))
    {
        return Err("theme.cursor_size must be between 1 and 256".into());
    }
    let focus_ring_color = parse_color(
        &raw.appearance.focus_ring_color,
        "appearance.focus_ring_color",
    )?;
    let shadow_color = premultiply(
        parse_color(&raw.appearance.shadow_color, "appearance.shadow_color")?,
        raw.appearance.shadow_opacity as f32,
    );
    if !(OVERVIEW_ZOOM_RANGE).contains(&raw.overview.zoom) {
        let (low, high) = (OVERVIEW_ZOOM_RANGE.start(), OVERVIEW_ZOOM_RANGE.end());
        return Err(format!("overview.zoom must be between {low} and {high}"));
    }
    let backdrop_color = parse_color(&raw.overview.backdrop_color, "overview.backdrop_color")?;
    if let Some(path) = &raw.backend.drm_device
        && !path.is_absolute()
    {
        return Err("backend.drm_device must be an absolute path".into());
    }
    if raw
        .backend
        .connector
        .as_ref()
        .is_some_and(|connector| connector.trim().is_empty())
    {
        return Err("backend.connector cannot be empty".into());
    }
    if raw
        .security
        .privileged_clients
        .iter()
        .any(|name| name.trim().is_empty())
    {
        return Err("security.privileged_clients entries cannot be empty".into());
    }
    for (index, command) in raw.autostart.commands.iter().enumerate() {
        if command.is_empty() || command[0].trim().is_empty() {
            return Err(format!(
                "autostart.commands[{index}] must contain an executable"
            ));
        }
    }
    if raw.backend.connector.is_some() && !raw.monitors.is_empty() {
        return Err("backend.connector cannot be used with [[monitors]]".into());
    }

    let mut monitors = Vec::new();
    for monitor in raw.monitors {
        if monitor.connector.trim().is_empty() {
            return Err("monitors.connector cannot be empty".into());
        }
        if monitors
            .iter()
            .any(|item: &MonitorConfig| item.connector == monitor.connector)
        {
            return Err(format!(
                "duplicate monitor connector `{}`",
                monitor.connector
            ));
        }
        if monitor.focus_at_startup
            && monitors
                .iter()
                .any(|item: &MonitorConfig| item.focus_at_startup)
        {
            return Err("only one monitor can set focus_at_startup = true".into());
        }
        if monitor
            .resolution
            .is_some_and(|[width, height]| width == 0 || height == 0)
        {
            return Err(format!(
                "monitor `{}` resolution must be non-zero",
                monitor.connector
            ));
        }
        if monitor
            .refresh
            .is_some_and(|refresh| !refresh.is_finite() || refresh <= 0.0)
        {
            return Err(format!(
                "monitor `{}` refresh must be positive",
                monitor.connector
            ));
        }
        if !monitor.scale.is_finite() || monitor.scale <= 0.0 {
            return Err(format!(
                "monitor `{}` scale must be positive",
                monitor.connector
            ));
        }
        monitors.push(MonitorConfig {
            connector: monitor.connector,
            focus_at_startup: monitor.focus_at_startup,
            resolution: monitor.resolution,
            refresh: monitor.refresh,
            position: monitor.position,
            scale: monitor.scale,
            transform: parse_transform(&monitor.transform)?,
            vrr: monitor.vrr,
        });
    }

    let mut scratchpads = Vec::new();
    for (name, scratchpad) in raw.scratchpads {
        if name.trim().is_empty() {
            return Err("scratchpad names cannot be empty".into());
        }
        if scratchpad.command.is_empty() || scratchpad.command[0].trim().is_empty() {
            return Err(format!(
                "scratchpads.{name}.command must contain an executable"
            ));
        }
        if scratchpad.app_id.trim().is_empty() {
            return Err(format!("scratchpads.{name}.app_id cannot be empty"));
        }
        if scratchpads
            .iter()
            .any(|item: &ScratchpadConfig| item.app_id == scratchpad.app_id)
        {
            return Err(format!(
                "scratchpad app_id `{}` is configured more than once",
                scratchpad.app_id
            ));
        }
        if scratchpad
            .size
            .iter()
            .any(|value| !value.is_finite() || !(1.0..=100.0).contains(value))
        {
            return Err(format!(
                "scratchpads.{name}.size values must be between 1 and 100"
            ));
        }
        if let Some(position) = scratchpad.position
            && (position
                .iter()
                .any(|value| !value.is_finite() || !(0.0..=100.0).contains(value))
                || position[0] + scratchpad.size[0] > 100.0
                || position[1] + scratchpad.size[1] > 100.0)
        {
            return Err(format!(
                "scratchpads.{name}.position and size must fit within 100 percent"
            ));
        }
        if scratchpad.margin > 256 {
            return Err(format!("scratchpads.{name}.margin must be at most 256"));
        }
        scratchpads.push(ScratchpadConfig {
            name,
            command: scratchpad.command,
            app_id: scratchpad.app_id,
            edge: scratchpad.edge,
            size: scratchpad.size,
            position: scratchpad.position,
            margin: scratchpad.margin.into(),
        });
    }

    let mut bindings = Vec::new();
    let workspace = raw.bindings.workspace;
    let command_bindings = raw.bindings.commands;
    let scratchpad_bindings = raw.bindings.scratchpads;
    if workspace.switch.len() != 9 || workspace.r#move.len() != 9 {
        return Err("bindings.workspace switch and move must each contain exactly 9 lists".into());
    }
    if let Some(unknown) = raw
        .bindings
        .actions
        .keys()
        .find(|name| !DEFAULT_BINDINGS.iter().any(|(known, _, _)| known == name))
    {
        return Err(format!("unknown binding action `{unknown}`"));
    }
    for (name, action, defaults) in DEFAULT_BINDINGS {
        match raw.bindings.actions.get(*name) {
            Some(values) => {
                for value in values {
                    add_binding(&mut bindings, value, *action)?;
                }
            }
            None => {
                for value in defaults.split_whitespace() {
                    add_binding(&mut bindings, value, *action)?;
                }
            }
        }
    }

    let mut configured = vec![
        (workspace.previous, Action::PreviousWorkspace),
        (workspace.next, Action::NextWorkspace),
        (workspace.move_previous, Action::MoveToPreviousWorkspace),
        (workspace.move_next, Action::MoveToNextWorkspace),
    ];
    configured.extend(
        workspace
            .switch
            .into_iter()
            .enumerate()
            .map(|(index, values)| (values, Action::SwitchWorkspace(index))),
    );
    configured.extend(
        workspace
            .r#move
            .into_iter()
            .enumerate()
            .map(|(index, values)| (values, Action::MoveToWorkspace(index))),
    );
    for (values, action) in configured {
        for value in values {
            add_binding(&mut bindings, &value, action)?;
        }
    }
    let mut binding_commands = Vec::new();
    for (value, command) in command_bindings {
        if command.is_empty() || command[0].trim().is_empty() {
            return Err(format!(
                "bindings.commands.{value} must contain an executable"
            ));
        }
        let action = Action::LaunchCommand(binding_commands.len());
        add_binding(&mut bindings, &value, action)?;
        binding_commands.push(command);
    }
    for (value, name) in scratchpad_bindings {
        let Some(index) = scratchpads
            .iter()
            .position(|scratchpad| scratchpad.name == name)
        else {
            return Err(format!(
                "bindings.scratchpads.{value} names unknown scratchpad `{name}`"
            ));
        };
        add_binding(&mut bindings, &value, Action::ToggleScratchpad(index))?;
    }

    Ok(Config {
        layout: raw.layout,
        appearance: AppearanceConfig {
            gaps: raw.appearance.gaps.into(),
            focus_ring_width: raw.appearance.focus_ring_width.into(),
            focus_ring_color,
            corner_radius: raw.appearance.corner_radius.into(),
            shadow_radius: raw.appearance.shadow_radius.into(),
            shadow_color,
            shadow_offset_x: raw.appearance.shadow_offset_x.into(),
            shadow_offset_y: raw.appearance.shadow_offset_y.into(),
            blur: raw.appearance.blur,
            blur_radius: raw.appearance.blur_radius.into(),
            transparency: raw.appearance.transparency,
            opacity: raw.appearance.opacity as f32,
        },
        animations: AnimationConfig {
            enabled: raw.animations.enabled,
            duration: Duration::from_secs_f64(
                f64::from(raw.animations.duration_ms) / 1000.0 * raw.animations.slowdown,
            ),
        },
        overview: OverviewConfig {
            zoom: raw.overview.zoom,
            backdrop_color,
        },
        input: raw.input,
        theme: raw.theme,
        backend: raw.backend,
        security: raw.security,
        monitors,
        autostart: raw.autostart.commands,
        scratchpads,
        binding_commands,
        bindings,
    })
}

fn add_binding(
    bindings: &mut Vec<(Binding, Action)>,
    value: &str,
    action: Action,
) -> Result<(), String> {
    let binding = Binding::parse(value)?;
    match (binding.trigger, action) {
        (BindingTrigger::Button(_), Action::TabGroupDrag) => {}
        (BindingTrigger::Button(_), _) => {
            return Err(format!(
                "binding `{value}` uses a mouse button for a non-drag action"
            ));
        }
        (_, Action::TabGroupDrag) => {
            return Err(format!(
                "binding `{value}` for tab_group_drag must use a mouse button"
            ));
        }
        _ => {}
    }
    if let Some((_, existing_action)) = bindings.iter().find(|(item, _)| *item == binding) {
        if *existing_action != action {
            return Err(format!(
                "binding `{value}` is assigned to more than one action"
            ));
        }
    } else {
        bindings.push((binding, action));
    }
    Ok(())
}

/// Below this the previews stop being readable; above it the strip stops
/// showing more than one workspace, which is the whole point of the overview.
const OVERVIEW_ZOOM_RANGE: std::ops::RangeInclusive<f64> = 0.2..=0.75;

fn parse_color(value: &str, field: &str) -> Result<[f32; 4], String> {
    let complain = || format!("{field} must use #RRGGBB");
    let hex = value
        .strip_prefix('#')
        .filter(|hex| hex.len() == 6)
        .ok_or_else(complain)?;
    let color = u32::from_str_radix(hex, 16).map_err(|_| complain())?;
    Ok([
        ((color >> 16) & 0xff) as f32 / 255.0,
        ((color >> 8) & 0xff) as f32 / 255.0,
        (color & 0xff) as f32 / 255.0,
        1.0,
    ])
}

/// Fold an alpha into an opaque colour. Render blending expects premultiplied
/// components, so a half-transparent red is `[0.5, 0.0, 0.0, 0.5]`.
fn premultiply(color: [f32; 4], alpha: f32) -> [f32; 4] {
    [color[0] * alpha, color[1] * alpha, color[2] * alpha, alpha]
}

fn parse_transform(value: &str) -> Result<Transform, String> {
    match value.to_ascii_lowercase().as_str() {
        "normal" => Ok(Transform::Normal),
        "90" => Ok(Transform::_90),
        "180" => Ok(Transform::_180),
        "270" => Ok(Transform::_270),
        "flipped" => Ok(Transform::Flipped),
        "flipped-90" => Ok(Transform::Flipped90),
        "flipped-180" => Ok(Transform::Flipped180),
        "flipped-270" => Ok(Transform::Flipped270),
        _ => Err(format!("unknown monitor transform `{value}`")),
    }
}

fn invalid_data(path: &Path, message: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}: {message}", path.display()),
    )
}

/// The shipped allowlist, verbatim, for the tests that swap it out.
#[cfg(test)]
const PRIVILEGED: &str = r#"privileged_clients = [
  "xdg-desktop-portal-wlr",
  "xdg-desktop-portal",
  "wl-paste",
  "wl-copy",
]"#;

/// Appended to `DEFAULT_CONFIG` by the tests that need configured monitors.
#[cfg(test)]
const MONITORS: &str = r#"
[[monitors]]
connector = "DP-2"
# focus_at_startup = true
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
vrr = "automatic"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// Drops one TOML section, header and all, up to the next section header.
    fn without_section(source: &str, header: &str) -> String {
        let start = source.find(header).expect("section not found");
        let body = start + header.len();
        let end = source[body..]
            .find("\n[")
            .map_or(source.len(), |offset| body + offset + 1);
        format!("{}{}", &source[..start], &source[end..])
    }

    #[test]
    fn defaults_and_validation() {
        let config = parse(DEFAULT_CONFIG).unwrap();
        assert_eq!(config.bindings.len(), 69);
        let group_modifiers = ModifiersState {
            shift: true,
            logo: true,
            ..Default::default()
        };
        let bracket_left = xkb::keysym_from_name("bracketleft", xkb::KEYSYM_CASE_INSENSITIVE);
        assert!(!config.layout.tab_grouping);
        assert_eq!(config.action(&group_modifiers, bracket_left), None);
        assert!(!config.tab_group_drag(
            &ModifiersState {
                ctrl: true,
                logo: true,
                ..Default::default()
            },
            BTN_LEFT,
        ));
        let enabled =
            parse(&DEFAULT_CONFIG.replace("tab_grouping = false", "tab_grouping = true")).unwrap();
        assert_eq!(
            enabled.action(&group_modifiers, bracket_left),
            Some(Action::TabGroupLeft)
        );
        assert!(enabled.tab_group_drag(
            &ModifiersState {
                ctrl: true,
                logo: true,
                ..Default::default()
            },
            BTN_LEFT,
        ));
        assert!(
            parse(&DEFAULT_CONFIG.replace(
                "tab_group_drag = [\"Super+Ctrl+MouseLeft\"]",
                "tab_group_drag = [\"Super+Ctrl+x\"]",
            ))
            .unwrap_err()
            .contains("must use a mouse button")
        );
        assert!(
            parse(&DEFAULT_CONFIG.replace(
                "focus_left = [\"Super+h\", \"Super+Left\"]",
                "focus_left = [\"Super+MouseRight\"]",
            ))
            .unwrap_err()
            .contains("non-drag action")
        );
        assert_eq!(
            config.action(
                &ModifiersState {
                    shift: true,
                    logo: true,
                    ..Default::default()
                },
                xkb::keysym_from_name("space", xkb::KEYSYM_CASE_INSENSITIVE),
            ),
            Some(Action::ToggleFloating)
        );
        assert_eq!(
            config.action(
                &ModifiersState {
                    logo: true,
                    ..Default::default()
                },
                xkb::keysym_from_name("o", xkb::KEYSYM_CASE_INSENSITIVE),
            ),
            Some(Action::ToggleOverview)
        );
        let disabled = parse(&DEFAULT_CONFIG.replace(
            "toggle_floating = [\"Super+Shift+space\"]",
            "toggle_floating = []",
        ))
        .unwrap();
        assert_eq!(
            disabled.action(
                &ModifiersState {
                    shift: true,
                    logo: true,
                    ..Default::default()
                },
                xkb::keysym_from_name("space", xkb::KEYSYM_CASE_INSENSITIVE),
            ),
            None
        );
        assert_eq!(
            config.action(
                &ModifiersState {
                    ctrl: true,
                    logo: true,
                    ..Default::default()
                },
                xkb::keysym_from_name("k", xkb::KEYSYM_CASE_INSENSITIVE),
            ),
            Some(Action::DecreaseRowHeight)
        );
        assert_eq!(
            config.action(
                &ModifiersState {
                    ctrl: true,
                    logo: true,
                    ..Default::default()
                },
                xkb::keysym_from_name("Down", xkb::KEYSYM_CASE_INSENSITIVE),
            ),
            Some(Action::IncreaseRowHeight)
        );
        assert!(!config.input.focus_follows_mouse);
        assert!(config.input.middle_click_paste);
        assert_eq!(config.theme, ThemeConfig::default());
        assert_eq!(config.layout.column_width, 0.6);
        assert_eq!(config.layout.enabled_layouts, default_enabled_layouts());
        assert_eq!(config.layout.preset_column_widths, [0.33333, 0.5, 0.66667]);
        assert_eq!(
            config.action(
                &ModifiersState {
                    logo: true,
                    ..Default::default()
                },
                xkb::keysym_from_name("Tab", xkb::KEYSYM_CASE_INSENSITIVE),
            ),
            Some(Action::CycleLayout)
        );
        assert_eq!(config.appearance.gaps, 15);
        assert_eq!(config.appearance.focus_ring_width, 1);
        assert_eq!(
            config.appearance.focus_ring_color,
            [11.0 / 255.0, 61.0 / 255.0, 145.0 / 255.0, 1.0]
        );
        assert_eq!(config.appearance.blur_radius, 8);
        assert_eq!(config.appearance.corner_radius, 8);
        assert_eq!(config.appearance.shadow_radius, 12);
        assert_eq!(config.appearance.shadow_offset_y, 4);
        // Black at 45%: premultiplied, so only the alpha carries the opacity.
        assert_eq!(config.appearance.shadow_color, [0.0, 0.0, 0.0, 0.45]);
        assert!(config.appearance.shadow_enabled());
        assert!(config.appearance.blur_enabled());
        assert_eq!(config.appearance.window_opacity(), 0.95);
        assert!(config.animations.active());
        assert_eq!(config.overview.zoom, 0.5);
        assert_eq!(
            config.overview.backdrop_color,
            [28.0 / 255.0, 28.0 / 255.0, 28.0 / 255.0, 1.0]
        );
        assert!(
            parse(&DEFAULT_CONFIG.replace("zoom = 0.5", "zoom = 0.9"))
                .unwrap_err()
                .contains("overview.zoom must be between")
        );
        assert!(
            parse(&DEFAULT_CONFIG.replace(
                r##"backdrop_color = "#1c1c1c""##,
                r#"backdrop_color = "grey""#
            ))
            .unwrap_err()
            .contains("overview.backdrop_color must use #RRGGBB")
        );
        // A config written before the overview existed still gets its defaults.
        let legacy_overview = parse(
            &DEFAULT_CONFIG
                .replace("zoom = 0.5\n", "")
                .replace(r##"backdrop_color = "#1c1c1c""##, ""),
        )
        .unwrap();
        assert_eq!(legacy_overview.overview, config.overview);
        assert!(config.backend.direct_scanout);
        // The shipped config and the built-in default must not drift apart.
        assert_eq!(config.security, SecurityConfig::default());
        assert_eq!(
            config.security.privileged_clients,
            [
                "xdg-desktop-portal-wlr",
                "xdg-desktop-portal",
                "wl-paste",
                "wl-copy"
            ]
        );
        assert!(
            parse(&DEFAULT_CONFIG.replace(PRIVILEGED, r#"privileged_clients = ["  "]"#))
                .unwrap_err()
                .contains("cannot be empty")
        );
        assert_eq!(config.animations.duration, Duration::from_millis(150));
        // The shipped config configures no monitors; they are auto-detected.
        assert!(config.monitors.is_empty());
        assert_eq!(config.startup_monitor(), None);
        assert!(config.autostart.is_empty());

        let with_monitors = format!("{DEFAULT_CONFIG}{MONITORS}");
        let monitored = parse(&with_monitors).unwrap();
        assert_eq!(monitored.monitors.len(), 2);
        assert_eq!(monitored.monitors[0].connector, "DP-2");
        assert!(!monitored.monitors[0].focus_at_startup);
        assert_eq!(monitored.startup_monitor(), None);
        assert_eq!(monitored.monitors[0].vrr, VrrPolicy::Automatic);
        assert_eq!(monitored.monitors[0].scale, 1.0);
        assert_eq!(monitored.monitors[1].transform, Transform::Normal);
        let default_scale = parse(&format!(
            "{DEFAULT_CONFIG}{}",
            MONITORS.replace("scale = 1.0\n", "")
        ))
        .unwrap();
        assert_eq!(default_scale.monitors[0].scale, 1.0);
        assert_eq!(
            config.action(
                &ModifiersState {
                    shift: true,
                    logo: true,
                    ..Default::default()
                },
                xkb::keysym_from_name("k", xkb::KEYSYM_CASE_INSENSITIVE),
            ),
            Some(Action::SwapUp)
        );
        assert_eq!(
            config.action(
                &ModifiersState {
                    logo: true,
                    ..Default::default()
                },
                xkb::keysym_from_name("comma", xkb::KEYSYM_CASE_INSENSITIVE),
            ),
            Some(Action::FocusMonitorLeft)
        );
        assert_eq!(
            config.wheel_action(
                &ModifiersState {
                    logo: true,
                    ..Default::default()
                },
                ScrollDirection::Up,
            ),
            None
        );

        let invalid_width = DEFAULT_CONFIG.replace("column_width = 0.6", "column_width = 2.0");
        assert!(
            parse(&invalid_width)
                .unwrap_err()
                .contains("between 0.1 and 1.0")
        );

        let no_layouts = DEFAULT_CONFIG.replace(
            "enabled_layouts = [\"scrolling\", \"dwindle\", \"master_stack\", \"monocle\"]",
            "enabled_layouts = []",
        );
        assert!(
            parse(&no_layouts)
                .unwrap_err()
                .contains("must not be empty")
        );
        let duplicate_layout = DEFAULT_CONFIG.replace(
            "enabled_layouts = [\"scrolling\", \"dwindle\", \"master_stack\", \"monocle\"]",
            "enabled_layouts = [\"scrolling\", \"scrolling\"]",
        );
        assert!(parse(&duplicate_layout).unwrap_err().contains("duplicates"));
        let unknown_layout = DEFAULT_CONFIG.replace("\"dwindle\"", "\"grid\"");
        assert!(
            parse(&unknown_layout)
                .unwrap_err()
                .contains("unknown variant")
        );

        let invalid_presets = DEFAULT_CONFIG.replace(
            "preset_column_widths = [0.33333, 0.5, 0.66667]",
            "preset_column_widths = []",
        );
        assert!(
            parse(&invalid_presets)
                .unwrap_err()
                .contains("preset_column_widths")
        );

        let invalid_color = DEFAULT_CONFIG.replace("#0b3d91", "blue");
        assert!(parse(&invalid_color).unwrap_err().contains("#RRGGBB"));

        let invalid_ring = DEFAULT_CONFIG.replace("focus_ring_width = 1", "focus_ring_width = 16");
        assert!(parse(&invalid_ring).unwrap_err().contains("cannot exceed"));

        let invalid_corner = DEFAULT_CONFIG.replace("corner_radius = 8", "corner_radius = 65");
        assert!(
            parse(&invalid_corner)
                .unwrap_err()
                .contains("corner_radius")
        );

        let invalid_shadow = DEFAULT_CONFIG.replace("shadow_radius = 12", "shadow_radius = 65");
        assert!(
            parse(&invalid_shadow)
                .unwrap_err()
                .contains("shadow_radius")
        );

        let invalid_offset = DEFAULT_CONFIG.replace("shadow_offset_y = 4", "shadow_offset_y = -65");
        assert!(
            parse(&invalid_offset)
                .unwrap_err()
                .contains("shadow_offset_y")
        );

        // The colour error has to name the key that is wrong, not the first key
        // that happens to be a colour.
        let invalid_shadow_color =
            DEFAULT_CONFIG.replace(r##"shadow_color = "#000000""##, r#"shadow_color = "black""#);
        assert!(
            parse(&invalid_shadow_color)
                .unwrap_err()
                .contains("shadow_color")
        );

        let invalid_opacity = DEFAULT_CONFIG.replace("opacity = 0.95", "opacity = 1.5");
        assert!(parse(&invalid_opacity).unwrap_err().contains("opacity"));

        let invalid_duration = DEFAULT_CONFIG.replace("duration_ms = 150", "duration_ms = 5001");
        assert!(
            parse(&invalid_duration)
                .unwrap_err()
                .contains("duration_ms")
        );

        let invalid_slowdown = DEFAULT_CONFIG.replace("slowdown = 1.0", "slowdown = 0.0");
        assert!(parse(&invalid_slowdown).unwrap_err().contains("slowdown"));

        // slowdown scales duration; the renderer only ever sees the product.
        let slowed = DEFAULT_CONFIG.replace("slowdown = 1.0", "slowdown = 3.0");
        assert_eq!(
            parse(&slowed).unwrap().animations.duration,
            Duration::from_millis(450)
        );

        // Both switches independently disable their effect.
        let no_blur = DEFAULT_CONFIG.replace("blur = true", "blur = false");
        assert!(!parse(&no_blur).unwrap().appearance.blur_enabled());
        let opaque = DEFAULT_CONFIG.replace("transparency = true", "transparency = false");
        assert_eq!(parse(&opaque).unwrap().appearance.window_opacity(), 1.0);
        let no_scanout =
            parse(&DEFAULT_CONFIG.replace("direct_scanout = true", "direct_scanout = false"))
                .unwrap();
        assert!(!no_scanout.backend.direct_scanout);
        let mut active = parse(DEFAULT_CONFIG).unwrap();
        assert!(active.apply_runtime(no_scanout));
        assert!(active.backend.direct_scanout);
        let no_animation = DEFAULT_CONFIG.replace("enabled = true", "enabled = false");
        assert!(!parse(&no_animation).unwrap().animations.active());

        let themed = parse(&format!(
            "{DEFAULT_CONFIG}\n[theme]\ncursor_theme = \"Oxygen_Zion\"\ncursor_size = 24\nicon_theme = \"Papirus-Dark\"\ngtk_theme = \"Breeze\"\nqt_style = \"Breeze\"\nfont = \"Sans 11\"\n"
        ))
        .unwrap();
        assert_eq!(themed.theme.cursor_size, Some(24));
        assert_eq!(themed.theme.qt_style.as_deref(), Some("Breeze"));
        assert!(
            parse(&format!("{DEFAULT_CONFIG}\n[theme]\ncursor_size = 0\n"))
                .unwrap_err()
                .contains("between 1 and 256")
        );
        assert!(
            parse(&format!("{DEFAULT_CONFIG}\n[theme]\nfont = \" \"\n"))
                .unwrap_err()
                .contains("cannot be empty")
        );

        let ambiguous = DEFAULT_CONFIG.replace(
            "focus_right = [\"Super+l\", \"Super+Right\"]",
            "focus_right = [\"Super+h\"]",
        );
        assert!(
            parse(&ambiguous)
                .unwrap_err()
                .contains("more than one action")
        );

        let unknown_action = DEFAULT_CONFIG.replace("focus_left = ", "focus_lft = ");
        assert!(
            parse(&unknown_action)
                .unwrap_err()
                .contains("unknown binding action")
        );

        let invalid_binding = DEFAULT_CONFIG.replace("Super+h", "Hyper+h");
        assert!(
            parse(&invalid_binding)
                .unwrap_err()
                .contains("unknown modifier")
        );

        let wheel_bindings = DEFAULT_CONFIG
            .replace(
                "focus_left = [\"Super+h\", \"Super+Left\"]",
                "focus_left = [\"Super+h\", \"Super+Left\", \"Super+WheelScrollUp\"]",
            )
            .replace(
                "decrease_column_width = [\"Super+Ctrl+h\", \"Super+Ctrl+Left\"]",
                "decrease_column_width = [\"Ctrl+Alt+WheelScrollDown\"]",
            )
            .replace(
                "increase_column_width = [\"Super+Ctrl+l\", \"Super+Ctrl+Right\"]",
                "increase_column_width = [\"Ctrl+Alt+WheelScrollUp\"]",
            );
        let config = parse(&wheel_bindings).unwrap();
        assert_eq!(
            config.wheel_action(
                &ModifiersState {
                    logo: true,
                    ..Default::default()
                },
                ScrollDirection::Up,
            ),
            Some(Action::FocusLeft)
        );
        assert_eq!(
            config.wheel_action(
                &ModifiersState {
                    ctrl: true,
                    alt: true,
                    ..Default::default()
                },
                ScrollDirection::Down,
            ),
            Some(Action::DecreaseColumnWidth)
        );
        let duplicate_wheel = wheel_bindings.replace(
            "increase_column_width = [\"Ctrl+Alt+WheelScrollUp\"]",
            "increase_column_width = [\"Ctrl+Alt+WheelScrollDown\"]",
        );
        assert!(
            parse(&duplicate_wheel)
                .unwrap_err()
                .contains("more than one action")
        );
        assert!(
            parse(&wheel_bindings.replace("WheelScrollUp", "WheelScrollSideways"))
                .unwrap_err()
                .contains("unknown key")
        );

        let invalid_transform = format!(
            "{DEFAULT_CONFIG}{}",
            MONITORS.replacen("transform = \"normal\"", "transform = \"sideways\"", 1)
        );
        assert!(
            parse(&invalid_transform)
                .unwrap_err()
                .contains("unknown monitor transform")
        );
        for scale in ["0.0", "nan"] {
            let invalid_scale = format!(
                "{DEFAULT_CONFIG}{}",
                MONITORS.replacen("scale = 1.0", &format!("scale = {scale}"), 1)
            );
            assert!(
                parse(&invalid_scale)
                    .unwrap_err()
                    .contains("scale must be positive")
            );
        }

        let duplicate = format!(
            "{DEFAULT_CONFIG}{}",
            MONITORS.replace("connector = \"DP-1\"", "connector = \"DP-2\"")
        );
        assert!(
            parse(&duplicate)
                .unwrap_err()
                .contains("duplicate monitor connector")
        );

        let startup_monitor = format!(
            "{DEFAULT_CONFIG}{}",
            MONITORS.replace("# focus_at_startup = true", "focus_at_startup = true")
        );
        assert_eq!(
            parse(&startup_monitor).unwrap().startup_monitor(),
            Some("DP-2")
        );
        let duplicate_startup = startup_monitor.replace(
            "connector = \"DP-1\"\nresolution",
            "connector = \"DP-1\"\nfocus_at_startup = true\nresolution",
        );
        assert!(
            parse(&duplicate_startup)
                .unwrap_err()
                .contains("only one monitor")
        );

        // A config predating every optional section still parses to the defaults.
        let legacy = without_section(DEFAULT_CONFIG, "[appearance]");
        let legacy = without_section(&legacy, "[animations]");
        let legacy = without_section(&legacy, "[security]");
        let legacy = without_section(&legacy, "[backend]")
            .replace("close_window = [\"Super+q\", \"Super+BackSpace\"]\n", "")
            .replace("toggle_fullscreen = [\"Super+Shift+f\"]\n", "")
            .replace("focus_monitor_left = [\"Super+comma\"]\n", "")
            .replace("quit = [\"Super+Shift+Escape\"]\n", "");
        let legacy = parse(&legacy).unwrap();
        assert_eq!(legacy.bindings.len(), 69);
        // Whole-struct comparison: the built-in defaults and the values written
        // into DEFAULT_CONFIG must not drift apart.
        assert_eq!(legacy.appearance, config.appearance);
        assert_eq!(legacy.animations, config.animations);
        assert_eq!(legacy.input, config.input);
        assert_eq!(legacy.backend, config.backend);
        assert_eq!(legacy.security, config.security);
        assert!(legacy.monitors.is_empty());
        assert_eq!(
            legacy.action(
                &ModifiersState {
                    logo: true,
                    ..Default::default()
                },
                xkb::keysym_from_name("comma", xkb::KEYSYM_CASE_INSENSITIVE),
            ),
            Some(Action::FocusMonitorLeft)
        );

        let pre_workspace_config = r#"
[layout]
column_width = 0.6
center_focused_column = false

[bindings]
focus_left = ["Super+h"]
focus_right = ["Super+l"]
swap_left = ["Super+Shift+h"]
swap_right = ["Super+Shift+l"]
"#;
        let config = parse(pre_workspace_config).unwrap();
        assert!(config.input.middle_click_paste);
        assert_eq!(config.bindings.len(), 56);

        let old_monitor_bindings = DEFAULT_CONFIG
            .replace(
                "decrease_row_height = [\"Super+Ctrl+k\", \"Super+Ctrl+Up\"]\n",
                "",
            )
            .replace(
                "increase_row_height = [\"Super+Ctrl+j\", \"Super+Ctrl+Down\"]\n",
                "",
            )
            .replace(
                "focus_monitor_up = []",
                "focus_monitor_up = [\"Super+Ctrl+Up\"]",
            )
            .replace(
                "focus_monitor_down = []",
                "focus_monitor_down = [\"Super+Ctrl+Down\"]",
            );
        assert!(parse(&old_monitor_bindings).is_ok());

        let invalid_workspaces = DEFAULT_CONFIG.replace(
            "  [\"Super+7\"], [\"Super+8\"], [\"Super+9\"],\n]",
            "  [\"Super+7\"], [\"Super+8\"],\n]",
        );
        assert!(
            parse(&invalid_workspaces)
                .unwrap_err()
                .contains("exactly 9")
        );
    }

    #[test]
    fn vrr_policy_matches_fullscreen_state() {
        assert!(!VrrPolicy::Never.enabled(true));
        assert!(!VrrPolicy::Automatic.enabled(false));
        assert!(VrrPolicy::Automatic.enabled(true));
        assert!(VrrPolicy::Always.enabled(false));

        let with_monitors = format!("{DEFAULT_CONFIG}{MONITORS}");
        let config =
            parse(&with_monitors.replace("vrr = \"automatic\"", "vrr = \"never\"")).unwrap();
        assert_eq!(config.monitors[0].vrr, VrrPolicy::Never);

        let config = parse(&with_monitors.replace("vrr = \"automatic\"\n", "")).unwrap();
        assert!(
            config
                .monitors
                .iter()
                .all(|monitor| monitor.vrr == VrrPolicy::Automatic)
        );
    }

    #[test]
    fn parses_and_validates_autostart() {
        let configured = DEFAULT_CONFIG.replace(
            "commands = [\n  # [\"swaybg\", \"-i\", \"/absolute/path/to/wallpaper.png\", \"-m\", \"fill\"],\n  # [\"waybar\"],\n]",
            "commands = [[\"swaybg\", \"-m\", \"fill\"], [\"waybar\"]]",
        );
        assert_eq!(
            parse(&configured).unwrap().autostart,
            [
                vec!["swaybg".to_string(), "-m".to_string(), "fill".to_string()],
                vec!["waybar".to_string()]
            ]
        );

        let empty = DEFAULT_CONFIG.replace(
            "commands = [\n  # [\"swaybg\", \"-i\", \"/absolute/path/to/wallpaper.png\", \"-m\", \"fill\"],\n  # [\"waybar\"],\n]",
            "commands = [[]]",
        );
        assert!(parse(&empty).unwrap_err().contains("contain an executable"));

        let blank = DEFAULT_CONFIG.replace(
            "commands = [\n  # [\"swaybg\", \"-i\", \"/absolute/path/to/wallpaper.png\", \"-m\", \"fill\"],\n  # [\"waybar\"],\n]",
            "commands = [[\"  \"]]",
        );
        assert!(parse(&blank).unwrap_err().contains("contain an executable"));
    }

    #[test]
    fn parses_and_validates_command_bindings() {
        let configured = DEFAULT_CONFIG.replace(
            "# \"Super+d\" = [\"fuzzel\"]",
            "\"Super+d\" = [\"fuzzel\", \"--show-actions\"]",
        );
        let config = parse(&configured).unwrap();
        assert_eq!(
            config.binding_commands,
            [
                vec!["kitty".to_string(), "--single-instance".to_string()],
                vec!["fuzzel".to_string(), "--show-actions".to_string()]
            ]
        );
        assert_eq!(
            config.action(
                &ModifiersState {
                    logo: true,
                    ..Default::default()
                },
                xkb::keysym_from_name("d", xkb::KEYSYM_CASE_INSENSITIVE),
            ),
            Some(Action::LaunchCommand(1))
        );

        let empty = DEFAULT_CONFIG.replace("# \"Super+d\" = [\"fuzzel\"]", "\"Super+d\" = []");
        assert!(parse(&empty).unwrap_err().contains("contain an executable"));

        let blank =
            DEFAULT_CONFIG.replace("# \"Super+d\" = [\"fuzzel\"]", "\"Super+d\" = [\"  \"]");
        assert!(parse(&blank).unwrap_err().contains("contain an executable"));

        let duplicate =
            DEFAULT_CONFIG.replace("# \"Super+d\" = [\"fuzzel\"]", "\"Super+h\" = [\"fuzzel\"]");
        assert!(
            parse(&duplicate)
                .unwrap_err()
                .contains("more than one action")
        );
    }

    /// A scratch config tree with `config.toml` and a `config.d/` beside it.
    fn config_tree(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("paw2-{label}-{}-{nonce}", std::process::id()));
        fs::create_dir_all(root.join(OVERLAY_DIR)).unwrap();
        fs::write(root.join("config.toml"), DEFAULT_CONFIG).unwrap();
        root
    }

    fn watcher_at(root: &Path) -> ConfigWatcher {
        ConfigWatcher {
            path: root.join("config.toml"),
            sources: Vec::new(),
            stamp: None,
            document: toml::Value::Table(toml::Table::new()),
        }
    }

    #[test]
    fn layout_names_round_trip_through_the_config_spelling() {
        // `name` is what the control socket reports and accepts, so it has to
        // stay the spelling serde reads back - including the underscore.
        for layout in [
            LayoutMode::Scrolling,
            LayoutMode::Dwindle,
            LayoutMode::MasterStack,
            LayoutMode::Monocle,
        ] {
            let quoted = format!("\"{}\"", layout.name());
            assert_eq!(
                serde_json::from_str::<LayoutMode>(&quoted).unwrap(),
                layout,
                "{}",
                layout.name()
            );
        }
    }

    #[test]
    fn merges_tables_and_replaces_arrays() {
        let mut base = toml::from_str::<toml::Value>(
            "[a]\nkept = 1\nreplaced = 2\n[a.deep]\nkept = 3\nreplaced = 4\nlist = [1, 2, 3]\n",
        )
        .unwrap();
        let overlay = toml::from_str::<toml::Value>(
            "[a]\nreplaced = 20\nadded = 30\n[a.deep]\nreplaced = 40\nlist = [9]\n",
        )
        .unwrap();
        merge(&mut base, overlay);

        let a = base.get("a").unwrap();
        assert_eq!(a.get("kept").unwrap().as_integer(), Some(1));
        assert_eq!(a.get("replaced").unwrap().as_integer(), Some(20));
        assert_eq!(a.get("added").unwrap().as_integer(), Some(30));
        let deep = a.get("deep").unwrap();
        assert_eq!(deep.get("kept").unwrap().as_integer(), Some(3));
        assert_eq!(deep.get("replaced").unwrap().as_integer(), Some(40));
        // Arrays are complete statements: the overlay's list wins outright
        // rather than appending to the three entries underneath it.
        assert_eq!(deep.get("list").unwrap().as_array().unwrap().len(), 1);
    }

    #[test]
    fn the_last_drop_in_to_name_a_section_owns_it_whole() {
        let root = config_tree("overlay-order");
        // Neither drop-in carries `layout` or `bindings`, which `RawConfig`
        // requires: this only parses because the compiled defaults are the
        // bottom layer.
        fs::write(
            root.join(OVERLAY_DIR).join("10-first.toml"),
            "[appearance]\ngaps = 40\ncorner_radius = 3\n",
        )
        .unwrap();
        fs::write(
            root.join(OVERLAY_DIR).join("20-second.toml"),
            "[appearance]\ngaps = 41\n",
        )
        .unwrap();

        let config = watcher_at(&root).reload().unwrap().unwrap();
        // Later filename wins the section ...
        assert_eq!(config.appearance.gaps, 41);
        // ... whole: the earlier drop-in's corner_radius goes with it, back to
        // the compiled default rather than lingering under the new owner.
        assert_eq!(config.appearance.corner_radius, 8);
        // A section nobody claimed is still config.toml's.
        assert_eq!(config.layout.column_width, 0.6);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_drop_in_section_replaces_config_toml_rather_than_merging_into_it() {
        let root = config_tree("overlay-owns");
        // config.toml sets both halves of the section, as a hand-written one does.
        fs::write(
            root.join("config.toml"),
            DEFAULT_CONFIG
                .replace("gaps = 15", "gaps = 40")
                .replace("focus_follows_mouse = false", "focus_follows_mouse = true")
                .replace(
                    "center_focused_column = false",
                    "center_focused_column = true",
                ),
        )
        .unwrap();
        // The drop-in claims [layout] while naming only the one key it has a
        // control for. Partial sections are legal because every field defaults.
        fs::write(
            root.join(OVERLAY_DIR).join("shell.toml"),
            "[layout]\ncolumn_width = 0.35\n",
        )
        .unwrap();

        let config = watcher_at(&root).reload().unwrap().unwrap();
        assert_eq!(config.layout.column_width, 0.35);
        // The claimed section comes from the drop-in entirely: config.toml's
        // `center_focused_column` is inert, not merged underneath.
        assert!(!config.layout.center_focused_column);
        // ... and the keys it never named fall back to the compiled defaults.
        assert_eq!(config.layout.enabled_layouts, default_enabled_layouts());
        // Sections the drop-in did not claim are untouched.
        assert_eq!(config.appearance.gaps, 40);
        assert!(config.input.focus_follows_mouse);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn watches_drop_ins_being_added_edited_and_removed() {
        let root = config_tree("overlay-watch");
        let drop_in = root.join(OVERLAY_DIR).join("shell.toml");
        let mut watcher = watcher_at(&root);
        assert_eq!(
            watcher.reload().unwrap().unwrap().appearance.gaps,
            15,
            "the default config alone"
        );
        assert!(watcher.reload().unwrap().is_none());

        // Added. The file count moved even though config.toml did not.
        fs::write(&drop_in, "[appearance]\ngaps = 40\n").unwrap();
        assert_eq!(watcher.reload().unwrap().unwrap().appearance.gaps, 40);

        // Edited.
        fs::write(&drop_in, "[appearance]\ngaps = 41\n").unwrap();
        assert_eq!(watcher.reload().unwrap().unwrap().appearance.gaps, 41);

        // Rewritten with identical bytes: a save that changed nothing must not
        // churn the compositor, even though the mtime moved.
        fs::write(&drop_in, "[appearance]\ngaps = 41\n").unwrap();
        assert!(watcher.reload().unwrap().is_none());

        // Removed - back to what config.toml says.
        fs::remove_file(&drop_in).unwrap();
        assert_eq!(watcher.reload().unwrap().unwrap().appearance.gaps, 15);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_broken_drop_in_names_itself_and_leaves_the_config_alone() {
        let root = config_tree("overlay-broken");
        let drop_in = root.join(OVERLAY_DIR).join("shell.toml");
        let mut watcher = watcher_at(&root);
        watcher.reload().unwrap().unwrap();

        fs::write(&drop_in, "[appearance]\ngaps = \"forty\"\n").unwrap();
        let error = watcher.reload().unwrap_err().to_string();
        assert!(error.contains("shell.toml"), "{error}");

        // The caller keeps the last good config, and a fixed file recovers
        // without needing config.toml to be touched.
        fs::write(&drop_in, "[appearance]\ngaps = 40\n").unwrap();
        assert_eq!(watcher.reload().unwrap().unwrap().appearance.gaps, 40);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_rejected_drop_in_falls_back_to_config_toml_at_startup() {
        // The real failure this exists for: the settings UI wrote a focus ring
        // wider than the user's gaps, and dropping the whole config took
        // `autostart` with it - so the shell that would have shown the error
        // was the one thing that could not start.
        let root = config_tree("overlay-rejected");
        // A fragment, not a whole config: every layer merges over DEFAULT_CONFIG.
        fs::write(
            root.join("config.toml"),
            "[autostart]\ncommands = [[\"qs\", \"-c\", \"paw2-shell\"]]\n",
        )
        .unwrap();
        let drop_in = root.join(OVERLAY_DIR).join("shell.toml");
        fs::write(&drop_in, "[appearance]\nfocus_ring_width = 17\n").unwrap();
        let mut watcher = watcher_at(&root);

        let error = watcher.reload().unwrap_err().to_string();
        assert!(error.contains("focus_ring_width"), "{error}");

        // config.toml alone still parses, autostart included, and that document
        // is what a shell is shown.
        let config = watcher.base_only().unwrap();
        assert_eq!(config.autostart, [["qs", "-c", "paw2-shell"]]);
        assert_eq!(config.appearance.focus_ring_width, 1);
        assert_eq!(
            watcher.document()["appearance"]["focus_ring_width"].as_integer(),
            Some(1)
        );

        // Fixing the drop-in recovers the full merge, with config.toml untouched.
        fs::write(&drop_in, "[appearance]\nfocus_ring_width = 3\n").unwrap();
        let config = watcher.reload().unwrap().unwrap();
        assert_eq!(config.appearance.focus_ring_width, 3);
        assert_eq!(config.autostart, [["qs", "-c", "paw2-shell"]]);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn creates_default_without_overwriting() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("paw2-config-test-{}-{nonce}", std::process::id()));
        let path = root.join("paw2/config.toml");

        create_default(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), DEFAULT_CONFIG);

        fs::write(&path, "custom = true").unwrap();
        create_default(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "custom = true");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reloads_only_changed_valid_config() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("paw2-reload-test-{}-{nonce}", std::process::id()));
        let path = root.join("config.toml");
        fs::create_dir_all(&root).unwrap();
        let with_monitors = format!("{DEFAULT_CONFIG}{MONITORS}");
        fs::write(&path, &with_monitors).unwrap();

        let mut watcher = ConfigWatcher {
            path: path.clone(),
            sources: vec![with_monitors.clone()],
            stamp: None,
            document: toml::Value::Table(toml::Table::new()),
        };
        assert!(watcher.reload().unwrap().is_none());

        // Prove the mtime gate short-circuits rather than re-reading: the cached
        // source is now a lie, and an untouched file must still report no change.
        watcher.sources = vec!["stale".into()];
        assert!(watcher.reload().unwrap().is_none());
        watcher.sources = vec![with_monitors.clone()];

        let changed = with_monitors
            .replace("column_width = 0.6", "column_width = 0.7")
            .replace("resolution = [2560, 1440]", "resolution = [1920, 1080]")
            .replace(
                "# \"Super+d\" = [\"fuzzel\"]",
                "\"Super+d\" = [\"fuzzel\"]",
            )
            .replace(
                "commands = [\n  # [\"swaybg\", \"-i\", \"/absolute/path/to/wallpaper.png\", \"-m\", \"fill\"],\n  # [\"waybar\"],\n]",
                "commands = [[\"waybar\"]]",
            );
        fs::write(&path, changed).unwrap();
        let updated = watcher.reload().unwrap().unwrap();
        let mut active = parse(&with_monitors).unwrap();
        assert!(active.apply_runtime(updated));
        assert_eq!(active.layout.column_width, 0.7);
        assert_eq!(active.monitors[0].resolution, Some([1920, 1080]));
        assert!(active.autostart.is_empty());
        assert!(
            active
                .binding_commands
                .iter()
                .any(|command| command[0] == "fuzzel")
        );

        let updated =
            parse(&with_monitors.replace(PRIVILEGED, r#"privileged_clients = ["waybar"]"#))
                .unwrap();
        assert!(active.apply_runtime(updated));
        assert_eq!(active.security, SecurityConfig::default());

        let updated = parse(
            &with_monitors.replace("middle_click_paste = true", "middle_click_paste = false"),
        )
        .unwrap();
        assert!(active.apply_runtime(updated));
        assert!(active.input.middle_click_paste);

        let updated = parse(
            &with_monitors.replace("focus_follows_mouse = false", "focus_follows_mouse = true"),
        )
        .unwrap();
        assert!(!active.apply_runtime(updated));
        assert!(active.input.focus_follows_mouse);

        let updated = parse(&format!(
            "{with_monitors}\n[theme]\ncursor_theme = \"Breeze\"\n"
        ))
        .unwrap();
        assert!(!active.apply_runtime(updated));
        assert_eq!(active.theme.cursor_theme.as_deref(), Some("Breeze"));

        let updated = parse(&format!(
            "{with_monitors}\n[theme]\nicon_theme = \"Papirus\"\ngtk_theme = \"Breeze\"\n"
        ))
        .unwrap();
        assert!(!active.apply_runtime(updated));
        assert_eq!(active.theme.icon_theme.as_deref(), Some("Papirus"));
        assert_eq!(active.theme.gtk_theme.as_deref(), Some("Breeze"));

        let updated = parse(&format!(
            "{with_monitors}\n[theme]\nqt_style = \"Fusion\"\n"
        ))
        .unwrap();
        assert!(active.apply_runtime(updated));
        assert_eq!(active.theme.qt_style, None);

        let updated = parse(&format!("{with_monitors}\n[theme]\nfont = \"Sans 12\"\n")).unwrap();
        assert!(active.apply_runtime(updated));
        assert_eq!(active.theme.font, None);

        fs::write(&path, "not toml").unwrap();
        assert!(watcher.reload().is_err());
        assert!(watcher.reload().unwrap().is_none());

        fs::write(&path, &with_monitors).unwrap();
        assert!(watcher.reload().unwrap().is_some());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reloads_live_monitor_fields_but_preserves_restart_only_fields() {
        let source = format!("{DEFAULT_CONFIG}{MONITORS}");
        let mut active = parse(&source).unwrap();
        let updated = parse(&format!(
            "{DEFAULT_CONFIG}{}",
            MONITORS
                .replacen("resolution = [2560, 1440]", "resolution = [1920, 1080]", 1)
                .replacen("refresh = 120.0", "refresh = 144.0", 1)
                .replacen("scale = 1.0", "scale = 1.25", 1)
                .replacen("transform = \"normal\"", "transform = \"90\"", 1)
        ))
        .unwrap();

        assert!(!active.apply_runtime(updated));
        assert_eq!(active.monitors[0].resolution, Some([1920, 1080]));
        assert_eq!(active.monitors[0].refresh, Some(144.0));
        assert_eq!(active.monitors[0].scale, 1.25);
        assert_eq!(active.monitors[0].transform, Transform::_90);

        let updated = parse(&format!(
            "{DEFAULT_CONFIG}{}",
            MONITORS.replacen("position = [0, 0]", "position = [100, 0]", 1)
        ))
        .unwrap();
        assert!(active.apply_runtime(updated));
        assert_eq!(active.monitors[0].position, Some([0, 0]));
        assert_eq!(active.monitors[0].scale, 1.0);
        assert_eq!(active.monitors[0].transform, Transform::Normal);

        let updated = parse(&format!(
            "{DEFAULT_CONFIG}{}",
            MONITORS.replacen("vrr = \"automatic\"", "vrr = \"always\"", 1)
        ))
        .unwrap();
        assert!(active.apply_runtime(updated));
        assert_eq!(active.monitors[0].vrr, VrrPolicy::Automatic);
    }

    #[test]
    fn parses_and_validates_scratchpads() {
        let source = format!(
            "{}\n[scratchpads.term]\ncommand = [\"kitty\", \"--class\", \"paw-term\"]\napp_id = \"paw-term\"\nedge = \"bottom\"\nsize = [70, 55]\nposition = [15, 20]\nmargin = 12\n",
            DEFAULT_CONFIG.replace(
                "# \"Super+grave\" = \"terminal\"",
                "\"Super+grave\" = \"term\""
            )
        );
        let config = parse(&source).unwrap();
        assert_eq!(config.scratchpads.len(), 1);
        assert_eq!(config.scratchpads[0].edge, ScratchpadEdge::Bottom);
        assert_eq!(config.scratchpads[0].size, [70.0, 55.0]);
        assert_eq!(config.scratchpads[0].position, Some([15.0, 20.0]));
        assert_eq!(
            config.action(
                &ModifiersState {
                    logo: true,
                    ..Default::default()
                },
                xkb::keysym_from_name("grave", xkb::KEYSYM_CASE_INSENSITIVE),
            ),
            Some(Action::ToggleScratchpad(0))
        );

        let invalid = source.replace("size = [70, 55]", "size = [101, 55]");
        assert!(parse(&invalid).unwrap_err().contains("between 1 and 100"));
    }
}
