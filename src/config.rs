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

const DEFAULT_CONFIG: &str = r##"# Paw2 configuration

[layout]
# Default fraction of the output width used by each column.
column_width = 0.6
# Widths cycled on the focused column.
preset_column_widths = [0.33333, 0.5, 0.66667]
# When false, the viewport moves only enough to reveal the focused column.
center_focused_column = false

[appearance]
# Logical pixels between windows and output edges.
gaps = 15
focus_ring_width = 1
focus_ring_color = "#0b3d91"
# Rounded window corners in logical pixels; zero disables them.
corner_radius = 8
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

[input]
# Focus windows as the pointer enters them without moving the workspace viewport.
focus_follows_mouse = false
# Expose Wayland primary selection for middle-click paste.
middle_click_paste = true

# Desktop theme values are optional. When set, Paw2 applies them at startup.
#[theme]
#cursor_theme = "Oxygen_Zion"
#cursor_size = 24
#icon_theme = "Papirus-Dark"
#gtk_theme = "Breeze"
#qt_style = "Breeze"
#font = "Monofur Nerd Font Propo 12"

[backend]
# Automatically scan out compatible fullscreen buffers on the DRM primary plane.
direct_scanout = true
# drm_device = "/dev/dri/card1"
# connector = "DP-1"

[[monitors]]
connector = "DP-2"
# focus_at_startup = true
resolution = [2560, 1440]
refresh = 120.0
position = [0, 0]
transform = "normal"
vrr = "automatic"

[[monitors]]
connector = "DP-1"
resolution = [2560, 1440]
refresh = 199.999
position = [2560, 0]
transform = "normal"
vrr = "automatic"

[autostart]
# Commands are argv arrays and run once when Paw2 starts.
commands = [
  # ["swaybg", "-i", "/absolute/path/to/wallpaper.png", "-m", "fill"],
  # ["waybar"],
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
toggle_fullscreen = ["Super+Shift+f"]
focus_monitor_left = ["Super+comma"]
focus_monitor_right = ["Super+period"]
focus_monitor_up = ["Super+Ctrl+Up"]
focus_monitor_down = ["Super+Ctrl+Down"]
move_to_monitor_left = ["Super+Shift+comma"]
move_to_monitor_right = ["Super+Shift+period"]
move_to_monitor_up = ["Super+Ctrl+Shift+Up"]
move_to_monitor_down = ["Super+Ctrl+Shift+Down"]
quit = ["Super+Shift+Escape"]

[bindings.commands]
"Super+Return" = ["kitty", "--single-instance"]
# "Super+d" = ["fuzzel"]
# "Super+b" = ["firefox", "--new-window"]

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
    CycleColumnWidth,
    DecreaseColumnWidth,
    IncreaseColumnWidth,
    CloseWindow,
    ToggleFullscreen,
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

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LayoutConfig {
    pub column_width: f64,
    #[serde(default = "default_preset_column_widths")]
    pub preset_column_widths: Vec<f64>,
    pub center_focused_column: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AppearanceConfig {
    pub gaps: i32,
    pub focus_ring_width: i32,
    pub focus_ring_color: [f32; 4],
    pub corner_radius: i32,
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

#[derive(Debug)]
pub struct Config {
    pub layout: LayoutConfig,
    pub appearance: AppearanceConfig,
    pub animations: AnimationConfig,
    pub input: InputConfig,
    pub theme: ThemeConfig,
    pub backend: BackendConfig,
    pub monitors: Vec<MonitorConfig>,
    pub autostart: Vec<Vec<String>>,
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
        self.bindings
            .iter()
            .find_map(|(binding, action)| binding.matches_key(modifiers, keysym).then_some(*action))
    }

    pub fn wheel_action(
        &self,
        modifiers: &ModifiersState,
        direction: ScrollDirection,
    ) -> Option<Action> {
        self.bindings.iter().find_map(|(binding, action)| {
            binding
                .matches_wheel(modifiers, direction)
                .then_some(*action)
        })
    }

    pub fn apply_runtime(&mut self, updated: Self) -> bool {
        let restart_required = self.backend != updated.backend
            || self.monitors != updated.monitors
            || self.autostart != updated.autostart
            || self.input.middle_click_paste != updated.input.middle_click_paste
            || self.theme != updated.theme;
        self.layout = updated.layout;
        self.appearance = updated.appearance;
        self.animations = updated.animations;
        self.input.focus_follows_mouse = updated.input.focus_follows_mouse;
        self.binding_commands = updated.binding_commands;
        self.bindings = updated.bindings;
        restart_required
    }
}

pub struct ConfigWatcher {
    path: PathBuf,
    source: String,
}

impl ConfigWatcher {
    pub fn reload(&mut self) -> io::Result<Option<Config>> {
        let source = fs::read_to_string(&self.path)
            .map_err(|error| invalid_data(&self.path, format!("could not read config: {error}")))?;
        if source == self.source {
            return Ok(None);
        }

        self.source = source;
        parse(&self.source)
            .map(Some)
            .map_err(|error| invalid_data(&self.path, error))
    }
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
}

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

    fn matches_key(&self, modifiers: &ModifiersState, keysym: Keysym) -> bool {
        self.trigger == BindingTrigger::Key(keysym) && self.matches_modifiers(modifiers)
    }

    fn matches_wheel(&self, modifiers: &ModifiersState, direction: ScrollDirection) -> bool {
        self.trigger == BindingTrigger::Wheel(direction) && self.matches_modifiers(modifiers)
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
    monitors: Vec<RawMonitor>,
    #[serde(default)]
    autostart: RawAutostart,
    #[serde(default)]
    animations: RawAnimations,
    bindings: RawBindings,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawAppearance {
    gaps: u16,
    focus_ring_width: u16,
    focus_ring_color: String,
    corner_radius: u16,
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
struct RawBindings {
    focus_left: Vec<String>,
    focus_right: Vec<String>,
    focus_up: Vec<String>,
    focus_down: Vec<String>,
    swap_left: Vec<String>,
    swap_right: Vec<String>,
    swap_up: Vec<String>,
    swap_down: Vec<String>,
    consume_or_expel_left: Vec<String>,
    consume_or_expel_right: Vec<String>,
    cycle_column_width: Vec<String>,
    decrease_column_width: Vec<String>,
    increase_column_width: Vec<String>,
    close_window: Vec<String>,
    toggle_fullscreen: Vec<String>,
    focus_monitor_left: Vec<String>,
    focus_monitor_right: Vec<String>,
    focus_monitor_up: Vec<String>,
    focus_monitor_down: Vec<String>,
    move_to_monitor_left: Vec<String>,
    move_to_monitor_right: Vec<String>,
    move_to_monitor_up: Vec<String>,
    move_to_monitor_down: Vec<String>,
    quit: Vec<String>,
    commands: BTreeMap<String, Vec<String>>,
    workspace: RawWorkspaceBindings,
}

impl Default for RawBindings {
    fn default() -> Self {
        Self {
            focus_left: keys(&["Super+h", "Super+Left"]),
            focus_right: keys(&["Super+l", "Super+Right"]),
            focus_up: keys(&["Super+k", "Super+Up"]),
            focus_down: keys(&["Super+j", "Super+Down"]),
            swap_left: keys(&["Super+Shift+h", "Super+Shift+Left"]),
            swap_right: keys(&["Super+Shift+l", "Super+Shift+Right"]),
            swap_up: keys(&["Super+Shift+k", "Super+Shift+Up"]),
            swap_down: keys(&["Super+Shift+j", "Super+Shift+Down"]),
            consume_or_expel_left: keys(&["Super+bracketleft"]),
            consume_or_expel_right: keys(&["Super+bracketright"]),
            cycle_column_width: keys(&["Super+backslash"]),
            decrease_column_width: Vec::new(),
            increase_column_width: Vec::new(),
            close_window: keys(&["Super+q", "Super+BackSpace"]),
            toggle_fullscreen: keys(&["Super+Shift+f"]),
            focus_monitor_left: keys(&["Super+comma"]),
            focus_monitor_right: keys(&["Super+period"]),
            focus_monitor_up: keys(&["Super+Ctrl+Up"]),
            focus_monitor_down: keys(&["Super+Ctrl+Down"]),
            move_to_monitor_left: keys(&["Super+Shift+comma"]),
            move_to_monitor_right: keys(&["Super+Shift+period"]),
            move_to_monitor_up: keys(&["Super+Ctrl+Shift+Up"]),
            move_to_monitor_down: keys(&["Super+Ctrl+Shift+Down"]),
            quit: keys(&["Super+Shift+Escape"]),
            commands: BTreeMap::new(),
            workspace: RawWorkspaceBindings::default(),
        }
    }
}

fn keys(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).into()).collect()
}

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

fn default_transform() -> String {
    "normal".into()
}

pub fn load() -> io::Result<(Config, ConfigWatcher)> {
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
    let source = fs::read_to_string(&path)
        .map_err(|error| invalid_data(&path, format!("could not read config: {error}")))?;
    let config = parse(&source).map_err(|error| invalid_data(&path, error))?;
    Ok((config, ConfigWatcher { path, source }))
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
    if path.exists() {
        return Ok(());
    }

    fs::create_dir_all(path.parent().unwrap())?;
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => file.write_all(DEFAULT_CONFIG.as_bytes()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

fn parse(source: &str) -> Result<Config, String> {
    let raw: RawConfig = toml::from_str(source).map_err(|error| error.to_string())?;
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
    let focus_ring_color = parse_color(&raw.appearance.focus_ring_color)?;
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
        monitors.push(MonitorConfig {
            connector: monitor.connector,
            focus_at_startup: monitor.focus_at_startup,
            resolution: monitor.resolution,
            refresh: monitor.refresh,
            position: monitor.position,
            transform: parse_transform(&monitor.transform)?,
            vrr: monitor.vrr,
        });
    }

    let mut bindings = Vec::new();
    let workspace = raw.bindings.workspace;
    let command_bindings = raw.bindings.commands;
    if workspace.switch.len() != 9 || workspace.r#move.len() != 9 {
        return Err("bindings.workspace switch and move must each contain exactly 9 lists".into());
    }
    let mut configured = vec![
        (raw.bindings.focus_left, Action::FocusLeft),
        (raw.bindings.focus_right, Action::FocusRight),
        (raw.bindings.focus_up, Action::FocusUp),
        (raw.bindings.focus_down, Action::FocusDown),
        (raw.bindings.swap_left, Action::SwapLeft),
        (raw.bindings.swap_right, Action::SwapRight),
        (raw.bindings.swap_up, Action::SwapUp),
        (raw.bindings.swap_down, Action::SwapDown),
        (
            raw.bindings.consume_or_expel_left,
            Action::ConsumeOrExpelLeft,
        ),
        (
            raw.bindings.consume_or_expel_right,
            Action::ConsumeOrExpelRight,
        ),
        (raw.bindings.cycle_column_width, Action::CycleColumnWidth),
        (
            raw.bindings.decrease_column_width,
            Action::DecreaseColumnWidth,
        ),
        (
            raw.bindings.increase_column_width,
            Action::IncreaseColumnWidth,
        ),
        (raw.bindings.close_window, Action::CloseWindow),
        (raw.bindings.toggle_fullscreen, Action::ToggleFullscreen),
        (raw.bindings.focus_monitor_left, Action::FocusMonitorLeft),
        (raw.bindings.focus_monitor_right, Action::FocusMonitorRight),
        (raw.bindings.focus_monitor_up, Action::FocusMonitorUp),
        (raw.bindings.focus_monitor_down, Action::FocusMonitorDown),
        (raw.bindings.move_to_monitor_left, Action::MoveToMonitorLeft),
        (
            raw.bindings.move_to_monitor_right,
            Action::MoveToMonitorRight,
        ),
        (raw.bindings.move_to_monitor_up, Action::MoveToMonitorUp),
        (raw.bindings.move_to_monitor_down, Action::MoveToMonitorDown),
        (raw.bindings.quit, Action::Quit),
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

    Ok(Config {
        layout: raw.layout,
        appearance: AppearanceConfig {
            gaps: raw.appearance.gaps.into(),
            focus_ring_width: raw.appearance.focus_ring_width.into(),
            focus_ring_color,
            corner_radius: raw.appearance.corner_radius.into(),
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
        input: raw.input,
        theme: raw.theme,
        backend: raw.backend,
        monitors,
        autostart: raw.autostart.commands,
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

fn parse_color(value: &str) -> Result<[f32; 4], String> {
    let hex = value
        .strip_prefix('#')
        .filter(|hex| hex.len() == 6)
        .ok_or_else(|| "appearance.focus_ring_color must use #RRGGBB".to_string())?;
    let color = u32::from_str_radix(hex, 16)
        .map_err(|_| "appearance.focus_ring_color must use #RRGGBB".to_string())?;
    Ok([
        ((color >> 16) & 0xff) as f32 / 255.0,
        ((color >> 8) & 0xff) as f32 / 255.0,
        (color & 0xff) as f32 / 255.0,
        1.0,
    ])
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
        assert_eq!(config.bindings.len(), 54);
        assert!(!config.input.focus_follows_mouse);
        assert!(config.input.middle_click_paste);
        assert_eq!(config.theme, ThemeConfig::default());
        assert_eq!(config.layout.column_width, 0.6);
        assert_eq!(config.layout.preset_column_widths, [0.33333, 0.5, 0.66667]);
        assert_eq!(config.appearance.gaps, 15);
        assert_eq!(config.appearance.focus_ring_width, 1);
        assert_eq!(
            config.appearance.focus_ring_color,
            [11.0 / 255.0, 61.0 / 255.0, 145.0 / 255.0, 1.0]
        );
        assert_eq!(config.appearance.blur_radius, 8);
        assert_eq!(config.appearance.corner_radius, 8);
        assert!(config.appearance.blur_enabled());
        assert_eq!(config.appearance.window_opacity(), 0.95);
        assert!(config.animations.active());
        assert!(config.backend.direct_scanout);
        assert_eq!(config.animations.duration, Duration::from_millis(150));
        assert_eq!(config.monitors.len(), 2);
        assert_eq!(config.monitors[0].connector, "DP-2");
        assert!(!config.monitors[0].focus_at_startup);
        assert_eq!(config.startup_monitor(), None);
        assert_eq!(config.monitors[0].vrr, VrrPolicy::Automatic);
        assert_eq!(config.monitors[1].transform, Transform::Normal);
        assert!(config.autostart.is_empty());
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
                "cycle_column_width = [\"Super+backslash\"]",
                "cycle_column_width = [\"Super+backslash\"]\ndecrease_column_width = [\"Ctrl+Alt+WheelScrollDown\"]\nincrease_column_width = [\"Ctrl+Alt+WheelScrollUp\"]",
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

        let invalid_transform =
            DEFAULT_CONFIG.replacen("transform = \"normal\"", "transform = \"sideways\"", 1);
        assert!(
            parse(&invalid_transform)
                .unwrap_err()
                .contains("unknown monitor transform")
        );

        let duplicate = DEFAULT_CONFIG.replace("connector = \"DP-1\"", "connector = \"DP-2\"");
        assert!(
            parse(&duplicate)
                .unwrap_err()
                .contains("duplicate monitor connector")
        );

        let startup_monitor =
            DEFAULT_CONFIG.replace("# focus_at_startup = true", "focus_at_startup = true");
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
        let legacy = without_section(&legacy, "[backend]");
        let legacy = without_section(&legacy, "[[monitors]]");
        let legacy = without_section(&legacy, "[[monitors]]")
            .replace("close_window = [\"Super+q\", \"Super+BackSpace\"]\n", "")
            .replace("toggle_fullscreen = [\"Super+Shift+f\"]\n", "")
            .replace("focus_monitor_left = [\"Super+comma\"]\n", "")
            .replace("quit = [\"Super+Shift+Escape\"]\n", "");
        let config = parse(&legacy).unwrap();
        assert_eq!(config.bindings.len(), 54);
        assert_eq!(config.appearance.gaps, 15);
        assert_eq!(config.appearance.corner_radius, 8);
        assert!(config.appearance.blur);
        assert!(config.animations.enabled);
        assert!(config.backend.direct_scanout);
        assert_eq!(config.animations.duration, Duration::from_millis(150));
        assert!(config.monitors.is_empty());
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
        assert_eq!(config.bindings.len(), 49);

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

        let config =
            parse(&DEFAULT_CONFIG.replace("vrr = \"automatic\"", "vrr = \"never\"")).unwrap();
        assert_eq!(config.monitors[0].vrr, VrrPolicy::Never);

        let config = parse(&DEFAULT_CONFIG.replace("vrr = \"automatic\"\n", "")).unwrap();
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
        fs::write(&path, DEFAULT_CONFIG).unwrap();

        let mut watcher = ConfigWatcher {
            path: path.clone(),
            source: DEFAULT_CONFIG.into(),
        };
        assert!(watcher.reload().unwrap().is_none());

        let changed = DEFAULT_CONFIG
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
        let mut active = parse(DEFAULT_CONFIG).unwrap();
        assert!(active.apply_runtime(updated));
        assert_eq!(active.layout.column_width, 0.7);
        assert_eq!(active.monitors[0].resolution, Some([2560, 1440]));
        assert!(active.autostart.is_empty());
        assert!(
            active
                .binding_commands
                .iter()
                .any(|command| command[0] == "fuzzel")
        );

        let updated = parse(
            &DEFAULT_CONFIG.replace("middle_click_paste = true", "middle_click_paste = false"),
        )
        .unwrap();
        assert!(active.apply_runtime(updated));
        assert!(active.input.middle_click_paste);

        let updated = parse(
            &DEFAULT_CONFIG.replace("focus_follows_mouse = false", "focus_follows_mouse = true"),
        )
        .unwrap();
        assert!(!active.apply_runtime(updated));
        assert!(active.input.focus_follows_mouse);

        let updated = parse(&format!(
            "{DEFAULT_CONFIG}\n[theme]\ncursor_theme = \"Breeze\"\n"
        ))
        .unwrap();
        assert!(active.apply_runtime(updated));
        assert_eq!(active.theme, ThemeConfig::default());

        fs::write(&path, "not toml").unwrap();
        assert!(watcher.reload().is_err());
        assert!(watcher.reload().unwrap().is_none());

        fs::write(&path, DEFAULT_CONFIG).unwrap();
        assert!(watcher.reload().unwrap().is_some());
        fs::remove_dir_all(root).unwrap();
    }
}
