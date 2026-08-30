#![allow(irrefutable_let_patterns)]

mod backend;
mod config;
mod cursor;
mod drm;
mod handlers;
mod input;
mod ipc;
mod layout;
mod render;
mod screencopy;
mod state;
mod workspace;
mod xwayland;

use std::{
    ffi::{OsStr, OsString},
    io,
    sync::{Mutex, MutexGuard, OnceLock},
    time::{Duration, Instant},
};

use smithay::reexports::{
    calloop::{EventLoop, LoopSignal, timer::TimeoutAction, timer::Timer},
    rustix::process::{Pid, Signal, kill_process},
    wayland_server::Display,
};
use state::Paw2;

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Direct,
    Nested(Vec<OsString>),
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let clock = Instant::now();
    init_logging();

    let Some(mode) = parse_args(std::env::args_os().skip(1))? else {
        print_usage();
        return Ok(());
    };
    let cursor_defaults = cursor::CursorSettings::environment();
    let (config, mut config_watcher, config_error) = config::load()?;
    phase(&clock, "config loaded");
    // Only the environment half runs here: it costs nothing and has to precede
    // every spawned client. The `gsettings` half is one cold process per key, so
    // it waits until the desktop is on screen.
    apply_theme_environment(&config.theme);
    let mut event_loop: EventLoop<Paw2> = EventLoop::try_new()?;
    let display = Display::new()?;
    let mut state = Paw2::new(&mut event_loop, display, config, cursor_defaults);
    phase(&clock, "compositor state ready");
    state.initialize_scratchpads();
    state
        .ipc
        .publish_config(config_watcher.document(), config_error);
    ipc::init(&event_loop.handle(), &mut state);
    install_signal_handlers(event_loop.get_signal());

    event_loop.handle().insert_source(
        Timer::from_duration(Duration::from_millis(500)),
        move |_, _, state| {
            match config_watcher.reload() {
                Ok(Some(config)) => {
                    let mut errors = drm::apply_monitor_changes(state, &config.monitors);
                    let desktop_theme_changes =
                        changed_desktop_gsettings(&state.config.theme, &config.theme);
                    let cursor_changes =
                        changed_cursor_gsettings(&state.config.theme, &config.theme);
                    let cursor_changed = state.config.theme.cursor_theme
                        != config.theme.cursor_theme
                        || state.config.theme.cursor_size != config.theme.cursor_size;
                    if cursor_changed {
                        match drm::reload_cursor(state, &config.theme) {
                            Ok(()) => {
                                apply_gsettings_updates(cursor_changes);
                                let settings = state.cursor_defaults.configured(
                                    config.theme.cursor_theme.as_deref(),
                                    config.theme.cursor_size,
                                );
                                if state.direct.is_some() {
                                    update_cursor_activation_environment(&settings);
                                }
                            }
                            Err(error) => errors.push(error),
                        }
                    }
                    apply_gsettings_updates(desktop_theme_changes);
                    let restart_required = state.config.apply_runtime(config);
                    state.set_middle_click_paste(state.config.input.middle_click_paste);
                    state.reconcile_scratchpads();
                    state.reconcile_tab_grouping();
                    state.relayout(true);
                    drm::redraw(state);
                    state.ipc.publish_config(
                        config_watcher.document(),
                        (!errors.is_empty()).then(|| errors.join("; ")),
                    );
                    tracing::info!("reloaded configuration");
                    if restart_required {
                        tracing::warn!(
                            "backend, monitor placement, VRR, startup focus, autostart, privileged clients, Qt style, and font changes require restarting Paw2"
                        );
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, "could not reload configuration");
                    // The document is still the config Paw2 is running; a shell
                    // needs both that and the reason its edit did not take.
                    state
                        .ipc
                        .publish_config(config_watcher.document(), Some(error.to_string()));
                }
            }
            TimeoutAction::ToDuration(Duration::from_millis(500))
        },
    )?;

    let started = match &mode {
        Mode::Direct => drm::init(&mut event_loop, &mut state),
        Mode::Nested(_) => backend::init(&mut event_loop, &mut state),
    };
    if let Err(error) = started {
        // Not a full `shutdown`: nothing below has touched the session
        // environment yet, so unsetting it here would clear the display
        // manager's. But `drm::init` installs the backend before its last few
        // fallible steps, and only `DirectBackend::shutdown` resets the device -
        // dropping it does not. Hand the GPU back while libseat is still up.
        if let Some(backend) = state.direct.take() {
            backend.shutdown();
        }
        return Err(error);
    }
    if mode == Mode::Direct {
        state.apply_startup_monitor();
    }
    phase(&clock, "backend ready");

    // SAFETY: Paw2 is single-threaded and sets the child environment before
    // spawning a client or entering the event loop.
    unsafe {
        std::env::set_var("WAYLAND_DISPLAY", &state.socket_name);
        if mode == Mode::Direct {
            std::env::set_var("XDG_SESSION_TYPE", "wayland");
            std::env::set_var("XDG_CURRENT_DESKTOP", "Paw2");
            std::env::set_var("XDG_SESSION_DESKTOP", "Paw2");
        }
    }
    // Never let nested clients escape to the parent X server.
    unsafe { std::env::remove_var("DISPLAY") };

    // The direct backend has already put the first frame on screen, so the rest
    // of the session handshake - four `gsettings`, a keyring daemon, D-Bus and
    // systemd - runs behind the desktop rather than in front of it.
    apply_gsettings(theme_gsettings(&state.config.theme));
    if mode == Mode::Direct {
        initialize_keyring();
    }
    match xwayland::start(event_loop.handle(), &mut state) {
        Ok(display) => unsafe { std::env::set_var("DISPLAY", format!(":{display}")) },
        Err(error) => tracing::warn!(?error, "XWayland unavailable; continuing Wayland-only"),
    }
    if mode == Mode::Direct {
        import_activation_environment();
        update_graphical_session("start");
    }

    let cursor_settings = state.cursor_defaults.configured(
        state.config.theme.cursor_theme.as_deref(),
        state.config.theme.cursor_size,
    );
    for command in &state.config.autostart {
        if let Err(error) = spawn(command, &cursor_settings) {
            tracing::error!(
                ?error,
                program = command[0],
                "failed to launch autostart command"
            );
        }
    }

    if let Mode::Nested(command) = &mode
        && !command.is_empty()
    {
        spawn(command, &cursor_settings)?;
    }

    phase(&clock, "startup complete");
    let result = event_loop.run(None, &mut state, |state| {
        ipc::flush(state);
        let _ = state.display_handle.flush_clients();
    });
    shutdown(state, event_loop, &mode);
    result?;
    Ok(())
}

/// Hand the machine back the way Paw2 found it.
///
/// The order is load-bearing. DRM state can only be restored while the libseat
/// session is still active, and the session is owned by an event source, so it
/// is closed by dropping the event loop - which in turn only happens now that
/// no source callback holds a strong `LoopHandle` back to it.
fn shutdown(mut state: Paw2, event_loop: EventLoop<'static, Paw2>, mode: &Mode) {
    state::start_shutdown();
    if *mode == Mode::Direct {
        update_graphical_session("stop");
    }
    if let Some(backend) = state.direct.take() {
        backend.shutdown();
    }
    // Closes the Wayland socket, which is how well-behaved clients learn to exit.
    drop(state);
    // Kills XWayland, closes the libinput devices, and closes the libseat session.
    drop(event_loop);
    terminate_children();
    if *mode == Mode::Direct {
        unset_activation_environment();
    }
}

/// The variables Paw2 exports to the systemd user manager and the D-Bus
/// activation environment.
///
/// That environment outlives the session, so exactly this list has to be unset
/// again on the way out; otherwise the next compositor starts with a
/// `WAYLAND_DISPLAY` pointing at a dead socket and Paw2's theme overrides.
fn activation_environment() -> Vec<&'static str> {
    let mut variables = vec![
        "WAYLAND_DISPLAY",
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_DESKTOP",
    ];
    if std::env::var_os("PAW2_SOCKET").is_some() {
        variables.push("PAW2_SOCKET");
    }
    for name in [
        "DISPLAY",
        "GNOME_KEYRING_CONTROL",
        "XCURSOR_THEME",
        "XCURSOR_SIZE",
        "GTK_THEME",
        "QT_STYLE_OVERRIDE",
        "QT_QPA_PLATFORMTHEME",
    ] {
        if std::env::var_os(name).is_some() {
            variables.push(name);
        }
    }
    variables
}

fn import_activation_environment() {
    let mut variables = vec!["--systemd"];
    variables.extend(activation_environment());
    let status = std::process::Command::new("dbus-update-activation-environment")
        .args(variables)
        .status();
    match status {
        Ok(status) if status.success() => {}
        Ok(status) => tracing::warn!(%status, "could not import portal activation environment"),
        Err(error) => tracing::warn!(?error, "could not run dbus-update-activation-environment"),
    }
}

fn update_cursor_activation_environment(settings: &cursor::CursorSettings) {
    let variables = settings
        .environment_values()
        .map(|(name, value)| format!("{name}={value}"));
    match std::process::Command::new("dbus-update-activation-environment")
        .arg("--systemd")
        .args(variables)
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => tracing::warn!(%status, "could not update the cursor launch environment"),
        Err(error) => tracing::warn!(?error, "could not update the cursor launch environment"),
    }
}

/// Finish initializing the login daemon that `pam_gnome_keyring` unlocked.
fn initialize_keyring() {
    let output = std::process::Command::new("gnome-keyring-daemon")
        .args(["--start", "--components=secrets"])
        .output();
    let output = match output {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            tracing::warn!(status = %output.status, "could not initialize GNOME Keyring");
            return;
        }
        Err(error) => {
            tracing::warn!(?error, "could not run gnome-keyring-daemon");
            return;
        }
    };

    let Ok(stdout) = std::str::from_utf8(&output.stdout) else {
        tracing::warn!("GNOME Keyring returned an invalid control path");
        return;
    };
    if let Some(control) = keyring_control(stdout) {
        // SAFETY: startup is single-threaded and no clients have been spawned yet.
        unsafe { std::env::set_var("GNOME_KEYRING_CONTROL", control) };
    }
}

fn keyring_control(output: &str) -> Option<&str> {
    output.lines().find_map(|line| {
        line.strip_prefix("GNOME_KEYRING_CONTROL=")
            .filter(|value| !value.is_empty())
    })
}

fn update_graphical_session(action: &str) {
    let mut command = std::process::Command::new("systemctl");
    command.arg("--user");
    // Nothing in Paw2 waits on the target's units, so starting it is fire and
    // forget. Stopping stays blocking: `shutdown` needs those services gone
    // while the compositor is still alive to serve them.
    if action == "start" {
        command.arg("--no-block");
    }
    let status = command.args([action, "paw2-session.target"]).status();
    match status {
        Ok(status) if status.success() => {}
        Ok(status) => tracing::warn!(%status, action, "could not update graphical session target"),
        Err(error) => tracing::warn!(?error, action, "could not run systemctl"),
    }
}

/// `dbus-update-activation-environment` has no unset verb, but the systemd user
/// manager is what a systemd-launched session inherits, so clearing it there is
/// what keeps the next session clean.
fn unset_activation_environment() {
    let mut variables = vec!["--user", "unset-environment"];
    variables.extend(activation_environment());
    for name in ["XCURSOR_THEME", "XCURSOR_SIZE"] {
        if !variables.contains(&name) {
            variables.push(name);
        }
    }
    match std::process::Command::new("systemctl")
        .args(variables)
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => tracing::warn!(%status, "could not clear the session environment"),
        Err(error) => tracing::warn!(?error, "could not run systemctl to clear the environment"),
    }
}

fn apply_theme_environment(theme: &config::ThemeConfig) {
    // SAFETY: startup is single-threaded and no clients have been spawned yet.
    unsafe {
        for (name, value) in theme_environment(theme) {
            std::env::set_var(name, value);
        }
    }
}

fn apply_gsettings(settings: Vec<(&'static str, String)>) {
    apply_gsettings_updates(
        settings
            .into_iter()
            .map(|(key, value)| (key, Some(value)))
            .collect(),
    );
}

/// Every `gsettings` here is a cold process that loads glib and D-Bus-activates
/// `dconf`, and that latency is the same whether one runs or four do. Spawning
/// the whole set before waiting on any of it pays for it once.
fn apply_gsettings_updates(settings: Vec<(&'static str, Option<String>)>) {
    let mut running = Vec::with_capacity(settings.len());
    for (key, value) in settings {
        let mut command = std::process::Command::new("gsettings");
        command.arg(if value.is_some() { "set" } else { "reset" });
        command.args(["org.gnome.desktop.interface", key]);
        if let Some(value) = value {
            command.arg(value);
        }
        match command.spawn() {
            Ok(child) => running.push((key, child)),
            Err(error) => {
                tracing::warn!(
                    ?error,
                    "could not run gsettings; desktop theme was not synchronized"
                );
                break;
            }
        }
    }
    for (key, mut child) in running {
        match child.wait() {
            Ok(status) if status.success() => {}
            Ok(status) => tracing::warn!(%status, key, "could not apply desktop theme setting"),
            Err(error) => tracing::warn!(?error, key, "could not wait for gsettings"),
        }
    }
}

fn theme_environment(theme: &config::ThemeConfig) -> Vec<(&'static str, String)> {
    let mut variables = Vec::new();
    if let Some(value) = &theme.cursor_theme {
        variables.push(("XCURSOR_THEME", value.clone()));
    }
    if let Some(value) = theme.cursor_size {
        variables.push(("XCURSOR_SIZE", value.to_string()));
    }
    if let Some(value) = &theme.qt_style {
        variables.push(("QT_STYLE_OVERRIDE", value.clone()));
    }
    if theme.icon_theme.is_some() || theme.font.is_some() {
        variables.push(("QT_QPA_PLATFORMTHEME", "gtk3".into()));
    }
    variables
}

fn theme_gsettings(theme: &config::ThemeConfig) -> Vec<(&'static str, String)> {
    let mut settings = cursor_gsettings(theme);
    if let Some(value) = &theme.icon_theme {
        settings.push(("icon-theme", value.clone()));
    }
    if let Some(value) = &theme.gtk_theme {
        settings.push(("gtk-theme", value.clone()));
    }
    if let Some(value) = &theme.font {
        settings.push(("font-name", value.clone()));
    }
    settings
}

fn cursor_gsettings(theme: &config::ThemeConfig) -> Vec<(&'static str, String)> {
    let mut settings = Vec::new();
    if let Some(value) = &theme.cursor_theme {
        settings.push(("cursor-theme", value.clone()));
    }
    if let Some(value) = theme.cursor_size {
        settings.push(("cursor-size", value.to_string()));
    }
    settings
}

fn changed_cursor_gsettings(
    current: &config::ThemeConfig,
    updated: &config::ThemeConfig,
) -> Vec<(&'static str, Option<String>)> {
    let mut settings = Vec::new();
    if current.cursor_theme != updated.cursor_theme {
        settings.push(("cursor-theme", updated.cursor_theme.clone()));
    }
    if current.cursor_size != updated.cursor_size {
        settings.push((
            "cursor-size",
            updated.cursor_size.map(|size| size.to_string()),
        ));
    }
    settings
}

fn changed_desktop_gsettings(
    current: &config::ThemeConfig,
    updated: &config::ThemeConfig,
) -> Vec<(&'static str, Option<String>)> {
    let mut settings = Vec::new();
    if current.icon_theme != updated.icon_theme {
        settings.push(("icon-theme", updated.icon_theme.clone()));
    }
    if current.gtk_theme != updated.gtk_theme {
        settings.push(("gtk-theme", updated.gtk_theme.clone()));
    }
    settings
}

/// Live children, so logging out does not leave them behind holding the login
/// session open.
// ponytail: a plain pid list; the reaper clears each entry the moment the child
// is waited for, so the only window for pid reuse is shutdown itself.
static CHILDREN: Mutex<Vec<u32>> = Mutex::new(Vec::new());

fn children() -> MutexGuard<'static, Vec<u32>> {
    CHILDREN.lock().unwrap_or_else(|error| error.into_inner())
}

pub(crate) fn spawn<T: AsRef<OsStr>>(
    command: &[T],
    cursor_settings: &cursor::CursorSettings,
) -> io::Result<()> {
    let Some((program, args)) = command.split_first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "command cannot be empty",
        ));
    };
    let mut child = std::process::Command::new(program)
        .args(args)
        .envs(cursor_settings.environment_values())
        .spawn()?;
    let pid = child.id();
    children().push(pid);
    // ponytail: one waiter per child; replace with session process supervision if launch volume grows.
    std::thread::spawn(move || {
        let _ = child.wait();
        children().retain(|tracked| *tracked != pid);
    });
    Ok(())
}

/// Wayland clients exit on their own once the socket closes; anything else we
/// launched would outlive the session and keep logind from ending it.
fn terminate_children() {
    for pid in children().drain(..) {
        if let Some(pid) = Pid::from_raw(pid as i32) {
            let _ = kill_process(pid, Signal::TERM);
        }
    }
}

/// The event loop's stop switch, so the signal handler can reach it.
static LOOP_SIGNAL: OnceLock<LoopSignal> = OnceLock::new();

extern "C" fn on_terminate(_signal: libc::c_int) {
    state::start_shutdown();
    if let Some(signal) = LOOP_SIGNAL.get() {
        signal.stop();
        signal.wakeup();
    }
}

/// A display manager ends the session with `SIGTERM`. Without a handler the
/// process dies on the spot and none of [`shutdown`] runs, leaving the seat
/// open and the session environment polluted.
fn install_signal_handlers(signal: LoopSignal) {
    let _ = LOOP_SIGNAL.set(signal);
    // SAFETY: the handler only performs relaxed atomic stores and a write to the
    // event loop's eventfd, all of which are async-signal-safe.
    unsafe {
        libc::signal(
            libc::SIGTERM,
            on_terminate as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            on_terminate as *const () as libc::sighandler_t,
        );
    }
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> io::Result<Option<Mode>> {
    let mut args = args.into_iter();
    match args.next().as_deref() {
        None => Ok(Some(Mode::Direct)),
        Some(value) if value == "--nested" => Ok(Some(Mode::Nested(args.collect()))),
        Some(value) if value == "--help" || value == "-h" => {
            if args.next().is_some() {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--help takes no arguments",
                ))
            } else {
                Ok(None)
            }
        }
        Some(value) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown argument `{}`; try --help", value.to_string_lossy()),
        )),
    }
}

fn print_usage() {
    println!("Usage:\n  paw2\n  paw2 --nested [COMMAND [ARG...]]\n  paw2 --help");
}

/// Startup phase timing, so a slow login is legible in the session log instead
/// of reconstructed from the timestamps of unrelated lines.
pub(crate) fn phase(clock: &Instant, name: &str) {
    tracing::info!(
        name,
        elapsed_ms = clock.elapsed().as_millis(),
        "startup phase"
    );
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // stderr, not the default stdout: a display manager keeps the session's
    // stderr (SDDM's `SessionLogFile`) and sends its stdout to /dev/null, so
    // this is the difference between a startup failure being diagnosable and
    // being invisible.
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_terminates_tracked_children() {
        spawn(&["sleep", "10"], &cursor::CursorSettings::environment()).unwrap();
        assert!(!children().is_empty());
        terminate_children();
        assert!(children().is_empty());
    }

    #[test]
    fn reads_only_a_valid_keyring_control_path() {
        assert_eq!(
            keyring_control(
                "SSH_AUTH_SOCK=/tmp/ssh\nGNOME_KEYRING_CONTROL=/run/user/1000/keyring\n"
            ),
            Some("/run/user/1000/keyring")
        );
        assert_eq!(keyring_control("GNOME_KEYRING_CONTROL=\n"), None);
        assert_eq!(keyring_control("UNRELATED=value\n"), None);
    }

    #[test]
    fn parses_cli() {
        assert_eq!(parse_args([]).unwrap(), Some(Mode::Direct));
        assert_eq!(
            parse_args(["--nested".into(), "kitty".into()]).unwrap(),
            Some(Mode::Nested(vec!["kitty".into()]))
        );
        assert_eq!(parse_args(["--help".into()]).unwrap(), None);
        assert!(parse_args(["kitty".into()]).is_err());
    }

    #[test]
    fn maps_theme_to_desktop_interfaces() {
        let theme = config::ThemeConfig {
            cursor_theme: Some("Cursor".into()),
            cursor_size: Some(32),
            icon_theme: Some("Icons".into()),
            gtk_theme: Some("Gtk".into()),
            qt_style: Some("Qt".into()),
            font: Some("Sans 11".into()),
        };
        assert_eq!(
            theme_environment(&theme),
            [
                ("XCURSOR_THEME", "Cursor".into()),
                ("XCURSOR_SIZE", "32".into()),
                ("QT_STYLE_OVERRIDE", "Qt".into()),
                ("QT_QPA_PLATFORMTHEME", "gtk3".into()),
            ]
        );
        assert_eq!(theme_gsettings(&theme).len(), 5);
        assert_eq!(
            changed_cursor_gsettings(&config::ThemeConfig::default(), &theme),
            [
                ("cursor-theme", Some("Cursor".into())),
                ("cursor-size", Some("32".into())),
            ]
        );
        assert_eq!(
            changed_cursor_gsettings(&theme, &config::ThemeConfig::default()),
            [("cursor-theme", None), ("cursor-size", None)]
        );
        assert_eq!(
            changed_desktop_gsettings(&config::ThemeConfig::default(), &theme),
            [
                ("icon-theme", Some("Icons".into())),
                ("gtk-theme", Some("Gtk".into())),
            ]
        );
        assert_eq!(
            changed_desktop_gsettings(&theme, &config::ThemeConfig::default()),
            [("icon-theme", None), ("gtk-theme", None)]
        );
    }
}
