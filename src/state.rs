use std::{
    ffi::OsString,
    os::unix::net::UnixStream,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use smithay::{
    desktop::PopupManager,
    input::{
        Seat, SeatState,
        keyboard::{KeyboardHandle, XkbConfig},
        pointer::PointerHandle,
    },
    reexports::{
        calloop::{
            EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction, generic::Generic,
        },
        wayland_protocols::xwayland::keyboard_grab::zv1::server::zwp_xwayland_keyboard_grab_v1::ZwpXwaylandKeyboardGrabV1,
        wayland_server::{
            Client, Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{Clock, ClockSource, Logical, Monotonic, Point},
    wayland::{
        alpha_modifier::AlphaModifierState,
        background_effect::BackgroundEffectState,
        commit_timing::CommitTimingManagerState,
        compositor::{CompositorClientState, CompositorState},
        content_type::ContentTypeState,
        cursor_shape::CursorShapeManagerState,
        dmabuf::DmabufState,
        drm_syncobj::DrmSyncobjState,
        fifo::FifoManagerState,
        fractional_scale::FractionalScaleManagerState,
        idle_inhibit::IdleInhibitManagerState,
        keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState,
        output::OutputManagerState,
        pointer_constraints::PointerConstraintsState,
        presentation::PresentationState,
        relative_pointer::RelativePointerManagerState,
        security_context::SecurityContextState,
        selection::{
            data_device::DataDeviceState, ext_data_control::DataControlState,
            primary_selection::PrimarySelectionState,
        },
        shell::{
            wlr_layer::WlrLayerShellState,
            xdg::{XdgShellState, decoration::XdgDecorationState, dialog::XdgDialogState},
        },
        shm::ShmState,
        single_pixel_buffer::SinglePixelBufferState,
        socket::ListeningSocketSource,
        viewporter::ViewporterState,
        xwayland_keyboard_grab::XWaylandKeyboardGrabState,
        xwayland_shell::XWaylandShellState,
    },
    xwayland::X11Wm,
};

use crate::{
    config::{Action, Config},
    ipc::IpcState,
    layout::Scrollers,
    screencopy::ScreencopyState,
    workspace::WorkspaceState,
};

pub struct Paw2 {
    pub start_time: Instant,
    pub clock: Clock<Monotonic>,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,
    pub loop_handle: LoopHandle<'static, Self>,
    pub loop_signal: LoopSignal,
    pub popups: PopupManager,
    pub seat: Seat<Self>,
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub xwayland_shell_state: XWaylandShellState,
    pub xwm: Option<X11Wm>,
    pub layer_shell_state: WlrLayerShellState,
    pub shm_state: ShmState,
    pub keyboard_shortcuts_inhibit_state: KeyboardShortcutsInhibitState,
    /// The live `zwp_xwayland_keyboard_grab_v1`, if an XWayland client took the
    /// keyboard. Smithay drops the grab when the client destroys the object and
    /// offers no ungrab callback, so the resource's own liveness is the signal.
    pub xwayland_keyboard_grab: Option<ZwpXwaylandKeyboardGrabV1>,
    pub drm_syncobj_state: Option<DrmSyncobjState>,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    /// Read by the primary-selection global's visibility filter on every bind,
    /// so `[input] middle_click_paste` takes effect on save. See
    /// [`Paw2::set_middle_click_paste`] for what a live change does not reach.
    primary_selection_enabled: Arc<AtomicBool>,
    pub data_control_state: DataControlState,
    pub dmabuf_state: DmabufState,
    pub background_effect_state: Option<BackgroundEffectState>,
    pub blur_surfaces: Vec<WlSurface>,
    pub cursor_status: smithay::input::pointer::CursorImageStatus,
    pub cursor_position_hint: Option<(WlSurface, Point<f64, Logical>)>,
    pub cursor_defaults: crate::cursor::CursorSettings,
    pub direct: Option<crate::drm::DirectBackend>,
    pub config: Config,
    pub scroller: Scrollers,
    pub suppressed_keys: Vec<smithay::input::keyboard::Keysym>,
    /// The keyboard layout panels were last told about; see `note_keyboard_layout`.
    pub keyboard_layout: smithay::input::keyboard::Layout,
    pub wheel_v120_remainder: f64,
    pub pending_wheel_action: Option<Action>,
    pub workspace_state: WorkspaceState,
    pub screencopy_state: ScreencopyState,
    pub ipc: IpcState,
    pub clipboard: crate::clipboard::ClipboardState,
    pub session: crate::session::SessionState,
    pub gamma: crate::gamma::GammaState,
}

/// Whether the compositor is on its way out.
///
/// This is a global rather than a field on [`Paw2`] because the `SIGTERM`
/// handler has to set it too, and a signal handler cannot reach the state.
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// Mark the compositor as shutting down without stopping the event loop; only
/// the signal handler needs this, everything else calls [`Paw2::begin_shutdown`].
pub fn start_shutdown() {
    SHUTTING_DOWN.store(true, Ordering::Relaxed);
}

/// Once this is true nothing may queue another page flip: a frame submitted
/// after the last dispatch is never acknowledged, and the outstanding flip makes
/// the DRM state restore fail with `EBUSY`.
pub fn shutting_down() -> bool {
    SHUTTING_DOWN.load(Ordering::Relaxed)
}

impl Paw2 {
    pub fn new(
        event_loop: &mut EventLoop<'static, Self>,
        display: Display<Self>,
        config: Config,
        cursor_defaults: crate::cursor::CursorSettings,
    ) -> Self {
        let display_handle = display.handle();
        let clock = Clock::<Monotonic>::new();
        // Only the protocol states Paw2 actually reads are bound to a name and
        // kept on `Self`. The rest are created and dropped: at the pinned
        // smithay revision each is a bare `GlobalId`, `GlobalId` has no `Drop`,
        // and a global is removed by `DisplayHandle::remove_global` and nothing
        // else - so holding one changes nothing but the size of this struct.
        // Re-check that if smithay is bumped.
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        XdgDialogState::new::<Self>(&display_handle);
        // Answered in `decoration`: Paw2 draws titlebars for apps that ask.
        XdgDecorationState::new::<Self>(&display_handle);
        let xwayland_shell_state = XWaylandShellState::new::<Self>(&display_handle);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, vec![]);
        OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
        IdleInhibitManagerState::new::<Self>(&display_handle);
        let keyboard_shortcuts_inhibit_state =
            KeyboardShortcutsInhibitState::new::<Self>(&display_handle);
        XWaylandKeyboardGrabState::new::<Self>(&display_handle);
        PresentationState::new::<Self>(&display_handle, Monotonic::ID as u32);
        FifoManagerState::new::<Self>(&display_handle);
        FractionalScaleManagerState::new::<Self>(&display_handle);
        CommitTimingManagerState::new::<Self>(&display_handle);
        ContentTypeState::new::<Self>(&display_handle);
        CursorShapeManagerState::new::<Self>(&display_handle);
        RelativePointerManagerState::new::<Self>(&display_handle);
        PointerConstraintsState::new::<Self>(&display_handle);
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);
        let middle_click_paste = config.input.middle_click_paste;
        // Shared rather than captured by value: the filter runs on every bind,
        // so reading it live is what lets the setting apply without a restart.
        let primary_selection_enabled = Arc::new(AtomicBool::new(middle_click_paste));
        let enabled = Arc::clone(&primary_selection_enabled);
        let primary_selection_state =
            PrimarySelectionState::new_with_filter::<Self, _>(&display_handle, move |_| {
                enabled.load(Ordering::Relaxed)
            });
        // Clipboard managers read every copy the moment it happens, so this is
        // only for the programs named in `[security] clipboard_clients`.
        let data_control_state = DataControlState::new::<Self, _>(
            &display_handle,
            middle_click_paste.then_some(&primary_selection_state),
            is_clipboard_client,
        );
        // Lets a sandbox such as Flatpak hand its apps a socket of their own,
        // so Paw2 knows they are sandboxed whatever their binary is called. A
        // sandboxed client must not see this global, or it could open a fresh
        // socket that looks unsandboxed.
        SecurityContextState::new::<Self, _>(&display_handle, is_unsandboxed);
        // Both are handled entirely inside smithay's surface renderer, which
        // Paw2 already goes through; neither needs anything of its own here.
        ViewporterState::new::<Self>(&display_handle);
        SinglePixelBufferState::new::<Self>(&display_handle);
        AlphaModifierState::new::<Self>(&display_handle);
        let workspace_state = WorkspaceState::new(&display_handle);
        let screencopy_state = ScreencopyState::new(&display_handle);
        let session = crate::session::SessionState::new(&display_handle, event_loop.handle());
        let gamma = crate::gamma::GammaState::new(&display_handle);

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&display_handle, "paw2");
        // `XkbConfig::default()` reads the `XKB_DEFAULT_*` environment, so a
        // typo out there must not cost the session its keyboard entirely.
        if let Err(error) = seat.add_keyboard(XkbConfig::default(), 200, 25) {
            tracing::error!(?error, "could not compile the keymap; falling back to us");
            seat.add_keyboard(
                XkbConfig {
                    layout: "us",
                    ..XkbConfig::default()
                },
                200,
                25,
            )
            .expect("the built-in us keymap compiles");
        }
        seat.add_pointer();

        let socket_name = Self::listen(display, event_loop);

        Self {
            start_time: Instant::now(),
            clock,
            socket_name,
            display_handle,
            loop_handle: event_loop.handle(),
            loop_signal: event_loop.get_signal(),
            popups: PopupManager::default(),
            seat,
            compositor_state,
            xdg_shell_state,
            xwayland_shell_state,
            xwm: None,
            layer_shell_state,
            shm_state,
            keyboard_shortcuts_inhibit_state,
            xwayland_keyboard_grab: None,
            drm_syncobj_state: None,
            seat_state,
            data_device_state,
            primary_selection_state,
            primary_selection_enabled,
            data_control_state,
            dmabuf_state: DmabufState::new(),
            background_effect_state: None,
            blur_surfaces: Vec::new(),
            cursor_status: smithay::input::pointer::CursorImageStatus::default_named(),
            cursor_position_hint: None,
            cursor_defaults,
            direct: None,
            config,
            scroller: Scrollers::default(),
            suppressed_keys: Vec::new(),
            keyboard_layout: Default::default(),
            wheel_v120_remainder: 0.0,
            pending_wheel_action: None,
            workspace_state,
            screencopy_state,
            ipc: IpcState::default(),
            clipboard: Default::default(),
            session,
            gamma,
        }
    }

    /// Every exit path goes through here so shutdown behaves the same whether it
    /// was a keybinding, a lost device, or a closed nested window.
    /// The seat's pointer.
    ///
    /// Both capabilities are added in [`Self::new`] and never removed, so this
    /// cannot fail. Stated once here rather than as an `unwrap` at each of the
    /// dozen-odd call sites.
    pub fn pointer(&self) -> PointerHandle<Self> {
        self.seat
            .get_pointer()
            .expect("the seat has had a pointer since startup")
    }

    /// The seat's keyboard. See [`Self::pointer`].
    /// Tell panels when a key switched the keyboard layout (an xkb group
    /// toggle). Only the focused client hears `wl_keyboard` group changes, and
    /// a bar almost never has the focus.
    pub fn note_keyboard_layout(&mut self) {
        let layout = self.keyboard().with_xkb_state(self, |context| {
            context.xkb().lock().unwrap().active_layout()
        });
        if layout != self.keyboard_layout {
            self.keyboard_layout = layout;
            self.ipc.touch();
        }
    }

    /// The keyboard as panels see it: the active layout and lock-key LEDs.
    pub fn keyboard_ipc_state(&mut self) -> crate::ipc::KeyboardState {
        let keyboard = self.keyboard();
        let leds = keyboard.led_state();
        let (layout, layouts) = keyboard.with_xkb_state(self, |context| {
            let xkb = context.xkb().lock().unwrap();
            (
                xkb.layout_name(xkb.active_layout()).to_owned(),
                xkb.layouts()
                    .map(|layout| xkb.layout_name(layout).to_owned())
                    .collect(),
            )
        });
        crate::ipc::KeyboardState {
            layout,
            layouts,
            caps_lock: leds.caps.unwrap_or(false),
            num_lock: leds.num.unwrap_or(false),
            scroll_lock: leds.scroll.unwrap_or(false),
        }
    }

    pub fn keyboard(&self) -> KeyboardHandle<Self> {
        self.seat
            .get_keyboard()
            .expect("the seat has had a keyboard since startup")
    }

    pub fn begin_shutdown(&mut self) {
        start_shutdown();
        self.loop_signal.stop();
    }

    /// Follow `[input] middle_click_paste` after a reload.
    ///
    /// The primary-selection global's filter reads the shared flag on every
    /// bind, and `xwayland.rs` reads `self.config` per call, so both follow
    /// immediately. Two things do not: a client already holding the global
    /// keeps it, and `ext-data-control`'s primary-selection arm was decided
    /// when `DataControlState` was built. So turning it off shuts the door on
    /// everything new, but a privileged clipboard manager that bound at startup
    /// keeps its access until Paw2 restarts.
    pub fn set_middle_click_paste(&self, enabled: bool) {
        self.primary_selection_enabled
            .store(enabled, Ordering::Relaxed);
    }

    pub fn enable_background_effects(&mut self) {
        if self.background_effect_state.is_none() && self.config.appearance.blur_enabled() {
            self.background_effect_state =
                Some(BackgroundEffectState::new::<Self>(&self.display_handle));
        }
    }

    fn listen(display: Display<Self>, event_loop: &mut EventLoop<Self>) -> OsString {
        let socket = ListeningSocketSource::new_auto().expect("failed to create Wayland socket");
        let socket_name = socket.socket_name().to_os_string();

        event_loop
            .handle()
            .insert_source(socket, |stream, _, state| {
                // Resolved once, here: the filter closures on the privileged
                // globals only get a `&Client`, and reaching credentials from
                // there would mean storing a `DisplayHandle` inside data the
                // display itself owns.
                let trust = client_trust(&stream, &state.config.security.clipboard_clients);
                state.accept_client(stream, ClientState::new(trust));
            })
            .expect("failed to listen for Wayland clients");

        event_loop
            .handle()
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, state| {
                    // SAFETY: the display remains owned by this event source.
                    unsafe { display.get_mut().dispatch_clients(state) }?;
                    Ok(PostAction::Continue)
                },
            )
            .expect("failed to add the Wayland display to the event loop");

        socket_name
    }

    /// Hand a connected stream to the Wayland display.
    pub fn accept_client(&mut self, stream: UnixStream, client_state: ClientState) {
        // Refusing one client must never end the session: hitting the
        // open-file limit is recoverable, losing every running program is not.
        if let Err(error) = self
            .display_handle
            .insert_client(stream, Arc::new(client_state))
        {
            tracing::warn!(?error, "failed to accept Wayland client");
        }
    }
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
    /// Whether this client may bind the globals that expose other clients'
    /// windows. See [`client_trust`] for what that is worth.
    pub privileged: bool,
    /// Whether this client may watch the clipboard through `ext-data-control`.
    pub clipboard: bool,
    /// Connected through a sandbox's security-context socket. Never privileged.
    pub sandboxed: bool,
}

impl ClientState {
    fn new(trust: ClientTrust) -> Self {
        Self {
            privileged: trust.privileged,
            clipboard: trust.clipboard,
            ..Default::default()
        }
    }

    pub fn sandboxed() -> Self {
        Self {
            sandboxed: true,
            ..Default::default()
        }
    }
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// Client filter for the globals that expose other clients' windows: the
/// foreign-toplevel list, per-window capture, and workspace control.
///
/// Whole-output capture is deliberately not gated on this, so `grim` and
/// `wf-recorder` keep working for everyone.
pub fn is_privileged(client: &Client) -> bool {
    client
        .get_data::<ClientState>()
        .is_some_and(|state| state.privileged)
}

/// Client filter for globals no sandboxed app may see: the security-context
/// global itself (or it could open a fresh socket that looks unsandboxed), and
/// session locking and idle reporting.
pub(crate) fn is_unsandboxed(client: &Client) -> bool {
    !client
        .get_data::<ClientState>()
        .is_some_and(|state| state.sandboxed)
}

/// Client filter for `ext-data-control`, which sees every copy as it happens.
fn is_clipboard_client(client: &Client) -> bool {
    client
        .get_data::<ClientState>()
        .is_some_and(|state| state.clipboard)
}

/// What a connecting, unsandboxed peer is allowed to see.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ClientTrust {
    /// Window titles, per-window capture, and the privileged control socket.
    pub privileged: bool,
    /// The clipboard, through `ext-data-control`.
    pub clipboard: bool,
}

/// Decide what a peer may see from the program it is running.
///
/// A program installed by the package manager is trusted with window titles
/// and per-window capture; anything a user account could have written - a
/// game under `~`, a download, a copy in `/tmp` - is not. That makes bars,
/// shells and the screen-sharing portal work with no setup on any distro,
/// while the programs most likely to be nosy stay out. The clipboard is more
/// sensitive, so it also needs an entry in `clipboard_clients`.
///
/// The executable is read from `/proc/<pid>/exe` rather than `comm`, which
/// truncates to 15 bytes and would collapse every `xdg-desktop-portal-*` binary
/// onto the same string.
///
/// This is a soft boundary and must not be described as more. A same-user
/// process can still run a trusted program and drive it, and there is a narrow
/// race: `socket_peercred` captures the pid at connect time and
/// `/proc/<pid>/exe` is read after, so a peer that exits in between lets a
/// reused pid answer for it. Sandboxed apps are handled properly, through
/// `wp_security_context_v1`, and never reach this function.
pub(crate) fn client_trust(stream: &UnixStream, clipboard_clients: &[String]) -> ClientTrust {
    // `std::os::unix::net::UnixStream::peer_cred` is still nightly-only.
    let Ok(peer) = smithay::reexports::rustix::net::sockopt::socket_peercred(stream) else {
        return ClientTrust::default();
    };
    let Ok(executable) = std::fs::read_link(format!("/proc/{}/exe", peer.pid.as_raw_nonzero()))
    else {
        return ClientTrust::default();
    };
    let privileged = installed_by_root(&executable);
    ClientTrust {
        privileged,
        clipboard: privileged && allowlisted(&executable, clipboard_clients),
    }
}

/// Whether only root could have put this file here: it and every directory
/// above it are owned by root and writable by nobody else.
///
/// A deleted executable reads back as `/path (deleted)`, which does not exist
/// and so is never trusted.
fn installed_by_root(executable: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    const GROUP_OR_OTHER_WRITABLE: u32 = 0o022;
    executable.is_absolute()
        && executable.ancestors().all(|path| {
            std::fs::metadata(path).is_ok_and(|metadata| {
                metadata.uid() == 0 && metadata.mode() & GROUP_OR_OTHER_WRITABLE == 0
            })
        })
}

/// Split from [`client_trust`] so the matching rule is testable without a live
/// socket.
///
/// An entry containing a `/` is matched against the whole resolved path. A bare
/// name matches that basename, which is safe only because [`client_trust`]
/// checks [`installed_by_root`] first. `/proc/<pid>/exe` resolves symlinks, so
/// an entry has to name the real binary rather than a wrapper link.
fn allowlisted(executable: &Path, allowlist: &[String]) -> bool {
    let name = executable.file_name().and_then(|name| name.to_str());
    allowlist.iter().any(|allowed| {
        if allowed.contains('/') {
            Path::new(allowed) == executable
        } else {
            name == Some(allowed.as_str())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_allowlisted_executables_match() {
        let allowlist = ["wl-paste".to_string()];
        assert!(allowlisted(Path::new("/usr/bin/wl-paste"), &allowlist));
        // A prefix of an allowed name is not an allowed name.
        assert!(!allowlisted(Path::new("/usr/bin/wl-past"), &allowlist));
        assert!(!allowlisted(Path::new("/usr/bin/kitty"), &allowlist));

        // A path entry pins down exactly one binary, and mixes with bare names.
        let mixed = ["/usr/bin/cliphist".to_string(), "wl-paste".to_string()];
        assert!(allowlisted(Path::new("/usr/bin/cliphist"), &mixed));
        assert!(!allowlisted(Path::new("/usr/local/bin/cliphist"), &mixed));
        assert!(allowlisted(Path::new("/usr/local/bin/wl-paste"), &mixed));
    }

    #[test]
    fn system_programs_are_installed_by_root() {
        // Present on every distribution Paw2 supports.
        assert!(installed_by_root(Path::new("/usr/bin/env")));
    }

    #[test]
    fn user_writable_programs_are_not_installed_by_root() {
        // The test binary itself lives in the user's build directory.
        let test_binary = std::env::current_exe().unwrap();
        assert!(!installed_by_root(&test_binary));
        // `/tmp` is root-owned but writable by everyone.
        assert!(!installed_by_root(Path::new("/tmp")));
        assert!(!installed_by_root(Path::new("/usr/bin/env (deleted)")));
        assert!(!installed_by_root(Path::new("usr/bin/env")));
    }
}
