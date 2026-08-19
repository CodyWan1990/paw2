use std::{
    collections::BTreeMap,
    env,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
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
# Smooth backdrop blur radius in logical pixels; zero disables it.
blur_radius = 8

[input]
# Expose Wayland primary selection for middle-click paste.
middle_click_paste = true

[backend]
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

#[derive(Clone, Debug, PartialEq)]
pub struct LayoutConfig {
    pub column_width: f64,
    pub preset_column_widths: Vec<f64>,
    pub center_focused_column: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AppearanceConfig {
    pub gaps: i32,
    pub focus_ring_width: i32,
    pub focus_ring_color: [f32; 4],
    pub blur_radius: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputConfig {
    pub middle_click_paste: bool,
}

#[derive(Debug)]
pub struct Config {
    pub layout: LayoutConfig,
    pub appearance: AppearanceConfig,
    pub input: InputConfig,
    pub backend: BackendConfig,
    pub monitors: Vec<MonitorConfig>,
    pub autostart: Vec<Vec<String>>,
    pub binding_commands: Vec<Vec<String>>,
    bindings: Vec<(Binding, Action)>,
}

#[derive(Debug, Default, PartialEq)]
pub struct BackendConfig {
    pub drm_device: Option<PathBuf>,
    pub connector: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MonitorConfig {
    pub connector: String,
    pub resolution: Option<[u16; 2]>,
    pub refresh: Option<f64>,
    pub position: Option<[i32; 2]>,
    pub transform: Transform,
}

impl Config {
    pub fn action(&self, modifiers: &ModifiersState, keysym: Keysym) -> Option<Action> {
        self.bindings
            .iter()
            .find_map(|(binding, action)| binding.matches(modifiers, keysym).then_some(*action))
    }

    pub fn apply_runtime(&mut self, updated: Self) -> bool {
        let restart_required = self.backend != updated.backend
            || self.monitors != updated.monitors
            || self.autostart != updated.autostart
            || self.input != updated.input;
        self.layout = updated.layout;
        self.appearance = updated.appearance;
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
    keysym: Keysym,
}

impl Binding {
    fn parse(value: &str) -> Result<Self, String> {
        let mut parts = value.split('+').peekable();
        let mut binding = Self {
            ctrl: false,
            alt: false,
            shift: false,
            logo: false,
            keysym: Keysym::NoSymbol,
        };

        while let Some(part) = parts.next() {
            if part.is_empty() {
                return Err(format!("invalid empty component in binding `{value}`"));
            }

            if parts.peek().is_none() {
                binding.keysym = xkb::keysym_from_name(part, xkb::KEYSYM_CASE_INSENSITIVE);
                if binding.keysym.raw() == keysyms::KEY_NoSymbol {
                    return Err(format!("unknown key `{part}` in binding `{value}`"));
                }
                break;
            }

            let modifier = match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => &mut binding.ctrl,
                "alt" => &mut binding.alt,
                "shift" => &mut binding.shift,
                "super" | "logo" => &mut binding.logo,
                _ => return Err(format!("unknown modifier `{part}` in binding `{value}`")),
            };
            if std::mem::replace(modifier, true) {
                return Err(format!("duplicate modifier `{part}` in binding `{value}`"));
            }
        }

        if binding.keysym.raw() == keysyms::KEY_NoSymbol {
            return Err(format!("binding `{value}` has no key"));
        }

        Ok(binding)
    }

    fn matches(&self, modifiers: &ModifiersState, keysym: Keysym) -> bool {
        self.keysym == keysym
            && self.ctrl == modifiers.ctrl
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
    layout: RawLayout,
    #[serde(default)]
    appearance: RawAppearance,
    #[serde(default)]
    input: RawInput,
    #[serde(default)]
    backend: RawBackend,
    #[serde(default)]
    monitors: Vec<RawMonitor>,
    #[serde(default)]
    autostart: RawAutostart,
    bindings: RawBindings,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLayout {
    column_width: f64,
    #[serde(default = "default_preset_column_widths")]
    preset_column_widths: Vec<f64>,
    center_focused_column: bool,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawAppearance {
    gaps: u16,
    focus_ring_width: u16,
    focus_ring_color: String,
    blur_radius: u16,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawInput {
    middle_click_paste: bool,
}

impl Default for RawInput {
    fn default() -> Self {
        Self {
            middle_click_paste: true,
        }
    }
}

impl Default for RawAppearance {
    fn default() -> Self {
        Self {
            gaps: 15,
            focus_ring_width: 1,
            focus_ring_color: "#0b3d91".into(),
            blur_radius: 8,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBindings {
    focus_left: Vec<String>,
    focus_right: Vec<String>,
    #[serde(default = "default_focus_up")]
    focus_up: Vec<String>,
    #[serde(default = "default_focus_down")]
    focus_down: Vec<String>,
    swap_left: Vec<String>,
    swap_right: Vec<String>,
    #[serde(default = "default_swap_up")]
    swap_up: Vec<String>,
    #[serde(default = "default_swap_down")]
    swap_down: Vec<String>,
    #[serde(default = "default_consume_or_expel_left")]
    consume_or_expel_left: Vec<String>,
    #[serde(default = "default_consume_or_expel_right")]
    consume_or_expel_right: Vec<String>,
    #[serde(default = "default_cycle_column_width")]
    cycle_column_width: Vec<String>,
    #[serde(default = "default_close_window")]
    close_window: Vec<String>,
    #[serde(default = "default_toggle_fullscreen")]
    toggle_fullscreen: Vec<String>,
    #[serde(default = "default_focus_monitor_left")]
    focus_monitor_left: Vec<String>,
    #[serde(default = "default_focus_monitor_right")]
    focus_monitor_right: Vec<String>,
    #[serde(default = "default_focus_monitor_up")]
    focus_monitor_up: Vec<String>,
    #[serde(default = "default_focus_monitor_down")]
    focus_monitor_down: Vec<String>,
    #[serde(default = "default_move_to_monitor_left")]
    move_to_monitor_left: Vec<String>,
    #[serde(default = "default_move_to_monitor_right")]
    move_to_monitor_right: Vec<String>,
    #[serde(default = "default_move_to_monitor_up")]
    move_to_monitor_up: Vec<String>,
    #[serde(default = "default_move_to_monitor_down")]
    move_to_monitor_down: Vec<String>,
    #[serde(default = "default_quit")]
    quit: Vec<String>,
    #[serde(default)]
    commands: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    workspace: RawWorkspaceBindings,
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

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBackend {
    drm_device: Option<PathBuf>,
    connector: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMonitor {
    connector: String,
    resolution: Option<[u16; 2]>,
    refresh: Option<f64>,
    position: Option<[i32; 2]>,
    #[serde(default = "default_transform")]
    transform: String,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAutostart {
    #[serde(default)]
    commands: Vec<Vec<String>>,
}

fn default_close_window() -> Vec<String> {
    vec!["Super+q".into(), "Super+BackSpace".into()]
}

fn default_preset_column_widths() -> Vec<f64> {
    vec![0.33333, 0.5, 0.66667]
}

fn default_focus_up() -> Vec<String> {
    vec!["Super+k".into(), "Super+Up".into()]
}

fn default_focus_down() -> Vec<String> {
    vec!["Super+j".into(), "Super+Down".into()]
}

fn default_swap_up() -> Vec<String> {
    vec!["Super+Shift+k".into(), "Super+Shift+Up".into()]
}

fn default_swap_down() -> Vec<String> {
    vec!["Super+Shift+j".into(), "Super+Shift+Down".into()]
}

fn default_consume_or_expel_left() -> Vec<String> {
    vec!["Super+bracketleft".into()]
}

fn default_consume_or_expel_right() -> Vec<String> {
    vec!["Super+bracketright".into()]
}

fn default_cycle_column_width() -> Vec<String> {
    vec!["Super+backslash".into()]
}

fn default_toggle_fullscreen() -> Vec<String> {
    vec!["Super+f".into()]
}

fn default_quit() -> Vec<String> {
    vec!["Super+Shift+Escape".into()]
}

fn default_focus_monitor_left() -> Vec<String> {
    vec!["Super+Ctrl+Left".into()]
}
fn default_focus_monitor_right() -> Vec<String> {
    vec!["Super+Ctrl+Right".into()]
}
fn default_focus_monitor_up() -> Vec<String> {
    vec!["Super+Ctrl+Up".into()]
}
fn default_focus_monitor_down() -> Vec<String> {
    vec!["Super+Ctrl+Down".into()]
}
fn default_move_to_monitor_left() -> Vec<String> {
    vec!["Super+Ctrl+Shift+Left".into()]
}
fn default_move_to_monitor_right() -> Vec<String> {
    vec!["Super+Ctrl+Shift+Right".into()]
}
fn default_move_to_monitor_up() -> Vec<String> {
    vec!["Super+Ctrl+Shift+Up".into()]
}
fn default_move_to_monitor_down() -> Vec<String> {
    vec!["Super+Ctrl+Shift+Down".into()]
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
            resolution: monitor.resolution,
            refresh: monitor.refresh,
            position: monitor.position,
            transform: parse_transform(&monitor.transform)?,
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
        layout: LayoutConfig {
            column_width: raw.layout.column_width,
            preset_column_widths: raw.layout.preset_column_widths,
            center_focused_column: raw.layout.center_focused_column,
        },
        appearance: AppearanceConfig {
            gaps: raw.appearance.gaps.into(),
            focus_ring_width: raw.appearance.focus_ring_width.into(),
            focus_ring_color,
            blur_radius: raw.appearance.blur_radius.into(),
        },
        input: InputConfig {
            middle_click_paste: raw.input.middle_click_paste,
        },
        backend: BackendConfig {
            drm_device: raw.backend.drm_device,
            connector: raw.backend.connector,
        },
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

    #[test]
    fn defaults_and_validation() {
        let config = parse(DEFAULT_CONFIG).unwrap();
        assert_eq!(config.bindings.len(), 54);
        assert!(config.input.middle_click_paste);
        assert_eq!(config.layout.column_width, 0.6);
        assert_eq!(config.layout.preset_column_widths, [0.33333, 0.5, 0.66667]);
        assert_eq!(config.appearance.gaps, 15);
        assert_eq!(config.appearance.focus_ring_width, 1);
        assert_eq!(
            config.appearance.focus_ring_color,
            [11.0 / 255.0, 61.0 / 255.0, 145.0 / 255.0, 1.0]
        );
        assert_eq!(config.appearance.blur_radius, 8);
        assert_eq!(config.monitors.len(), 2);
        assert_eq!(config.monitors[0].connector, "DP-2");
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

        let legacy = DEFAULT_CONFIG
            .replace("[appearance]\n# Logical pixels between windows and output edges.\ngaps = 15\nfocus_ring_width = 1\nfocus_ring_color = \"#0b3d91\"\n# Smooth backdrop blur radius in logical pixels; zero disables it.\nblur_radius = 8\n\n", "")
            .replace("[[monitors]]\nconnector = \"DP-2\"\nresolution = [2560, 1440]\nrefresh = 120.0\nposition = [0, 0]\ntransform = \"normal\"\n\n[[monitors]]\nconnector = \"DP-1\"\nresolution = [2560, 1440]\nrefresh = 199.999\nposition = [2560, 0]\ntransform = \"normal\"\n\n", "")
            .replace("[backend]\n# drm_device = \"/dev/dri/card1\"\n# connector = \"DP-1\"\n\n", "")
            .replace("close_window = [\"Super+q\", \"Super+BackSpace\"]\n", "")
            .replace("toggle_fullscreen = [\"Super+f\"]\n", "")
            .replace("quit = [\"Super+Shift+Escape\"]\n", "");
        let config = parse(&legacy).unwrap();
        assert_eq!(config.bindings.len(), 54);
        assert_eq!(config.appearance.gaps, 15);
        assert!(config.monitors.is_empty());

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

        fs::write(&path, "not toml").unwrap();
        assert!(watcher.reload().is_err());
        assert!(watcher.reload().unwrap().is_none());

        fs::write(&path, DEFAULT_CONFIG).unwrap();
        assert!(watcher.reload().unwrap().is_some());
        fs::remove_dir_all(root).unwrap();
    }
}
