#![allow(irrefutable_let_patterns)]

mod backend;
mod config;
mod drm;
mod handlers;
mod input;
mod layout;
mod render;
mod screencopy;
mod state;
mod workspace;
mod xwayland;

use std::{
    ffi::{OsStr, OsString},
    io,
    time::Duration,
};

use smithay::reexports::{
    calloop::{EventLoop, timer::TimeoutAction, timer::Timer},
    wayland_server::Display,
};
use state::Paw2;

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Direct,
    Nested(Vec<OsString>),
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();

    let Some(mode) = parse_args(std::env::args_os().skip(1))? else {
        print_usage();
        return Ok(());
    };
    let (config, mut config_watcher) = config::load()?;
    let mut event_loop: EventLoop<Paw2> = EventLoop::try_new()?;
    let display = Display::new()?;
    let mut state = Paw2::new(&mut event_loop, display, config);

    event_loop.handle().insert_source(
        Timer::from_duration(Duration::from_millis(500)),
        move |_, _, state| {
            match config_watcher.reload() {
                Ok(Some(config)) => {
                    let restart_required = state.config.apply_runtime(config);
                    state.relayout(true);
                    drm::redraw(state);
                    tracing::info!("reloaded configuration");
                    if restart_required {
                        tracing::warn!(
                            "backend, monitor, autostart, and input protocol changes require restarting Paw2"
                        );
                    }
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(%error, "could not reload configuration"),
            }
            TimeoutAction::ToDuration(Duration::from_millis(500))
        },
    )?;

    match &mode {
        Mode::Direct => drm::init(&mut event_loop, &mut state)?,
        Mode::Nested(_) => backend::init(&mut event_loop, &mut state)?,
    }

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
    match xwayland::start(event_loop.handle(), &mut state) {
        Ok(display) => unsafe { std::env::set_var("DISPLAY", format!(":{display}")) },
        Err(error) => tracing::warn!(?error, "XWayland unavailable; continuing Wayland-only"),
    }
    if mode == Mode::Direct {
        import_activation_environment();
    }

    for command in &state.config.autostart {
        if let Err(error) = spawn(command) {
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
        spawn(command)?;
    }

    event_loop.run(None, &mut state, |state| {
        let _ = state.display_handle.flush_clients();
    })?;
    Ok(())
}

fn import_activation_environment() {
    let mut variables = vec![
        "--systemd",
        "WAYLAND_DISPLAY",
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_DESKTOP",
    ];
    if std::env::var_os("DISPLAY").is_some() {
        variables.push("DISPLAY");
    }
    let status = std::process::Command::new("dbus-update-activation-environment")
        .args(variables)
        .status();
    match status {
        Ok(status) if status.success() => {}
        Ok(status) => tracing::warn!(%status, "could not import portal activation environment"),
        Err(error) => tracing::warn!(?error, "could not run dbus-update-activation-environment"),
    }
}

pub(crate) fn spawn<T: AsRef<OsStr>>(command: &[T]) -> io::Result<()> {
    let Some((program, args)) = command.split_first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "command cannot be empty",
        ));
    };
    let mut child = std::process::Command::new(program).args(args).spawn()?;
    // ponytail: one waiter per child; replace with session process supervision if launch volume grows.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
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

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
