use std::collections::HashMap;

use smithay::{
    desktop::{WeakWindow, Window},
    output::{Output, WeakOutput},
    reexports::wayland_server::protocol::wl_shm,
    utils::{Buffer as BufferCoords, Size},
    wayland::{
        compositor::with_states,
        foreign_toplevel_list::{
            ForeignToplevelHandle, ForeignToplevelListHandler, ForeignToplevelListState,
        },
        image_capture_source::{
            ImageCaptureSource, ImageCaptureSourceHandler, OutputCaptureSourceHandler,
            OutputCaptureSourceState, ToplevelCaptureSourceHandler, ToplevelCaptureSourceState,
        },
        image_copy_capture::{
            BufferConstraints, CaptureFailureReason, DmabufConstraints, Frame,
            ImageCopyCaptureHandler, ImageCopyCaptureState, Session, SessionRef,
        },
        shell::xdg::XdgToplevelSurfaceData,
    },
};

use crate::state::Paw2;

#[derive(Debug, Clone)]
pub(crate) enum CaptureTarget {
    Output(WeakOutput),
    Window(WeakWindow),
}

pub struct ScreencopyState {
    pub output_capture_source: OutputCaptureSourceState,
    pub toplevel_capture_source: ToplevelCaptureSourceState,
    pub image_copy_capture: ImageCopyCaptureState,
    pub foreign_toplevel_list: ForeignToplevelListState,
    sessions: Vec<Session>,
    toplevels: HashMap<usize, (WeakWindow, ForeignToplevelHandle)>,
    pending: Vec<PendingCapture>,
}

pub(crate) struct PendingCapture {
    pub target: CaptureTarget,
    pub frame: Frame,
    pub draw_cursor: bool,
}

impl ScreencopyState {
    pub fn new(display: &smithay::reexports::wayland_server::DisplayHandle) -> Self {
        Self {
            output_capture_source: OutputCaptureSourceState::new::<Paw2>(display),
            toplevel_capture_source: ToplevelCaptureSourceState::new::<Paw2>(display),
            image_copy_capture: ImageCopyCaptureState::new::<Paw2>(display),
            foreign_toplevel_list: ForeignToplevelListState::new::<Paw2>(display),
            sessions: Vec::new(),
            toplevels: HashMap::new(),
            pending: Vec::new(),
        }
    }

    pub(crate) fn take_for_output(&mut self, output: &Output) -> Vec<PendingCapture> {
        self.pending
            .extract_if(.., |capture| match &capture.target {
                CaptureTarget::Output(target) => target.upgrade().as_ref() == Some(output),
                CaptureTarget::Window(_) => false,
            })
            .collect()
    }

    pub(crate) fn take_window_captures(&mut self) -> Vec<PendingCapture> {
        self.pending
            .extract_if(.., |capture| {
                matches!(capture.target, CaptureTarget::Window(_))
            })
            .collect()
    }

    pub(crate) fn fail_output(&mut self, output: &Output) {
        for capture in self.take_for_output(output) {
            capture.frame.fail(CaptureFailureReason::Stopped);
        }
    }
}

impl Paw2 {
    pub(crate) fn capture_window_added(&mut self, window: &Window) {
        let (title, app_id) = window_metadata(window);
        let handle = self
            .screencopy_state
            .foreign_toplevel_list
            .new_toplevel::<Self>(title, app_id);
        self.screencopy_state
            .toplevels
            .insert(window.id(), (window.downgrade(), handle));
    }

    pub(crate) fn capture_window_removed(&mut self, window: &Window) {
        if let Some((_, handle)) = self.screencopy_state.toplevels.remove(&window.id()) {
            self.screencopy_state
                .foreign_toplevel_list
                .remove_toplevel(&handle);
        }
        let removed = self
            .screencopy_state
            .pending
            .extract_if(.., |capture| {
                matches!(
                    &capture.target,
                    CaptureTarget::Window(target)
                        if target.upgrade().as_ref() == Some(window)
                )
            })
            .collect::<Vec<_>>();
        for capture in removed {
            capture.frame.fail(CaptureFailureReason::Stopped);
        }
    }

    pub(crate) fn capture_window_metadata_changed(&mut self, window: &Window) {
        let Some((_, handle)) = self.screencopy_state.toplevels.get(&window.id()) else {
            return;
        };
        let (title, app_id) = window_metadata(window);
        handle.send_title(&title);
        handle.send_app_id(&app_id);
        handle.send_done();
    }

    fn capture_constraints_for(&self, target: &CaptureTarget) -> Option<BufferConstraints> {
        let size = match target {
            CaptureTarget::Output(output) => {
                let output = output.upgrade()?;
                output
                    .current_transform()
                    .transform_size(output.current_mode()?.size)
            }
            CaptureTarget::Window(window) => {
                let window = window.upgrade()?;
                let scale = self
                    .capture_output_for_window(&window)
                    .map_or(1.0, |output| output.current_scale().fractional_scale());
                window
                    .bbox_with_popups()
                    .size
                    .to_f64()
                    .to_physical(scale)
                    .to_i32_round()
            }
        };
        if size.w <= 0 || size.h <= 0 {
            return None;
        }
        let backend = self.direct.as_ref()?;
        let formats = backend.capture_formats();
        Some(BufferConstraints {
            size: Size::<i32, BufferCoords>::from((size.w, size.h)),
            shm: capture_shm_formats(),
            dma: (!formats.is_empty()).then(|| DmabufConstraints {
                node: backend.render_node(),
                formats,
            }),
        })
    }
}

fn capture_shm_formats() -> Vec<wl_shm::Format> {
    vec![wl_shm::Format::Xrgb8888, wl_shm::Format::Argb8888]
}

fn window_metadata(window: &Window) -> (String, String) {
    if let Some(surface) = window.toplevel() {
        return with_states(surface.wl_surface(), |states| {
            let attributes = states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap();
            (
                attributes.title.clone().unwrap_or_default(),
                attributes.app_id.clone().unwrap_or_default(),
            )
        });
    }
    window.x11_surface().map_or_else(
        || (String::new(), String::new()),
        |surface| (surface.title(), surface.class()),
    )
}

impl ImageCaptureSourceHandler for Paw2 {}

impl OutputCaptureSourceHandler for Paw2 {
    fn output_capture_source_state(&mut self) -> &mut OutputCaptureSourceState {
        &mut self.screencopy_state.output_capture_source
    }

    fn output_source_created(&mut self, source: ImageCaptureSource, output: &Output) {
        source
            .user_data()
            .insert_if_missing(|| CaptureTarget::Output(output.downgrade()));
    }
}

impl ToplevelCaptureSourceHandler for Paw2 {
    fn toplevel_capture_source_state(&mut self) -> &mut ToplevelCaptureSourceState {
        &mut self.screencopy_state.toplevel_capture_source
    }

    fn toplevel_source_created(
        &mut self,
        source: ImageCaptureSource,
        toplevel: ForeignToplevelHandle,
    ) {
        let window = self
            .screencopy_state
            .toplevels
            .values()
            .find(|(_, handle)| handle.matches(&toplevel))
            .map(|(window, _)| window.clone());
        if let Some(window) = window {
            source
                .user_data()
                .insert_if_missing(|| CaptureTarget::Window(window));
        }
    }
}

impl ForeignToplevelListHandler for Paw2 {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelListState {
        &mut self.screencopy_state.foreign_toplevel_list
    }
}

impl ImageCopyCaptureHandler for Paw2 {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        &mut self.screencopy_state.image_copy_capture
    }

    fn capture_constraints(&mut self, source: &ImageCaptureSource) -> Option<BufferConstraints> {
        self.capture_constraints_for(source.user_data().get::<CaptureTarget>()?)
    }

    fn new_session(&mut self, session: Session) {
        if let Some(constraints) = self.capture_constraints(&session.source()) {
            session.update_constraints(constraints);
            self.screencopy_state.sessions.push(session);
        }
    }

    fn frame(&mut self, session: &SessionRef, frame: Frame) {
        let Some(target) = session.source().user_data().get::<CaptureTarget>().cloned() else {
            frame.fail(CaptureFailureReason::Unknown);
            return;
        };
        self.screencopy_state.pending.push(PendingCapture {
            target,
            frame,
            draw_cursor: session.draw_cursor(),
        });
        crate::drm::redraw(self);
    }

    fn session_destroyed(&mut self, session: SessionRef) {
        self.screencopy_state
            .sessions
            .retain(|owned| owned.as_ref() != session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screenshots_offer_mandatory_8_bit_shm_formats() {
        assert_eq!(
            capture_shm_formats(),
            [wl_shm::Format::Xrgb8888, wl_shm::Format::Argb8888]
        );
    }
}
