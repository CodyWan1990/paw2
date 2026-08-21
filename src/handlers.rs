use smithay::{
    backend::{
        allocator::dmabuf::Dmabuf,
        renderer::{ImportDma, element::Id, utils::on_commit_buffer_handler},
    },
    desktop::{
        LayerSurface, PopupKind, WindowSurfaceType, find_popup_root_surface,
        get_popup_toplevel_coords, layer_map_for_output,
    },
    input::{
        Seat, SeatHandler, SeatState,
        dnd::{DnDGrab, DndGrabHandler, GrabType, Source},
        pointer::{Focus, PointerHandle},
    },
    output::Output,
    reexports::{
        calloop::Interest,
        wayland_server::{
            Client, Resource,
            protocol::{wl_buffer, wl_output::WlOutput, wl_seat, wl_surface::WlSurface},
        },
    },
    utils::{Logical, Point, Rectangle, Serial},
    wayland::{
        background_effect::ExtBackgroundEffectHandler,
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            SurfaceAttributes, add_blocker, add_pre_commit_hook, get_parent, is_sync_subsurface,
            with_states,
        },
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier, get_dmabuf},
        drm_syncobj::{DrmSyncobjCachedState, DrmSyncobjHandler, DrmSyncobjState},
        idle_inhibit::IdleInhibitHandler,
        output::OutputHandler,
        pointer_constraints::{
            PointerConstraint, PointerConstraintsHandler, with_pointer_constraint,
        },
        selection::{
            SelectionHandler,
            data_device::{
                DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
            },
            primary_selection::{
                PrimarySelectionHandler, PrimarySelectionState, set_primary_focus,
            },
        },
        shell::wlr_layer::{
            Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData, WlrLayerShellHandler,
            WlrLayerShellState,
        },
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            dialog::{ToplevelDialogHint, XdgDialogHandler},
        },
        shm::{ShmHandler, ShmState},
    },
    xwayland::XWaylandClientData,
};

use crate::state::{ClientState, Paw2};

impl CompositorHandler for Paw2 {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        if let Some(state) = client.get_data::<XWaylandClientData>() {
            &state.compositor_state
        } else {
            &client.get_data::<ClientState>().unwrap().compositor_state
        }
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        add_pre_commit_hook::<Self, _>(surface, |state, _, surface| {
            let (dmabuf, acquire_point) = with_states(surface, |states| {
                let dmabuf = states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .pending()
                    .buffer
                    .as_ref()
                    .and_then(|assignment| match assignment {
                        BufferAssignment::NewBuffer(buffer) => get_dmabuf(buffer).cloned().ok(),
                        _ => None,
                    });
                let acquire_point = states
                    .cached_state
                    .get::<DrmSyncobjCachedState>()
                    .pending()
                    .acquire_point
                    .clone();
                (dmabuf, acquire_point)
            });
            let Some(dmabuf) = dmabuf else {
                return;
            };
            let Some(client) = surface.client() else {
                return;
            };
            // An explicit acquire point is preferred; the buffer's implicit fence
            // is the fallback when there is none or its source cannot be added.
            if let Some(acquire_point) = acquire_point
                && let Ok((blocker, source)) = acquire_point.generate_blocker()
                && unblock_on(state, client.clone(), source)
            {
                add_blocker(surface, blocker);
                return;
            }
            if let Ok((blocker, source)) = dmabuf.generate_blocker(Interest::READ)
                && unblock_on(state, client, source)
            {
                add_blocker(surface, blocker);
            }
        });
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = self.window_for_surface(&root).cloned() {
                window.on_commit();
                if let Some(toplevel) = window.toplevel() {
                    self.refresh_wayland_floating(toplevel);
                }
            }
        }

        if let Some(window) = self.window_for_surface(surface)
            && let Some(toplevel) = window.toplevel()
            && !toplevel.is_initial_configure_sent()
        {
            toplevel.send_configure();
        }

        self.popups.commit(surface);
        if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(surface)
            && !popup.is_initial_configure_sent()
        {
            self.unconstrain_popup(&popup);
            if let Err(error) = popup.send_configure() {
                tracing::warn!(?error, "initial popup configure failed");
            }
        }

        let is_layer_surface = with_states(surface, |states| {
            states.data_map.get::<LayerSurfaceData>().is_some()
        });
        if is_layer_surface {
            for output in self.outputs() {
                let mut map = layer_map_for_output(&output);
                if map
                    .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                    .is_none()
                {
                    continue;
                }
                let initial_configure_sent = with_states(surface, |states| {
                    states
                        .data_map
                        .get::<LayerSurfaceData>()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .initial_configure_sent
                });
                let previous_zone = map.non_exclusive_zone();
                map.arrange();
                let zone_changed = previous_zone != map.non_exclusive_zone();
                if !initial_configure_sent {
                    map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                        .unwrap()
                        .layer_surface()
                        .send_configure();
                }
                drop(map);
                if zone_changed {
                    self.relayout(true);
                }
                break;
            }
            self.sync_focus();
        }
        crate::drm::redraw(self);
    }
}

/// Clears the client's commit blocker once `source` fires. Reports whether the
/// source was accepted, since an unusable source must not leave a blocker behind.
fn unblock_on<S, E>(state: &mut Paw2, client: Client, source: S) -> bool
where
    S: smithay::reexports::calloop::EventSource<Ret = Result<(), E>> + 'static,
{
    state
        .loop_handle
        .insert_source(source, move |_, _, state| {
            let display_handle = state.display_handle.clone();
            state
                .client_compositor_state(&client)
                .blocker_cleared(state, &display_handle);
            Ok(())
        })
        .is_ok()
}

impl BufferHandler for Paw2 {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl DmabufHandler for Paw2 {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        let imported = self
            .direct
            .as_mut()
            .is_some_and(|backend| backend.renderer.import_dmabuf(&dmabuf, None).is_ok());
        if imported {
            let _ = notifier.successful::<Self>();
        } else {
            notifier.failed();
        }
    }
}

impl ShmHandler for Paw2 {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl DrmSyncobjHandler for Paw2 {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        self.drm_syncobj_state.as_mut()
    }
}

impl Paw2 {
    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let popup_kind = PopupKind::Xdg(popup.clone());
        let Ok(root) = find_popup_root_surface(&popup_kind) else {
            return;
        };
        let popup_offset = get_popup_toplevel_coords(&popup_kind);
        let root_id = Id::from_wayland_resource(&root);

        for output in self.outputs() {
            let Some(output_geometry) = self.output_geometry(&output) else {
                continue;
            };
            let window_location = self
                .window_render_state(&output, &root_id)
                .map(|(geometry, _)| output_geometry.loc + geometry.loc);
            let layer_location = || {
                let layers = layer_map_for_output(&output);
                let layer = layers.layer_for_surface(&root, WindowSurfaceType::TOPLEVEL)?;
                Some(output_geometry.loc + layers.layer_geometry(layer)?.loc)
            };
            let Some(root_location) = window_location.or_else(layer_location) else {
                continue;
            };
            let target = popup_constraint_geometry(output_geometry, root_location, popup_offset);
            popup.with_pending_state(|state| {
                let geometry = state.positioner.get_unconstrained_geometry(target);
                state.geometry = fit_popup_geometry(geometry, target);
            });
            return;
        }
    }
}

fn popup_constraint_geometry(
    mut output: Rectangle<i32, Logical>,
    root_location: Point<i32, Logical>,
    popup_offset: Point<i32, Logical>,
) -> Rectangle<i32, Logical> {
    output.loc -= root_location + popup_offset;
    output
}

fn fit_popup_geometry(
    mut popup: Rectangle<i32, Logical>,
    target: Rectangle<i32, Logical>,
) -> Rectangle<i32, Logical> {
    popup.size.w = popup.size.w.min(target.size.w);
    popup.size.h = popup.size.h.min(target.size.h);
    popup.loc.x = popup
        .loc
        .x
        .clamp(target.loc.x, target.loc.x + target.size.w - popup.size.w);
    popup.loc.y = popup
        .loc
        .y
        .clamp(target.loc.y, target.loc.y + target.size.h - popup.size.h);
    popup
}

impl XdgShellHandler for Paw2 {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.add_window(surface);
    }

    fn parent_changed(&mut self, surface: ToplevelSurface) {
        self.refresh_wayland_floating(&surface);
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.window_for_surface(surface.wl_surface()).cloned() {
            self.capture_window_metadata_changed(&window);
        }
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.window_for_surface(surface.wl_surface()).cloned() {
            self.capture_window_metadata_changed(&window);
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.remove_window(surface.wl_surface());
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn move_request(&mut self, _surface: ToplevelSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
    }

    fn resize_request(
        &mut self,
        _surface: ToplevelSurface,
        _seat: wl_seat::WlSeat,
        _serial: Serial,
        _edges: smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge,
    ) {
    }

    fn fullscreen_request(&mut self, surface: ToplevelSurface, output: Option<WlOutput>) {
        let output = output.as_ref().and_then(Output::from_resource);
        self.set_fullscreen(&surface, output);
        crate::drm::redraw(self);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.unset_fullscreen(&surface);
        crate::drm::redraw(self);
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}
}

impl XdgDialogHandler for Paw2 {
    fn dialog_hint_changed(&mut self, surface: ToplevelSurface, _hint: ToplevelDialogHint) {
        self.refresh_wayland_floating(&surface);
    }
}

impl SeatHandler for Paw2 {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        image: smithay::input::pointer::CursorImageStatus,
    ) {
        self.cursor_status = image;
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(|surface| self.display_handle.get_client(surface.id()).ok());
        set_data_device_focus(&self.display_handle, seat, client.clone());
        set_primary_focus(&self.display_handle, seat, client);
    }
}

impl PointerConstraintsHandler for Paw2 {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        if pointer.current_focus().as_ref() == Some(surface) {
            with_pointer_constraint(surface, pointer, |constraint| {
                if let Some(constraint) = constraint {
                    constraint.activate();
                }
            });
        }
    }

    fn remove_constraint(
        &mut self,
        _surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        _constraint: Option<&PointerConstraint>,
    ) {
        let Some((surface, hint)) = self.cursor_position_hint.take() else {
            return;
        };
        if pointer.last_enter().is_some()
            && let Some(origin) = self.window_location(&surface)
        {
            pointer.set_location(origin.to_f64() + hint);
        }
    }

    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        location: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) {
        if with_pointer_constraint(surface, pointer, |constraint| {
            constraint.is_some_and(|constraint| constraint.is_active())
        }) {
            self.cursor_position_hint = Some((surface.clone(), location));
        }
    }
}

impl IdleInhibitHandler for Paw2 {
    // The protocol stays advertised so clients can inhibit, but Paw2 has no idle
    // or DPMS timer for the inhibitors to suppress; there is nothing to record.
    fn inhibit(&mut self, _surface: WlSurface) {}

    fn uninhibit(&mut self, _surface: WlSurface) {}
}
impl OutputHandler for Paw2 {
    fn output_bound(&mut self, output: smithay::output::Output, wl_output: WlOutput) {
        self.workspace_state.output_bound(&output, &wl_output);
    }
}
impl WlrLayerShellHandler for Paw2 {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        let output = output
            .as_ref()
            .and_then(smithay::output::Output::from_resource)
            .or_else(|| self.default_output());
        let Some(output) = output else {
            surface.send_close();
            return;
        };
        if let Err(error) =
            layer_map_for_output(&output).map_layer(&LayerSurface::new(surface.clone(), namespace))
        {
            tracing::warn!(?error, "failed to map layer surface");
            surface.send_close();
        }
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        for output in self.outputs() {
            let mut map = layer_map_for_output(&output);
            let layer = map
                .layers()
                .find(|layer| layer.layer_surface() == &surface)
                .cloned();
            if let Some(layer) = layer {
                let previous_zone = map.non_exclusive_zone();
                map.unmap_layer(&layer);
                let zone_changed = previous_zone != map.non_exclusive_zone();
                drop(map);
                if zone_changed {
                    self.relayout(true);
                }
                self.sync_focus();
                crate::drm::redraw(self);
                break;
            }
        }
    }
}
impl ExtBackgroundEffectHandler for Paw2 {
    fn set_blur_region(
        &mut self,
        surface: WlSurface,
        _region: smithay::wayland::compositor::RegionAttributes,
    ) {
        if !self.blur_surfaces.contains(&surface) {
            self.blur_surfaces.push(surface);
        }
    }
}
impl SelectionHandler for Paw2 {
    type SelectionUserData = ();
}
impl DataDeviceHandler for Paw2 {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}
impl PrimarySelectionHandler for Paw2 {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
    }
}
impl DndGrabHandler for Paw2 {}
impl WaylandDndGrabHandler for Paw2 {
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        _icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: Serial,
        grab_type: GrabType,
    ) {
        match grab_type {
            GrabType::Pointer => {
                let pointer = seat.get_pointer().unwrap();
                let start_data = pointer.grab_start_data().unwrap();
                let grab = DnDGrab::new_pointer(&self.display_handle, start_data, source, seat);
                pointer.set_grab(self, grab, serial, Focus::Keep);
            }
            GrabType::Touch => source.cancel(),
        }
    }
}

smithay::delegate_dispatch2!(Paw2);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn popup_constraints_use_global_output_and_root_positions() {
        let output = Rectangle::new((2560, 0).into(), (2560, 1440).into());
        let target = popup_constraint_geometry(output, (5000, 20).into(), (50, 30).into());
        assert_eq!(
            target,
            Rectangle::new((-2490, -50).into(), (2560, 1440).into())
        );
    }

    #[test]
    fn popups_are_kept_inside_the_output() {
        let target = Rectangle::new((-2500, -30).into(), (2560, 1440).into());
        let popup = Rectangle::new((-100, 20).into(), (400, 500).into());
        assert_eq!(
            fit_popup_geometry(popup, target),
            Rectangle::new((-340, 20).into(), (400, 500).into())
        );
    }
}
