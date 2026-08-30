//! A JSON-lines control socket, for a shell that wants more than the Wayland
//! protocols offer.
//!
//! Paw2 is fully usable without this: workspaces are on `ext-workspace-v1`, the
//! window list on `ext-foreign-toplevel-list-v1`, capture on
//! `ext-image-copy-capture-v1`. What a panel cannot get from any of those is a
//! window's workspace, the active tiling layout, or the effective config - so
//! that, plus the four actions a panel needs, is all this carries.
//!
//! Configuration deliberately only travels outwards. A shell changes settings
//! by writing `config.d/`, which the config watcher already picks up; there is
//! no "write this key" command, and so no second path into the config with its
//! own idea of what is valid.
//!
//! The trust boundary is `$XDG_RUNTIME_DIR`, exactly as it is for the Wayland
//! socket sitting next to it. This is not an authenticated interface.

use std::{
    collections::VecDeque,
    io::{ErrorKind, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction, generic::Generic};

use crate::{
    config::{Action, LayoutMode},
    state::Paw2,
};

/// A window, as a panel needs it. `id` is the same identity
/// `ext-foreign-toplevel-list-v1` hands out, so the two views agree.
#[derive(Serialize, PartialEq)]
pub struct WindowState {
    pub id: usize,
    pub title: String,
    pub app_id: String,
    pub output: String,
    pub workspace: usize,
    pub focused: bool,
}

#[derive(Serialize, PartialEq)]
pub struct ScratchpadState {
    pub name: String,
    pub app_id: String,
    pub loaded: bool,
    pub visible: bool,
    pub output: Option<String>,
}

/// One entry in a monitor's mode list.
#[derive(Serialize, PartialEq)]
pub struct ModeState {
    pub width: i32,
    pub height: i32,
    /// Hz, as the config's `refresh` wants it.
    pub refresh: f64,
    pub current: bool,
}

/// A monitor and its nine workspace strips.
#[derive(Serialize, PartialEq)]
pub struct OutputState {
    pub name: String,
    /// Whether this monitor has the keyboard focus.
    pub focused: bool,
    pub active: usize,
    /// One flag per workspace: whether anything is on it.
    pub occupied: Vec<bool>,
    /// The active workspace's tiling layout.
    pub layout: String,
    /// Every mode the display advertises. Paw2 has no
    /// wlr-output-management, so a settings UI writes `[[monitors]]`; the
    /// config watcher applies resolution, refresh, scale, and transform live.
    pub modes: Vec<ModeState>,
}

#[derive(Serialize)]
struct Snapshot<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    outputs: &'a [OutputState],
    windows: &'a [WindowState],
    scratchpads: &'a [ScratchpadState],
    /// The layouts `cycle_layout` steps through, in order.
    layouts: &'a [String],
}

#[derive(Serialize)]
struct ConfigMessage<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    /// The merged config: compiled defaults, `config.toml`, then `config.d/`.
    values: &'a serde_json::Value,
    ok: bool,
    errors: Vec<&'a str>,
}

/// A connected shell.
struct Client {
    id: u64,
    stream: UnixStream,
    /// Bytes read but not yet terminated by a newline.
    pending: Vec<u8>,
}

#[derive(Default)]
pub struct IpcState {
    path: Option<PathBuf>,
    clients: Vec<Client>,
    next_id: u64,
    /// Set by the compositor whenever what a panel draws has changed. Coalesced
    /// into one broadcast per event-loop iteration by [`flush`], because a
    /// terminal with a spinner in its title changes state ten times a second
    /// and a bar only ever draws once per frame.
    dirty: bool,
    /// The config changed and every client is owed the new one, not just the
    /// ones that just connected.
    config_dirty: bool,
    /// Newly connected clients, owed a snapshot even if nothing has changed.
    greeting: VecDeque<u64>,
    config: serde_json::Value,
    config_error: Option<String>,
}

impl IpcState {
    /// Note that a panel's view of the world is out of date.
    pub fn touch(&mut self) {
        self.dirty = true;
    }

    /// Publish the merged config and any load or mode-application error. A load
    /// failure keeps the last parsed document; for a rejected monitor mode the
    /// document is the request and the output snapshot remains the actual state.
    pub fn publish_config(&mut self, document: &toml::Value, error: Option<String>) {
        self.config = serde_json::to_value(document).unwrap_or(serde_json::Value::Null);
        self.config_error = error;
        self.config_dirty = true;
        self.dirty = true;
    }
}

impl Drop for IpcState {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Bind the socket and start accepting shells.
///
/// A failure here is logged and swallowed: the socket is an optional extra, and
/// losing it is not a reason to refuse to start a session.
pub fn init(handle: &LoopHandle<'static, Paw2>, state: &mut Paw2) {
    let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") else {
        tracing::warn!("XDG_RUNTIME_DIR is unset; the control socket is unavailable");
        return;
    };
    let path = PathBuf::from(runtime_dir)
        .join(format!("paw2-{}.sock", state.socket_name.to_string_lossy()));
    // A socket left behind by a crash would refuse the bind; nothing can be
    // listening on it, because the Wayland socket it is named after was free.
    let _ = std::fs::remove_file(&path);

    let listener = match UnixListener::bind(&path).and_then(|listener| {
        listener.set_nonblocking(true)?;
        Ok(listener)
    }) {
        Ok(listener) => listener,
        Err(error) => {
            tracing::warn!(?error, ?path, "could not bind the control socket");
            return;
        }
    };

    let registered = handle.insert_source(
        Generic::new(listener, Interest::READ, Mode::Level),
        |_, listener, state| {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => accept(state, stream),
                    Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                    Err(error) => {
                        tracing::warn!(?error, "control socket accept failed");
                        break;
                    }
                }
            }
            Ok(PostAction::Continue)
        },
    );
    if let Err(error) = registered {
        tracing::warn!(?error, "could not watch the control socket");
        return;
    }

    state.ipc.path = Some(path.clone());
    // SAFETY: Paw2 is single-threaded and sets the child environment before
    // spawning a client or entering the event loop.
    unsafe { std::env::set_var("PAW2_SOCKET", &path) };
    tracing::info!(?path, "control socket ready");
}

fn accept(state: &mut Paw2, stream: UnixStream) {
    if let Err(error) = stream.set_nonblocking(true) {
        tracing::warn!(?error, "could not accept a control client");
        return;
    }
    let Ok(reader) = stream.try_clone() else {
        return;
    };
    let id = state.ipc.next_id;
    state.ipc.next_id += 1;

    let registered = state.loop_handle.insert_source(
        Generic::new(reader, Interest::READ, Mode::Level),
        move |_, reader, state| Ok(read_client(state, id, reader)),
    );
    if let Err(error) = registered {
        tracing::warn!(?error, "could not watch a control client");
        return;
    }
    state.ipc.clients.push(Client {
        id,
        stream,
        pending: Vec::new(),
    });
    state.ipc.greeting.push_back(id);
}

/// Drain whatever a client has sent, running each complete line.
fn read_client(state: &mut Paw2, id: u64, reader: &UnixStream) -> PostAction {
    // `&UnixStream` reads just fine; the event source only lends it out shared.
    let mut reader = reader;
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return disconnect(state, id),
            Ok(read) => {
                let Some(client) = state.ipc.clients.iter_mut().find(|c| c.id == id) else {
                    return PostAction::Remove;
                };
                client.pending.extend_from_slice(&chunk[..read]);
                // A peer that never sends a newline must not be able to grow
                // this without bound. Nothing legitimate comes close.
                if client.pending.len() > 64 * 1024 {
                    tracing::warn!("control client sent an oversized command");
                    return disconnect(state, id);
                }
                for line in take_lines(&mut client.pending) {
                    run(state, &line);
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => return PostAction::Continue,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => {
                tracing::debug!(?error, "control client read failed");
                return disconnect(state, id);
            }
        }
    }
}

/// Split off every complete line, leaving any partial tail in the buffer.
fn take_lines(pending: &mut Vec<u8>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
        let line = pending.drain(..=end).collect::<Vec<_>>();
        if let Ok(text) = String::from_utf8(line) {
            lines.push(text);
        }
    }
    lines
}

fn disconnect(state: &mut Paw2, id: u64) -> PostAction {
    state.ipc.clients.retain(|client| client.id != id);
    PostAction::Remove
}

/// What a shell may ask for.
///
/// Deserialised rather than picked apart by hand: serde gives the field types,
/// the missing-field errors and the unknown-command error for free, and a
/// command's shape is then stated once, here. Unknown *fields* are still
/// ignored, as they always were, so a newer shell talking to an older Paw2
/// keeps working.
#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
enum Command {
    /// `output` is accepted because a bar is per-monitor, so a click can land
    /// on a workspace belonging to a monitor that does not have focus, and
    /// `SwitchWorkspace` only ever acts on the focused one. Same for
    /// `set-layout` below.
    FocusWorkspace {
        index: usize,
        output: Option<String>,
    },
    FocusWindow {
        id: usize,
    },
    SetLayout {
        layout: Option<LayoutMode>,
        output: Option<String>,
    },
    ToggleScratchpad {
        name: String,
    },
    ShowScratchpad {
        name: String,
    },
    HideScratchpad {
        name: String,
    },
    ResetScratchpad {
        name: String,
    },
    ToggleScratchpadOverview,
    Quit,
    // No set-config: settings are changed by writing config.d/, which the
    // config watcher picks up within half a second.
}

fn run(state: &mut Paw2, line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let command = match serde_json::from_str::<Command>(line) {
        Ok(command) => command,
        Err(error) => {
            tracing::warn!(%error, "unusable control command");
            return;
        }
    };
    // Follow the click before acting: the actions below only ever reach the
    // focused output.
    if let Command::FocusWorkspace {
        output: Some(output),
        ..
    }
    | Command::SetLayout {
        output: Some(output),
        ..
    } = &command
    {
        state.focus_output_by_name(output);
    }
    match command {
        Command::FocusWorkspace { index, .. } => state.run_action(Action::SwitchWorkspace(index)),
        Command::FocusWindow { id } => state.focus_window_by_id(id),
        Command::SetLayout { layout, .. } => {
            state.run_action(layout.map_or(Action::CycleLayout, Action::SwitchLayout))
        }
        Command::ToggleScratchpad { name } => state.toggle_scratchpad_by_name(&name),
        Command::ShowScratchpad { name } => state.show_scratchpad_by_name(&name),
        Command::HideScratchpad { name } => state.hide_scratchpad(&name),
        Command::ResetScratchpad { name } => state.reset_scratchpad(&name),
        Command::ToggleScratchpadOverview => state.toggle_scratchpad_overview(),
        Command::Quit => state.run_action(Action::Quit),
    }
}

/// Send every client whatever it is owed, once per event-loop iteration.
pub fn flush(state: &mut Paw2) {
    if state.ipc.clients.is_empty() {
        state.ipc.dirty = false;
        state.ipc.config_dirty = false;
        state.ipc.greeting.clear();
        return;
    }
    let greeting = std::mem::take(&mut state.ipc.greeting);
    if !state.ipc.dirty && !state.ipc.config_dirty && greeting.is_empty() {
        return;
    }
    let config_changed = state.ipc.config_dirty;
    state.ipc.dirty = false;
    state.ipc.config_dirty = false;

    let (outputs, windows) = state.scroller.ipc_state();
    let scratchpads = state.scratchpad_ipc_state();
    let layouts = state
        .config
        .layout
        .enabled_layouts
        .iter()
        .map(|layout| layout.name().to_string())
        .collect::<Vec<_>>();
    let snapshot = serialize(&Snapshot {
        kind: "snapshot",
        outputs: &outputs,
        windows: &windows,
        scratchpads: &scratchpads,
        layouts: &layouts,
    });
    let config = serialize(&ConfigMessage {
        kind: "config",
        values: &state.ipc.config,
        ok: state.ipc.config_error.is_none(),
        errors: state.ipc.config_error.as_deref().into_iter().collect(),
    });

    let mut dropped = Vec::new();
    for client in &mut state.ipc.clients {
        // A client that just connected has no config at all yet; everyone else
        // needs it only when it actually changed.
        let owed: &[&str] = if config_changed || greeting.contains(&client.id) {
            &[&config, &snapshot]
        } else {
            &[&snapshot]
        };
        for message in owed {
            if !send(client, message) {
                dropped.push(client.id);
                break;
            }
        }
    }
    state
        .ipc
        .clients
        .retain(|client| !dropped.contains(&client.id));
}

fn serialize<T: Serialize>(message: &T) -> String {
    match serde_json::to_string(message) {
        Ok(mut text) => {
            text.push('\n');
            text
        }
        Err(error) => {
            tracing::warn!(%error, "could not serialise a control message");
            String::new()
        }
    }
}

/// False when the client is gone and should be dropped.
///
/// A write that cannot complete drops the client rather than buffering the
/// remainder: half a line on the wire desyncs the reader permanently, and a
/// peer more than a socket buffer behind is not reading at all. Reconnecting
/// costs it nothing, because every message is complete state.
fn send(client: &mut Client, message: &str) -> bool {
    let mut remaining = message.as_bytes();
    while !remaining.is_empty() {
        match client.stream.write(remaining) {
            Ok(0) => return false,
            Ok(written) => remaining = &remaining[written..],
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => {
                tracing::debug!(?error, "control client write failed");
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_lines_keeps_a_partial_command_buffered() {
        let mut pending = Vec::new();

        // A command split across two reads is not run until it is complete.
        pending.extend_from_slice(br#"{"cmd":"qu"#);
        assert!(take_lines(&mut pending).is_empty());
        pending.extend_from_slice(b"it\"}\n");
        assert_eq!(
            take_lines(&mut pending),
            [r#"{"cmd":"quit"}"#.to_string() + "\n"]
        );
        assert!(pending.is_empty());

        // Several commands in one read all run, and a trailing fragment stays.
        pending.extend_from_slice(b"one\ntwo\nthr");
        assert_eq!(take_lines(&mut pending), ["one\n", "two\n"]);
        assert_eq!(pending, b"thr");
    }
}
