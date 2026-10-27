#![allow(irrefutable_let_patterns)]

mod backend;
mod clipboard;
mod config;
mod cursor;
mod decoration;
mod drm;
mod gamma;
mod handlers;
mod input;
mod ipc;
mod layout;
mod render;
mod screencopy;
mod session;
mod state;
mod workspace;
mod xwayland;

use std::{
    cell::{Cell, RefCell},
    ffi::{OsStr, OsString},
    io,
    rc::Rc,
    sync::{Mutex, MutexGuard, OnceLock},
    time::{Duration, Instant},
};

use smithay::reexports::{
    calloop::{
        self, EventLoop, Interest, LoopHandle, LoopSignal, PostAction, generic::Generic,
        timer::TimeoutAction, timer::Timer,
    },
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
    restrict_process_memory();

    let Some(mode) = parse_args(std::env::args_os().skip(1))? else {
        print_usage();
        return Ok(());
    };
    let cursor_defaults = cursor::CursorSettings::environment();
    let (config, config_watcher, config_error) = config::load()?;
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

    let config_watcher = Rc::new(RefCell::new(config_watcher));
    watch_config(&event_loop.handle(), &config_watcher);
    // The fallback for what inotify cannot see - a config.toml symlinked
    // elsewhere, a network home - and the reaper for exited children.
    let fallback_watcher = config_watcher.clone();
    event_loop.handle().insert_source(
        Timer::from_duration(CONFIG_FALLBACK_POLL),
        move |_, _, state| {
            reload_config(state, &mut fallback_watcher.borrow_mut());
            reap_children();
            TimeoutAction::ToDuration(CONFIG_FALLBACK_POLL)
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
        state.end_activity_iteration();
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

/// How often the config is re-read even without an inotify event.
const CONFIG_FALLBACK_POLL: Duration = Duration::from_secs(5);
/// How long after the last inotify event the config is re-read, so a tool
/// writing two files in a row is read once, after both.
const CONFIG_SETTLE: Duration = Duration::from_millis(100);

/// Re-read the config shortly after any file in its directories changes.
///
/// Logs and carries on without it: the fallback poll still catches every edit,
/// only later.
fn watch_config(
    handle: &LoopHandle<'static, Paw2>,
    config_watcher: &Rc<RefCell<config::ConfigWatcher>>,
) {
    use smithay::reexports::rustix::fs::inotify;
    let fd = match inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK) {
        Ok(fd) => fd,
        Err(error) => {
            tracing::warn!(?error, "could not watch the config; polling it instead");
            return;
        }
    };
    config_watcher.borrow().watch(&fd);
    let config_watcher = config_watcher.clone();
    let settle_pending = Rc::new(Cell::new(false));
    let registered = handle.insert_source(
        Generic::new(fd, Interest::READ, calloop::Mode::Level),
        move |_, fd, state| {
            // Drain the queue; which file changed does not matter, every layer
            // is re-read as a set.
            let mut buffer = [0u8; 4096];
            while let Ok(read) = smithay::reexports::rustix::io::read(&*fd, &mut buffer)
                && read > 0
            {}
            config_watcher.borrow().watch(&*fd);
            if !settle_pending.replace(true) {
                let config_watcher = config_watcher.clone();
                let pending = settle_pending.clone();
                let scheduled = state.loop_handle.insert_source(
                    Timer::from_duration(CONFIG_SETTLE),
                    move |_, _, state| {
                        pending.set(false);
                        reload_config(state, &mut config_watcher.borrow_mut());
                        TimeoutAction::Drop
                    },
                );
                if let Err(error) = scheduled {
                    settle_pending.set(false);
                    tracing::warn!(?error, "could not schedule a config reload");
                }
            }
            Ok(PostAction::Continue)
        },
    );
    if let Err(error) = registered {
        tracing::warn!(?error, "could not watch the config; polling it instead");
    }
}

fn reload_config(state: &mut Paw2, config_watcher: &mut config::ConfigWatcher) {
    match config_watcher.reload() {
        Ok(Some(config)) => {
            let mut errors = drm::apply_monitor_changes(state, &config.monitors);
            let desktop_theme_changes =
                changed_desktop_gsettings(&state.config.theme, &config.theme);
            let cursor_changes = changed_cursor_gsettings(&state.config.theme, &config.theme);
            let cursor_changed = state.config.theme.cursor_theme != config.theme.cursor_theme
                || state.config.theme.cursor_size != config.theme.cursor_size;
            if cursor_changed {
                match drm::reload_cursor(state, &config.theme) {
                    Ok(()) => {
                        apply_gsettings_updates_in_background(cursor_changes);
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
            apply_gsettings_updates_in_background(desktop_theme_changes);
            let restart_required = state.config.apply_runtime(config);
            state.set_middle_click_paste(state.config.input.middle_click_paste);
            state.reconcile_scratchpads();
            state.reconcile_tab_grouping();
            state.reconcile_decorations();
            state.relayout(true);
            drm::redraw(state);
            state.ipc.publish_config(
                config_watcher.document(),
                (!errors.is_empty()).then(|| errors.join("; ")),
            );
            tracing::info!("reloaded configuration");
            if restart_required {
                tracing::warn!(
                    "backend, monitor order, VRR, startup focus, autostart, clipboard clients, Qt style, and font changes require restarting Paw2"
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
}

/// Run a session-integration helper to completion, logging rather than
/// failing. None of these is worth the session: a desktop without a portal
/// environment is degraded, a desktop that refused to start is not.
fn run(command: &mut std::process::Command, step: &str) {
    match command.status() {
        Ok(status) if status.success() => {}
        Ok(status) => tracing::warn!(%status, step, "session step failed"),
        Err(error) => tracing::warn!(?error, step, "could not run session step"),
    }
}

/// Push variables into the D-Bus and systemd activation environment, so
/// anything activated later - a portal, a user service - sees them.
fn update_activation_environment<I, S>(variables: I, step: &str)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run(
        std::process::Command::new("dbus-update-activation-environment")
            .arg("--systemd")
            .args(variables),
        step,
    );
}

fn import_activation_environment() {
    update_activation_environment(
        activation_environment(),
        "import the portal activation environment",
    );
}

/// Runs on a config reload, so it is not waited for; the fallback poll reaps it.
fn update_cursor_activation_environment(settings: &cursor::CursorSettings) {
    let spawned = std::process::Command::new("dbus-update-activation-environment")
        .arg("--systemd")
        .args(
            settings
                .environment_values()
                .map(|(name, value)| format!("{name}={value}")),
        )
        .spawn();
    match spawned {
        Ok(child) => children().push(child),
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
    run(
        command.args([action, "paw2-session.target"]),
        "update the graphical session target",
    );
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
    run(
        std::process::Command::new("systemctl").args(variables),
        "clear the session environment",
    );
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
///
/// Waits, for startup: clients launched next must read the new values.
fn apply_gsettings_updates(settings: Vec<(&'static str, Option<String>)>) {
    for (key, mut child) in spawn_gsettings(settings) {
        match child.wait() {
            Ok(status) if status.success() => {}
            Ok(status) => tracing::warn!(%status, key, "could not apply desktop theme setting"),
            Err(error) => tracing::warn!(?error, key, "could not wait for gsettings"),
        }
    }
}

/// [`apply_gsettings_updates`] without the wait, for a config reload: a second
/// of `gsettings` start-up there would be a second of frozen desktop. The
/// fallback poll reaps them.
fn apply_gsettings_updates_in_background(settings: Vec<(&'static str, Option<String>)>) {
    children().extend(
        spawn_gsettings(settings)
            .into_iter()
            .map(|(_, child)| child),
    );
}

fn spawn_gsettings(
    settings: Vec<(&'static str, Option<String>)>,
) -> Vec<(&'static str, std::process::Child)> {
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
    running
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
    if let Some(value) = &theme.qt_platform_theme {
        variables.push(("QT_QPA_PLATFORMTHEME", value.clone()));
    } else if theme.icon_theme.is_some() || theme.font.is_some() {
        variables.push(("QT_QPA_PLATFORMTHEME", "gtk3".into()));
    }
    variables
}

/// An `org.gnome.desktop.interface` key and where its value comes from.
type ThemeKey = (&'static str, fn(&config::ThemeConfig) -> Option<String>);

/// Cursor keys are their own list because a live change to one also has to
/// reload the compositor's own cursor and re-export `XCURSOR_*` to the
/// activation environment - see the reload timer in `main`.
const CURSOR_KEYS: &[ThemeKey] = &[
    ("cursor-theme", |theme| theme.cursor_theme.clone()),
    ("cursor-size", |theme| {
        theme.cursor_size.map(|size| size.to_string())
    }),
];

const DESKTOP_KEYS: &[ThemeKey] = &[
    ("icon-theme", |theme| theme.icon_theme.clone()),
    ("gtk-theme", |theme| theme.gtk_theme.clone()),
];

/// Set once at startup and deliberately absent from the reload path: a font
/// change is restart-only, which `config::theme_requires_restart` decides and
/// the reload warning tells the user.
const STARTUP_ONLY_KEYS: &[ThemeKey] = &[("font-name", |theme| theme.font.clone())];

/// Every key in `keys` the theme has a value for.
fn settings(keys: &[ThemeKey], theme: &config::ThemeConfig) -> Vec<(&'static str, String)> {
    keys.iter()
        .filter_map(|(key, read)| read(theme).map(|value| (*key, value)))
        .collect()
}

/// Every key in `keys` whose value differs, paired with the new one - `None`
/// meaning the key should be reset rather than set.
fn changed(
    keys: &[ThemeKey],
    current: &config::ThemeConfig,
    updated: &config::ThemeConfig,
) -> Vec<(&'static str, Option<String>)> {
    keys.iter()
        .filter_map(|(key, read)| {
            let value = read(updated);
            (read(current) != value).then_some((*key, value))
        })
        .collect()
}

fn theme_gsettings(theme: &config::ThemeConfig) -> Vec<(&'static str, String)> {
    [CURSOR_KEYS, DESKTOP_KEYS, STARTUP_ONLY_KEYS]
        .into_iter()
        .flat_map(|keys| settings(keys, theme))
        .collect()
}

fn changed_cursor_gsettings(
    current: &config::ThemeConfig,
    updated: &config::ThemeConfig,
) -> Vec<(&'static str, Option<String>)> {
    changed(CURSOR_KEYS, current, updated)
}

fn changed_desktop_gsettings(
    current: &config::ThemeConfig,
    updated: &config::ThemeConfig,
) -> Vec<(&'static str, Option<String>)> {
    changed(DESKTOP_KEYS, current, updated)
}

/// Live children, so logging out does not leave them behind holding the login
/// session open.
///
/// The `Child` handles are owned, not just their pids: an unwaited handle
/// keeps the pid reserved, so there is no window in which a dead child's pid
/// could be reused by something else and signalled at shutdown.
static CHILDREN: Mutex<Vec<std::process::Child>> = Mutex::new(Vec::new());

fn children() -> MutexGuard<'static, Vec<std::process::Child>> {
    CHILDREN.lock().unwrap_or_else(|error| error.into_inner())
}

/// Drop the children that have already exited.
///
/// Called from the config-reload timer, which is running twice a second
/// anyway. That is what replaced a thread per child whose only job was to sit
/// in `wait`.
fn reap_children() {
    children().retain_mut(|child| matches!(child.try_wait(), Ok(None)));
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
    let child = std::process::Command::new(program)
        .args(args)
        .envs(cursor_settings.environment_values())
        .spawn()?;
    children().push(child);
    Ok(())
}

/// Wayland clients exit on their own once the socket closes; anything else we
/// launched would outlive the session and keep logind from ending it.
fn terminate_children() {
    // Signalled, not waited for: a child that ignores `TERM` must not hold the
    // session open past logout. The process is on its way out, so the zombies
    // this leaves are reparented and reaped by init.
    for child in children().drain(..) {
        if let Some(pid) = Pid::from_raw(child.id() as i32) {
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

/// Deny same-user ptrace and suppress core dumps.
///
/// Paw2 holds every window's pixels, the clipboard, and the merged config in one
/// address space, and none of the allowlists mean anything to something reading
/// that memory directly. `ptrace(2)` access rule (4) denies a tracer without
/// `CAP_SYS_PTRACE` when the target is not dumpable, and a non-dumpable process
/// writes no core - so this one call closes both paths without a seccomp filter
/// or an LSM policy. It matters more since `panic = "abort"`: aborting is right
/// for the DRM state, but it is also what would hand a core dump full of window
/// contents to `systemd-coredump`.
///
/// Children are unaffected - `dumpable` resets to 1 on `execve`, so a terminal
/// spawned from a binding is as debuggable as it ever was. Note that
/// `PR_SET_NO_NEW_PRIVS` is deliberately *not* set alongside this: that one *is*
/// inherited, and would break `sudo` and `pkexec` in every such terminal.
///
/// `PAW2_DUMPABLE` is the escape hatch for attaching a debugger to Paw2 itself.
fn restrict_process_memory() {
    if std::env::var_os("PAW2_DUMPABLE").is_some() {
        tracing::warn!("PAW2_DUMPABLE is set; core dumps and debugger attach are permitted");
        return;
    }
    // SAFETY: `PR_SET_DUMPABLE` takes no pointers and touches only this
    // process's own credentials.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) } != 0 {
        tracing::warn!(
            error = %io::Error::last_os_error(),
            "could not disable core dumps and debugger attach"
        );
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
    // One test, not two: `CHILDREN` is a global and `terminate_children` drains
    // it, so a sibling test running in parallel would be reaping this one's.
    fn shutdown_terminates_tracked_children() {
        let settings = cursor::CursorSettings::environment();
        spawn(&["sleep", "10"], &settings).unwrap();
        spawn(&["true"], &settings).unwrap();
        assert_eq!(children().len(), 2);

        // `true` exits immediately, but not synchronously with `spawn`; the
        // sleeper has to survive every reap.
        let reaped = (0..200).any(|_| {
            reap_children();
            std::thread::sleep(Duration::from_millis(10));
            children().len() == 1
        });
        assert!(reaped, "an exited child was never reaped");

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
    fn qt_platform_theme_replaces_the_gtk3_default() {
        let theme = config::ThemeConfig {
            icon_theme: Some("Icons".into()),
            qt_platform_theme: Some("qt6ct".into()),
            ..Default::default()
        };
        assert_eq!(
            theme_environment(&theme),
            [("QT_QPA_PLATFORMTHEME", "qt6ct".into())]
        );
    }

    #[test]
    fn maps_theme_to_desktop_interfaces() {
        let theme = config::ThemeConfig {
            cursor_theme: Some("Cursor".into()),
            cursor_size: Some(32),
            icon_theme: Some("Icons".into()),
            gtk_theme: Some("Gtk".into()),
            qt_style: Some("Qt".into()),
            qt_platform_theme: None,
            font: Some("Sans 11".into()),
            titlebar_font: None,
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
