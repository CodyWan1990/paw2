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
    input::{Seat, SeatState},
    reexports::{
        calloop::{
            EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction, generic::Generic,
        },
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
        output::OutputManagerState,
        pointer_constraints::PointerConstraintsState,
        presentation::PresentationState,
        relative_pointer::RelativePointerManagerState,
        selection::{
            data_device::DataDeviceState, ext_data_control::DataControlState,
            primary_selection::PrimarySelectionState,
        },
        shell::{
            wlr_layer::WlrLayerShellState,
            xdg::{XdgShellState, dialog::XdgDialogState},
        },
        shm::ShmState,
        single_pixel_buffer::SinglePixelBufferState,
        socket::ListeningSocketSource,
        viewporter::ViewporterState,
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
    pub _xdg_dialog_state: XdgDialogState,
    pub xwayland_shell_state: XWaylandShellState,
    pub xwm: Option<X11Wm>,
    pub layer_shell_state: WlrLayerShellState,
    pub shm_state: ShmState,
    pub _output_manager_state: OutputManagerState,
    pub _idle_inhibit_state: IdleInhibitManagerState,
    pub _presentation_state: PresentationState,
    pub _fifo_manager_state: FifoManagerState,
    pub _fractional_scale_manager_state: FractionalScaleManagerState,
    pub _commit_timing_manager_state: CommitTimingManagerState,
    pub _content_type_state: ContentTypeState,
    pub _cursor_shape_manager_state: CursorShapeManagerState,
    pub drm_syncobj_state: Option<DrmSyncobjState>,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    /// Read by the primary-selection global's visibility filter on every bind,
    /// so `[input] middle_click_paste` takes effect on save. See
    /// [`Paw2::set_middle_click_paste`] for what a live change does not reach.
    primary_selection_enabled: Arc<AtomicBool>,
    pub data_control_state: DataControlState,
    pub _viewporter_state: ViewporterState,
    pub _single_pixel_buffer_state: SinglePixelBufferState,
    pub dmabuf_state: DmabufState,
    pub _alpha_modifier_state: AlphaModifierState,
    pub _background_effect_state: Option<BackgroundEffectState>,
    pub blur_surfaces: Vec<WlSurface>,
    pub cursor_status: smithay::input::pointer::CursorImageStatus,
    pub cursor_position_hint: Option<(WlSurface, Point<f64, Logical>)>,
    pub cursor_defaults: crate::cursor::CursorSettings,
    pub direct: Option<crate::drm::DirectBackend>,
    pub config: Config,
    pub scroller: Scrollers,
    pub suppressed_keys: Vec<smithay::input::keyboard::Keysym>,
    pub wheel_v120_remainder: f64,
    pub pending_wheel_action: Option<Action>,
    pub workspace_state: WorkspaceState,
    pub screencopy_state: ScreencopyState,
    pub ipc: IpcState,
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
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let xdg_dialog_state = XdgDialogState::new::<Self>(&display_handle);
        let xwayland_shell_state = XWaylandShellState::new::<Self>(&display_handle);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
        let idle_inhibit_state = IdleInhibitManagerState::new::<Self>(&display_handle);
        let presentation_state =
            PresentationState::new::<Self>(&display_handle, Monotonic::ID as u32);
        let fifo_manager_state = FifoManagerState::new::<Self>(&display_handle);
        let fractional_scale_manager_state =
            FractionalScaleManagerState::new::<Self>(&display_handle);
        let commit_timing_manager_state = CommitTimingManagerState::new::<Self>(&display_handle);
        let content_type_state = ContentTypeState::new::<Self>(&display_handle);
        let cursor_shape_manager_state = CursorShapeManagerState::new::<Self>(&display_handle);
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
        // Clipboard managers read every copy the moment it happens, so this sits
        // behind the same allowlist as the window list rather than being there
        // for the taking. The default allowlist covers the usual wl-clipboard
        // pairing; another manager needs its own entry in [security].
        let data_control_state = DataControlState::new::<Self, _>(
            &display_handle,
            middle_click_paste.then_some(&primary_selection_state),
            is_privileged,
        );
        // Both are handled entirely inside smithay's surface renderer, which
        // Paw2 already goes through; neither needs anything of its own here.
        let viewporter_state = ViewporterState::new::<Self>(&display_handle);
        let single_pixel_buffer_state = SinglePixelBufferState::new::<Self>(&display_handle);
        let alpha_modifier_state = AlphaModifierState::new::<Self>(&display_handle);
        let workspace_state = WorkspaceState::new(&display_handle);
        let screencopy_state = ScreencopyState::new(&display_handle);

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&display_handle, "paw2");
        seat.add_keyboard(Default::default(), 200, 25).unwrap();
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
            _xdg_dialog_state: xdg_dialog_state,
            xwayland_shell_state,
            xwm: None,
            layer_shell_state,
            shm_state,
            _output_manager_state: output_manager_state,
            _idle_inhibit_state: idle_inhibit_state,
            _presentation_state: presentation_state,
            _fifo_manager_state: fifo_manager_state,
            _fractional_scale_manager_state: fractional_scale_manager_state,
            _commit_timing_manager_state: commit_timing_manager_state,
            _content_type_state: content_type_state,
            _cursor_shape_manager_state: cursor_shape_manager_state,
            drm_syncobj_state: None,
            seat_state,
            data_device_state,
            primary_selection_state,
            primary_selection_enabled,
            data_control_state,
            _viewporter_state: viewporter_state,
            _single_pixel_buffer_state: single_pixel_buffer_state,
            dmabuf_state: DmabufState::new(),
            _alpha_modifier_state: alpha_modifier_state,
            _background_effect_state: None,
            blur_surfaces: Vec::new(),
            cursor_status: smithay::input::pointer::CursorImageStatus::default_named(),
            cursor_position_hint: None,
            cursor_defaults,
            direct: None,
            config,
            scroller: Scrollers::default(),
            suppressed_keys: Vec::new(),
            wheel_v120_remainder: 0.0,
            pending_wheel_action: None,
            workspace_state,
            screencopy_state,
            ipc: IpcState::default(),
        }
    }

    /// Every exit path goes through here so shutdown behaves the same whether it
    /// was a keybinding, a lost device, or a closed nested window.
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
        if self._background_effect_state.is_none() && self.config.appearance.blur_enabled() {
            self._background_effect_state =
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
                let privileged =
                    is_privileged_client(&stream, &state.config.security.privileged_clients);
                // Refusing one client must never end the session: hitting the
                // open-file limit is recoverable, losing every running program is not.
                if let Err(error) = state
                    .display_handle
                    .insert_client(stream, Arc::new(ClientState::new(privileged)))
                {
                    tracing::warn!(?error, "failed to accept Wayland client");
                }
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
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
    /// Whether this client may bind the globals that expose other clients'
    /// windows. See [`is_privileged_client`] for what that is worth.
    pub privileged: bool,
}

impl ClientState {
    fn new(privileged: bool) -> Self {
        Self {
            privileged,
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
/// `wf-recorder` keep working with no allowlist entry.
pub fn is_privileged(client: &Client) -> bool {
    client
        .get_data::<ClientState>()
        .is_some_and(|state| state.privileged)
}

/// Whether a connecting client may bind the privileged globals.
///
/// Reads the peer's executable rather than `/proc/<pid>/comm`, which truncates
/// to 15 bytes and would collapse every `xdg-desktop-portal-*` binary onto the
/// same string.
///
/// This is a soft boundary and must not be described as more. A process running
/// as the user can already ptrace the compositor, and the peer credentials
/// themselves are spoofable. What it buys is that an ordinary application can no
/// longer enumerate every window title just by binding a global that was there
/// for the taking.
fn is_privileged_client(stream: &UnixStream, allowlist: &[String]) -> bool {
    // `std::os::unix::net::UnixStream::peer_cred` is still nightly-only.
    let Ok(peer) = smithay::reexports::rustix::net::sockopt::socket_peercred(stream) else {
        return false;
    };
    let Ok(executable) = std::fs::read_link(format!("/proc/{}/exe", peer.pid.as_raw_nonzero()))
    else {
        return false;
    };
    allowlisted(&executable, allowlist)
}

/// Split from [`is_privileged_client`] so the matching rule is testable without
/// a live socket.
fn allowlisted(executable: &Path, allowlist: &[String]) -> bool {
    executable
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| allowlist.iter().any(|allowed| allowed == name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_allowlisted_executables_are_privileged() {
        let allowlist = ["xdg-desktop-portal-wlr".to_string()];
        assert!(allowlisted(
            Path::new("/usr/lib/xdg-desktop-portal-wlr"),
            &allowlist
        ));
        // Matched on the basename, so the install prefix does not matter.
        assert!(allowlisted(
            Path::new("/usr/local/libexec/xdg-desktop-portal-wlr"),
            &allowlist
        ));
        // A prefix of an allowed name is not an allowed name.
        assert!(!allowlisted(
            Path::new("/usr/lib/xdg-desktop-portal"),
            &allowlist
        ));
        assert!(!allowlisted(Path::new("/usr/bin/kitty"), &allowlist));
        // An empty allowlist privileges nobody.
        assert!(!allowlisted(
            Path::new("/usr/lib/xdg-desktop-portal-wlr"),
            &[]
        ));
    }
}
