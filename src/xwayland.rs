use std::{io, os::fd::OwnedFd, process::Stdio};

use smithay::{
    reexports::{calloop::LoopHandle, wayland_server::protocol::wl_surface::WlSurface},
    utils::{Logical, Rectangle},
    wayland::{
        selection::{
            SelectionTarget,
            data_device::{
                clear_data_device_selection, current_data_device_selection_userdata,
                request_data_device_client_selection, set_data_device_selection,
            },
            primary_selection::{
                clear_primary_selection, current_primary_selection_userdata,
                request_primary_client_selection, set_primary_selection,
            },
        },
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler,
        xwm::{Reorder, ResizeEdge, WmWindowProperty, XwmId},
    },
};

use crate::state::Paw2;

pub fn start(handle: LoopHandle<'static, Paw2>, state: &mut Paw2) -> io::Result<u32> {
    let (xwayland, client) = XWayland::spawn(
        &state.display_handle,
        None,
        std::iter::empty::<(String, String)>(),
        ["-nolisten", "tcp"],
        false,
        Stdio::null(),
        Stdio::null(),
        |_| (),
    )?;
    let display_number = xwayland.display_number();
    let display = state.display_handle.clone();
    // A strong `LoopHandle` stored in a source callback is a reference cycle that
    // leaks the whole event loop, and with it XWayland and the libseat session.
    let xwm_handle = handle.downgrade();
    handle
        .insert_source(xwayland, move |event, _, state| match event {
            XWaylandEvent::Ready { x11_socket, .. } => {
                let Some(loop_handle) = xwm_handle.upgrade() else {
                    return;
                };
                match X11Wm::start_wm(loop_handle, &display, x11_socket, client.clone()) {
                    Ok(xwm) => {
                        state.xwm = Some(xwm);
                        tracing::info!(display = display_number, "XWayland ready");
                    }
                    Err(error) => {
                        tracing::error!(?error, "failed to start XWayland window manager")
                    }
                }
            }
            XWaylandEvent::Error => tracing::error!("XWayland exited during startup"),
        })
        .map_err(io::Error::other)?;
    Ok(display_number)
}

impl XWaylandShellHandler for Paw2 {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    fn surface_associated(&mut self, _xwm: XwmId, _wl_surface: WlSurface, _surface: X11Surface) {
        self.sync_focus();
        crate::drm::redraw(self);
    }
}

impl XwmHandler for Paw2 {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().unwrap()
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Err(error) = window.set_mapped(true) {
            tracing::warn!(?error, "failed to map X11 window");
            return;
        }
        self.add_x11_window(window);
        crate::drm::redraw(self);
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.add_x11_override(window);
        crate::drm::redraw(self);
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.remove_x11_window(&window);
        if !window.is_override_redirect() {
            let _ = window.set_mapped(false);
        }
        crate::drm::redraw(self);
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.remove_x11_window(&window);
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        width: Option<u32>,
        height: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        if window.is_override_redirect() {
            let mut geometry = window.last_configure();
            geometry.loc.x = x.unwrap_or(geometry.loc.x);
            geometry.loc.y = y.unwrap_or(geometry.loc.y);
            geometry.size.w = width.map_or(geometry.size.w, |value| value as i32);
            geometry.size.h = height.map_or(geometry.size.h, |value| value as i32);
            let _ = window.configure(geometry);
        } else {
            self.configure_x11_window(&window, width, height);
        }
    }

    fn property_notify(&mut self, _xwm: XwmId, window: X11Surface, property: WmWindowProperty) {
        if matches!(property, WmWindowProperty::Title | WmWindowProperty::Class)
            && let Some(window) = self.window_for_x11_surface(&window).cloned()
        {
            self.capture_window_metadata_changed(&window);
        }
        if matches!(
            property,
            WmWindowProperty::NormalHints
                | WmWindowProperty::TransientFor
                | WmWindowProperty::WindowType
        ) {
            self.refresh_x11_floating(&window);
            crate::drm::redraw(self);
        }
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        if window.is_override_redirect() {
            self.configure_x11_override(&window, geometry.loc);
            crate::drm::redraw(self);
        }
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.set_x11_fullscreen(&window);
        crate::drm::redraw(self);
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.unset_x11_fullscreen(&window);
        crate::drm::redraw(self);
    }

    fn modal_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.refresh_x11_floating(&window);
        crate::drm::redraw(self);
    }

    fn unmodal_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.refresh_x11_floating(&window);
        crate::drm::redraw(self);
    }

    fn resize_request(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _button: u32,
        _edge: ResizeEdge,
    ) {
    }

    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {}

    fn active_window_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        _timestamp: u32,
        _currently_active: Option<X11Surface>,
    ) {
        if let Some(window) = self.window_for_x11_surface(&window).cloned() {
            self.focus_window(&window);
        }
    }

    fn allow_selection_access(&mut self, _xwm: XwmId, selection: SelectionTarget) -> bool {
        if selection == SelectionTarget::Primary && !self.config.input.middle_click_paste {
            return false;
        }
        self.seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus())
            .and_then(|surface| self.window_for_surface(&surface))
            .is_some_and(|window| window.x11_surface().is_some())
    }

    fn send_selection(
        &mut self,
        _xwm: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
    ) {
        let failed = match selection {
            SelectionTarget::Clipboard => {
                request_data_device_client_selection(&self.seat, mime_type, fd).is_err()
            }
            SelectionTarget::Primary => {
                request_primary_client_selection(&self.seat, mime_type, fd).is_err()
            }
        };
        if failed {
            tracing::warn!("failed to send Wayland selection to X11");
        }
    }

    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        match selection {
            SelectionTarget::Clipboard => {
                set_data_device_selection(&self.display_handle, &self.seat, mime_types, ())
            }
            SelectionTarget::Primary if self.config.input.middle_click_paste => {
                set_primary_selection(&self.display_handle, &self.seat, mime_types, ())
            }
            SelectionTarget::Primary => {}
        }
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        match selection {
            SelectionTarget::Clipboard
                if current_data_device_selection_userdata(&self.seat).is_some() =>
            {
                clear_data_device_selection(&self.display_handle, &self.seat)
            }
            SelectionTarget::Primary
                if current_primary_selection_userdata(&self.seat).is_some() =>
            {
                clear_primary_selection(&self.display_handle, &self.seat)
            }
            _ => {}
        }
    }

    fn disconnected(&mut self, _xwm: XwmId) {
        self.xwm = None;
        tracing::warn!("XWayland window manager disconnected");
    }
}
