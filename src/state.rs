use std::{ffi::OsString, sync::Arc, time::Instant};

use smithay::{
    desktop::PopupManager,
    input::{Seat, SeatState},
    reexports::{
        calloop::{
            EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction, generic::Generic,
        },
        wayland_server::{
            Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{Clock, ClockSource, Logical, Monotonic, Point},
    wayland::{
        alpha_modifier::AlphaModifierState,
        background_effect::BackgroundEffectState,
        compositor::{CompositorClientState, CompositorState},
        dmabuf::DmabufState,
        idle_inhibit::IdleInhibitManagerState,
        output::OutputManagerState,
        pointer_constraints::PointerConstraintsState,
        presentation::PresentationState,
        relative_pointer::RelativePointerManagerState,
        selection::{data_device::DataDeviceState, primary_selection::PrimarySelectionState},
        shell::{
            wlr_layer::WlrLayerShellState,
            xdg::{XdgShellState, dialog::XdgDialogState},
        },
        shm::ShmState,
        socket::ListeningSocketSource,
        xwayland_shell::XWaylandShellState,
    },
    xwayland::X11Wm,
};

use crate::{
    config::Config, layout::Scrollers, screencopy::ScreencopyState, workspace::WorkspaceState,
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
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub dmabuf_state: DmabufState,
    pub _alpha_modifier_state: AlphaModifierState,
    pub _background_effect_state: Option<BackgroundEffectState>,
    pub blur_surfaces: Vec<WlSurface>,
    pub cursor_status: smithay::input::pointer::CursorImageStatus,
    pub cursor_position_hint: Option<(WlSurface, Point<f64, Logical>)>,
    pub idle_inhibitors: Vec<WlSurface>,
    pub direct: Option<crate::drm::DirectBackend>,
    pub config: Config,
    pub scroller: Scrollers,
    pub suppressed_keys: Vec<smithay::input::keyboard::Keysym>,
    pub workspace_state: WorkspaceState,
    pub screencopy_state: ScreencopyState,
}

impl Paw2 {
    pub fn new(
        event_loop: &mut EventLoop<'static, Self>,
        display: Display<Self>,
        config: Config,
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
        RelativePointerManagerState::new::<Self>(&display_handle);
        PointerConstraintsState::new::<Self>(&display_handle);
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);
        let middle_click_paste = config.input.middle_click_paste;
        let primary_selection_state =
            PrimarySelectionState::new_with_filter::<Self, _>(&display_handle, move |_| {
                middle_click_paste
            });
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
            seat_state,
            data_device_state,
            primary_selection_state,
            dmabuf_state: DmabufState::new(),
            _alpha_modifier_state: alpha_modifier_state,
            _background_effect_state: None,
            blur_surfaces: Vec::new(),
            cursor_status: smithay::input::pointer::CursorImageStatus::default_named(),
            cursor_position_hint: None,
            idle_inhibitors: Vec::new(),
            direct: None,
            config,
            scroller: Scrollers::default(),
            suppressed_keys: Vec::new(),
            workspace_state,
            screencopy_state,
        }
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
                state
                    .display_handle
                    .insert_client(stream, Arc::new(ClientState::default()))
                    .expect("failed to accept Wayland client");
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
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
