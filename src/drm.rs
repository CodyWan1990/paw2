use std::{io, path::PathBuf, time::Duration};

use smithay::{
    backend::{
        allocator::{
            Fourcc, Modifier,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata, DrmEventTime, DrmNode, VrrSupport,
            compositor::{FrameError, FrameFlags, PrimaryPlaneElement},
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
        },
        egl::{EGLContext, EGLDisplay, context::ContextPriority},
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            Bind, ImportDma, ImportEgl, ImportMemWl,
            damage::OutputDamageTracker,
            element::{
                AsRenderElements, Kind,
                memory::MemoryRenderBufferRenderElement,
                solid::{SolidColorBuffer, SolidColorRenderElement},
                surface::WaylandSurfaceRenderElement,
                utils::{CropRenderElement, Relocate, RelocateRenderElement},
            },
            gles::GlesRenderer,
        },
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{UdevBackend, UdevEvent, primary_gpu},
    },
    desktop::{space::SurfaceTree, utils::OutputPresentationFeedback},
    input::pointer::{CursorImageStatus, CursorImageSurfaceData},
    output::{Mode, Output, PhysicalProperties},
    reexports::{
        calloop::{
            EventLoop, LoopHandle,
            timer::{TimeoutAction, Timer},
        },
        drm::control::{Mode as DrmMode, ModeTypeFlags, connector, crtc},
        input::Libinput,
        rustix::fs::OFlags,
        wayland_protocols::wp::presentation_time::server::wp_presentation_feedback,
        wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_frame_v1::Flags,
        wayland_server::backend::GlobalId,
    },
    utils::{DeviceFd, IsAlive, Logical, Monotonic, Point, Rectangle, Scale, Time, Transform},
    wayland::{
        compositor,
        dmabuf::DmabufFeedbackBuilder,
        drm_syncobj::{DrmSyncobjState, supports_syncobj_eventfd},
        presentation::Refresh,
    },
};
use smithay_drm_extras::{display_info, drm_scanner::DrmScanner};
use tracing::{debug, error, info, warn};

use crate::{
    config::{MonitorConfig, VrrPolicy},
    render::{Effects, PawRenderElements, clear_color},
    state::Paw2,
};

type Allocator = GbmAllocator<DrmDeviceFd>;
type Exporter = GbmFramebufferExporter<DrmDeviceFd>;
type FrameFeedback = OutputPresentationFeedback;
type KmsOutput = DrmOutput<Allocator, Exporter, FrameFeedback, DrmDeviceFd>;
type OutputManager = DrmOutputManager<Allocator, Exporter, FrameFeedback, DrmDeviceFd>;

struct ActiveOutput {
    connector: connector::Handle,
    crtc: crtc::Handle,
    output: Output,
    global: GlobalId,
    kms: KmsOutput,
    frame_pending: bool,
    repaint_scheduled: bool,
    direct_scanout_active: bool,
    vrr_policy: VrrPolicy,
    vrr_support: VrrSupport,
    vrr_requested: Option<bool>,
    vrr_active: bool,
}

pub struct DirectBackend {
    handle: LoopHandle<'static, Paw2>,
    _session: LibSeatSession,
    node: DrmNode,
    manager: OutputManager,
    scanner: DrmScanner,
    pub renderer: GlesRenderer,
    outputs: Vec<ActiveOutput>,
    cursor: Option<crate::cursor::Cursor>,
    cursor_fallback: SolidColorBuffer,
    effects: Effects,
}

pub fn init(
    event_loop: &mut EventLoop<'static, Paw2>,
    state: &mut Paw2,
) -> Result<(), Box<dyn std::error::Error>> {
    let (mut session, session_notifier) = LibSeatSession::new()?;
    let path = select_device(&session, state.config.backend.drm_device.clone())?;
    let node = DrmNode::from_path(&path)?;
    info!(device = %path.display(), "using DRM device");

    let fd = session.open(
        &path,
        OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
    )?;
    let fd = DrmDeviceFd::new(DeviceFd::from(fd));
    let (device, drm_notifier) = DrmDevice::new(fd.clone(), true)?;
    if supports_syncobj_eventfd(&fd) {
        state.drm_syncobj_state = Some(DrmSyncobjState::new::<Paw2>(
            &state.display_handle,
            fd.clone(),
        ));
        info!("enabled linux-drm-syncobj explicit synchronization");
    }
    let gbm = GbmDevice::new(fd)?;
    let display = unsafe { EGLDisplay::new(gbm.clone())? };
    let context = EGLContext::new_with_priority(&display, ContextPriority::High)?;
    let mut renderer = unsafe { GlesRenderer::new(context)? };
    if let Err(error) = renderer.bind_wl_display(&state.display_handle) {
        warn!(?error, "legacy EGL Wayland buffer support unavailable");
    }
    state.shm_state.update_formats(renderer.shm_formats());
    let effects = Effects::new(&mut renderer, state.config.appearance);
    if effects.blur_available() {
        state.enable_background_effects();
    }

    let formats = renderer
        .egl_context()
        .dmabuf_render_formats()
        .iter()
        .filter(|format| format.modifier != Modifier::Invalid || format.code == Fourcc::Argb8888)
        .copied()
        .collect::<Vec<_>>();
    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    let exporter = GbmFramebufferExporter::new(gbm.clone(), node.into());
    let manager = DrmOutputManager::new(
        device,
        allocator,
        exporter,
        Some(gbm),
        [Fourcc::Argb8888, Fourcc::Abgr8888],
        formats,
    );

    let cursor = crate::cursor::Cursor::load()
        .map_err(|error| warn!(?error, "could not load cursor theme; using fallback cursor"))
        .ok();
    let mut backend = DirectBackend {
        handle: event_loop.handle(),
        _session: session.clone(),
        node,
        manager,
        scanner: DrmScanner::new(),
        renderer,
        outputs: Vec::new(),
        cursor,
        cursor_fallback: SolidColorBuffer::new((12, 12), [1.0, 1.0, 1.0, 1.0]),
        effects,
    };
    backend.rescan(state)?;
    if backend.outputs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no matching connected DRM connector",
        )
        .into());
    }

    let dmabuf_formats = backend.renderer.dmabuf_formats();
    let dmabuf_feedback = DmabufFeedbackBuilder::new(node.dev_id(), dmabuf_formats)
        .build()
        .map_err(io::Error::other)?;
    state
        .dmabuf_state
        .create_global_with_default_feedback::<Paw2>(&state.display_handle, &dmabuf_feedback);
    state.direct = Some(backend);

    let mut libinput =
        Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.clone().into());
    libinput
        .udev_assign_seat(&session.seat())
        .map_err(|()| io::Error::other("failed to assign libinput seat"))?;
    event_loop.handle().insert_source(
        LibinputInputBackend::new(libinput.clone()),
        |event: InputEvent<LibinputInputBackend>, _, state| {
            state.process_input(event);
            redraw(state);
        },
    )?;

    event_loop
        .handle()
        .insert_source(session_notifier, move |event, _, state| {
            with_backend(state, |backend, _| match event {
                SessionEvent::PauseSession => {
                    info!("session paused");
                    libinput.suspend();
                    backend.manager.pause();
                }
                SessionEvent::ActivateSession => {
                    info!("session resumed");
                    if let Err(error) = libinput.resume() {
                        error!(?error, "failed to resume libinput");
                    }
                    if let Err(error) = backend.manager.lock().activate(false) {
                        error!(?error, "failed to reactivate DRM");
                    }
                    for output in &mut backend.outputs {
                        output.kms.reset_buffers();
                        output.frame_pending = false;
                        output.repaint_scheduled = false;
                    }
                }
            });
        })?;

    let render_handle = event_loop.handle();
    event_loop
        .handle()
        .insert_source(drm_notifier, move |event, metadata, state| {
            let mut repaint = None;
            with_backend(state, |backend, state| match event {
                DrmEvent::VBlank(crtc) => {
                    if let Some(output) = backend
                        .outputs
                        .iter_mut()
                        .find(|output| output.crtc == crtc)
                    {
                        match output.kms.frame_submitted() {
                            Ok(Some(mut feedback)) => present_feedback(
                                &mut feedback,
                                &output.output,
                                output.vrr_active,
                                metadata.as_ref(),
                                state,
                            ),
                            Ok(None) => {}
                            Err(error) => warn!(?error, "failed to finish DRM frame"),
                        }
                        output.frame_pending = false;
                        output.repaint_scheduled = true;
                        repaint = Some((crtc, frame_duration(&output.output).mul_f64(0.6)));
                    }
                }
                DrmEvent::Error(error) => warn!(?error, "DRM event error"),
            });
            if let Some((crtc, delay)) = repaint {
                schedule_repaint(&render_handle, crtc, delay);
            }
        })?;

    let udev = UdevBackend::new(session.seat())?;
    event_loop.handle().insert_source(udev, |event, _, state| {
        with_backend(state, |backend, state| match event {
            UdevEvent::Changed { device_id } if device_id == backend.node.dev_id() => {
                if let Err(error) = backend.rescan(state) {
                    error!(?error, "failed to rescan DRM connectors");
                }
            }
            UdevEvent::Removed { device_id } if device_id == backend.node.dev_id() => {
                error!("DRM device was removed");
                state.loop_signal.stop();
            }
            _ => {}
        });
    })?;

    event_loop
        .handle()
        .insert_source(Timer::from_duration(Duration::ZERO), |_, _, state| {
            with_backend(state, |backend, state| backend.render(state));
            TimeoutAction::Drop
        })?;

    Ok(())
}

fn select_device(session: &LibSeatSession, configured: Option<PathBuf>) -> io::Result<PathBuf> {
    if let Some(path) = configured {
        return Ok(path);
    }
    primary_gpu(session.seat())?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no DRM device found for this seat"))
}

fn schedule_repaint(handle: &LoopHandle<'static, Paw2>, crtc: crtc::Handle, delay: Duration) {
    handle
        .insert_source(Timer::from_duration(delay), move |_, _, state| {
            with_backend(state, |backend, state| {
                if let Some(output) = backend
                    .outputs
                    .iter_mut()
                    .find(|output| output.crtc == crtc)
                {
                    output.repaint_scheduled = false;
                }
                backend.render_output(state, crtc)
            });
            TimeoutAction::Drop
        })
        .expect("failed to schedule DRM repaint");
}

fn with_backend(state: &mut Paw2, f: impl FnOnce(&mut DirectBackend, &mut Paw2)) {
    if let Some(mut backend) = state.direct.take() {
        f(&mut backend, state);
        state.direct = Some(backend);
    }
}

pub fn redraw(state: &mut Paw2) {
    with_backend(state, |backend, state| backend.render(state));
}

/// One refresh interval; `refresh` is in mHz.
fn frame_duration(output: &Output) -> Duration {
    output
        .current_mode()
        .map(|mode| Duration::from_secs_f64(1_000.0 / mode.refresh as f64))
        .unwrap_or(Duration::from_millis(16))
}

fn present_feedback(
    feedback: &mut OutputPresentationFeedback,
    output: &Output,
    vrr_active: bool,
    metadata: Option<&DrmEventMetadata>,
    state: &Paw2,
) {
    let frame_duration = frame_duration(output);
    let timestamp: Time<Monotonic> = metadata
        .and_then(|metadata| match metadata.time {
            DrmEventTime::Monotonic(time) if !time.is_zero() => Some(time.into()),
            _ => None,
        })
        .unwrap_or_else(|| state.clock.now());
    let sequence = metadata.map_or(0, |metadata| metadata.sequence as u64);
    let hardware_timestamp = metadata.is_some_and(
        |metadata| matches!(metadata.time, DrmEventTime::Monotonic(time) if !time.is_zero()),
    );
    let mut flags = wp_presentation_feedback::Kind::Vsync;
    if hardware_timestamp {
        flags |=
            wp_presentation_feedback::Kind::HwClock | wp_presentation_feedback::Kind::HwCompletion;
    }
    let refresh = if vrr_active {
        Refresh::variable(frame_duration)
    } else {
        Refresh::fixed(frame_duration)
    };
    feedback.presented(timestamp, refresh, sequence, flags);
}

fn update_vrr(output: &mut ActiveOutput, fullscreen: bool) {
    if output.vrr_support == VrrSupport::NotSupported {
        output.vrr_requested = Some(false);
        output.vrr_active = false;
        return;
    }
    let requested = output.vrr_policy.enabled(fullscreen);
    if output.vrr_requested == Some(requested) {
        return;
    }
    output.vrr_requested = Some(requested);

    let result = output
        .kms
        .with_compositor(|compositor| compositor.use_vrr(requested));
    output.vrr_active = output
        .kms
        .with_compositor(|compositor| compositor.vrr_enabled());
    match result {
        Ok(()) => info!(
            output = %output.output.name(),
            vrr = output.vrr_active,
            "VRR state changed"
        ),
        Err(error) => warn!(
            output = %output.output.name(),
            requested,
            ?error,
            "failed to change VRR state"
        ),
    }
}

impl DirectBackend {
    fn rescan(&mut self, state: &mut Paw2) -> Result<(), Box<dyn std::error::Error>> {
        let scan = self.scanner.scan_connectors(self.manager.device())?;
        let disconnected = scan
            .disconnected
            .iter()
            .filter_map(|(connector, _)| {
                self.outputs
                    .iter()
                    .any(|output| output.connector == connector.handle())
                    .then_some(connector.handle())
            })
            .collect::<Vec<_>>();
        for connector in disconnected {
            self.disconnect(state, connector, false);
        }
        let changed = scan
            .changed
            .iter()
            .filter_map(|(connector, _)| {
                self.outputs
                    .iter()
                    .any(|output| output.connector == connector.handle())
                    .then_some(connector.handle())
            })
            .collect::<Vec<_>>();
        for connector in changed {
            self.disconnect(state, connector, true);
        }

        let legacy = state.config.backend.connector.as_deref();
        let mut candidates = self
            .scanner
            .connectors()
            .values()
            .filter(|connector| connector.state() == connector::State::Connected)
            .filter(|connector| {
                !self
                    .outputs
                    .iter()
                    .any(|output| output.connector == connector.handle())
            })
            .filter_map(|connector| {
                let name = connector_name(connector);
                let crtc = self.scanner.crtc_for_connector(&connector.handle())?;
                Some((name, connector.clone(), crtc))
            })
            .filter(|(name, _, _)| legacy.is_none_or(|wanted| wanted == name))
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(name, _, _)| {
            (
                state
                    .config
                    .monitors
                    .iter()
                    .position(|monitor| monitor.connector == *name)
                    .unwrap_or(usize::MAX),
                name.clone(),
            )
        });
        for (_, connector, crtc) in candidates {
            if let Err(error) = self.connect(state, connector, crtc) {
                warn!(?error, "failed to enable connector");
            }
        }
        Ok(())
    }

    fn connect(
        &mut self,
        state: &mut Paw2,
        connector: connector::Info,
        crtc: crtc::Handle,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let name = connector_name(&connector);
        let config = state
            .config
            .monitors
            .iter()
            .find(|monitor| monitor.connector == name)
            .cloned();
        let drm_mode = select_mode(&connector, config.as_ref())?;
        let mode = Mode::from(drm_mode);
        let preferred = connector
            .modes()
            .iter()
            .find(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
            .or_else(|| connector.modes().first())
            .copied()
            .map(Mode::from)
            .unwrap_or(mode);
        let transform = config
            .as_ref()
            .map_or(Transform::Normal, |value| value.transform);
        let mut position: Point<i32, Logical> = config
            .as_ref()
            .and_then(|value| value.position)
            .map(|[x, y]| (x, y).into())
            .unwrap_or_else(|| self.automatic_position(state));
        let size = transform.transform_size(mode.size).to_logical(1);
        if self.position_overlaps(state, Rectangle::new(position, size)) {
            warn!(connector = %name, ?position, "monitor placement overlaps; using automatic placement");
            position = self.automatic_position(state);
        }
        let info = display_info::for_connector(self.manager.device(), connector.handle());
        let (width, height) = connector.size().unwrap_or((0, 0));
        let output = Output::new(
            name.clone(),
            PhysicalProperties {
                size: (width as i32, height as i32).into(),
                subpixel: connector.subpixel().into(),
                make: info
                    .as_ref()
                    .and_then(|value| value.make())
                    .unwrap_or_else(|| "Unknown".into()),
                model: info
                    .as_ref()
                    .and_then(|value| value.model())
                    .unwrap_or_else(|| "Unknown".into()),
                serial_number: info
                    .as_ref()
                    .and_then(|value| value.serial())
                    .unwrap_or_else(|| "Unknown".into()),
            },
        );
        output.set_preferred(preferred);
        output.change_current_state(Some(mode), Some(transform), None, Some(position));

        let empty = DrmOutputRenderElements::<GlesRenderer, PawRenderElements>::default();
        let kms = self.manager.lock().initialize_output(
            crtc,
            drm_mode,
            &[connector.handle()],
            &output,
            None,
            &mut self.renderer,
            &empty,
        )?;
        let vrr_policy = config
            .as_ref()
            .map_or(VrrPolicy::Automatic, |value| value.vrr);
        let vrr_support = kms
            .with_compositor(|compositor| compositor.vrr_supported(connector.handle()))
            .unwrap_or_else(|error| {
                warn!(connector = %name, ?error, "failed to query VRR support");
                VrrSupport::NotSupported
            });
        let global = output.create_global::<Paw2>(&state.display_handle);
        state.output_added(&output, position);
        self.outputs.push(ActiveOutput {
            connector: connector.handle(),
            crtc,
            output,
            global,
            kms,
            frame_pending: false,
            repaint_scheduled: false,
            direct_scanout_active: false,
            vrr_policy,
            vrr_support,
            vrr_requested: None,
            vrr_active: false,
        });
        info!(connector = %name, size = ?mode.size, refresh = mode.refresh, ?position, ?transform, ?vrr_policy, ?vrr_support, "enabled output");
        Ok(())
    }

    fn disconnect(
        &mut self,
        state: &mut Paw2,
        connector: connector::Handle,
        preserve_layout: bool,
    ) {
        let Some(index) = self
            .outputs
            .iter()
            .position(|output| output.connector == connector)
        else {
            return;
        };
        let active = self.outputs.remove(index);
        state.screencopy_state.fail_output(&active.output);
        let name = active.output.name();
        if !preserve_layout {
            let destination = state.nearest_output(&name);
            state.output_removed(&name, destination.as_deref());
        } else {
            state.output_suspended(&name);
        }
        state.display_handle.remove_global::<Paw2>(active.global);
        info!(connector = %name, "disabled output");
    }

    fn render(&mut self, state: &mut Paw2) {
        let crtcs = self
            .outputs
            .iter()
            .map(|output| output.crtc)
            .collect::<Vec<_>>();
        for crtc in crtcs {
            self.render_output(state, crtc);
        }
    }

    fn render_output(&mut self, state: &mut Paw2, crtc: crtc::Handle) {
        let Some(index) = self.outputs.iter().position(|output| output.crtc == crtc) else {
            return;
        };
        if self.outputs[index].frame_pending || self.outputs[index].repaint_scheduled {
            return;
        }
        let output = self.outputs[index].output.clone();
        self.capture_output(state, &output);
        let fullscreen = state.fullscreen_window(&output).is_some();
        update_vrr(&mut self.outputs[index], fullscreen);
        let frame_target = state.clock.now() + frame_duration(&output);
        state.pre_repaint(&output, frame_target);
        let active = &mut self.outputs[index];
        let direct_scanout = state.config.backend.direct_scanout;
        let mut elements =
            match self
                .effects
                .output_elements(&mut self.renderer, state, &active.output)
            {
                Ok(elements) => elements,
                Err(error) => {
                    warn!(?error, "failed to collect render elements");
                    return;
                }
            };
        Self::add_cursor(
            &mut self.renderer,
            &self.cursor,
            &self.cursor_fallback,
            state,
            &active.output,
            &mut elements,
        );
        match active.kms.render_frame(
            &mut self.renderer,
            &elements,
            clear_color(fullscreen),
            frame_flags(fullscreen, direct_scanout),
        ) {
            Ok(frame) if !frame.is_empty => {
                let scanout_active =
                    matches!(&frame.primary_element, PrimaryPlaneElement::Element(_));
                if frame.needs_sync()
                    && let PrimaryPlaneElement::Swapchain(element) = &frame.primary_element
                {
                    // KMS cannot wait on this render fence, so finish it before
                    // submitting the buffer.
                    while element.sync.wait().is_err() {}
                }
                let feedback = state.take_presentation_feedback(&active.output, &frame.states);
                if let Err(error) = active.kms.queue_frame(feedback) {
                    warn!(?error, "failed to queue DRM frame");
                } else {
                    active.frame_pending = true;
                    if active.direct_scanout_active != scanout_active {
                        active.direct_scanout_active = scanout_active;
                        debug!(output = %active.output.name(), direct_scanout = scanout_active, "DRM presentation path changed");
                    }
                }
            }
            Ok(_)
            | Err(smithay::backend::drm::compositor::RenderFrameError::PrepareFrame(
                FrameError::EmptyFrame,
            )) => {}
            Err(error) => warn!(?error, "failed to render DRM frame"),
        }
        state.send_frames(&active.output);
        state.finish_frame();

        // A frame that produced no damage queues nothing, so the vblank chain
        // stops. Mid-animation that would strand a window short of its target.
        let active = &mut self.outputs[index];
        if state.animations_active() && !active.frame_pending && !active.repaint_scheduled {
            active.repaint_scheduled = true;
            let delay = frame_duration(&active.output).mul_f64(0.6);
            let crtc = active.crtc;
            self.schedule_repaint(crtc, delay);
        }
    }

    fn schedule_repaint(&self, crtc: crtc::Handle, delay: Duration) {
        schedule_repaint(&self.handle, crtc, delay);
    }

    fn capture_output(&mut self, state: &mut Paw2, output: &Output) {
        let captures = state.screencopy_state.take_for_output(output);
        for mut capture in captures {
            let result = (|| -> Result<(), Box<dyn std::error::Error>> {
                if state.output_geometry(output).is_none() {
                    return Err("capture output disappeared".into());
                }
                let mut elements =
                    self.effects
                        .output_elements(&mut self.renderer, state, output)?;
                if capture.overlay_cursor {
                    Self::add_cursor(
                        &mut self.renderer,
                        &self.cursor,
                        &self.cursor_fallback,
                        state,
                        output,
                        &mut elements,
                    );
                }
                let scale = Scale::from(output.current_scale().fractional_scale());
                let crop = capture.region;
                let elements = elements
                    .into_iter()
                    .filter_map(|element| CropRenderElement::from_element(element, scale, crop))
                    .map(|element| {
                        RelocateRenderElement::from_element(
                            element,
                            (-crop.loc.x, -crop.loc.y),
                            Relocate::Relative,
                        )
                    })
                    .collect::<Vec<_>>();
                let mut framebuffer = self.renderer.bind(&mut capture.buffer)?;
                let mut tracker = OutputDamageTracker::new(crop.size, scale, Transform::Normal);
                let fullscreen = state.fullscreen_window(output).is_some();
                let result = tracker.render_output(
                    &mut self.renderer,
                    &mut framebuffer,
                    0,
                    &elements,
                    clear_color(fullscreen),
                )?;
                result.sync.wait()?;
                if capture.with_damage {
                    capture
                        .frame
                        .damage(0, 0, crop.size.w as u32, crop.size.h as u32);
                }
                capture.frame.flags(Flags::empty());
                let elapsed = state.start_time.elapsed();
                let seconds = elapsed.as_secs();
                capture.frame.ready(
                    (seconds >> 32) as u32,
                    seconds as u32,
                    elapsed.subsec_nanos(),
                );
                Ok(())
            })();
            if let Err(error) = result {
                warn!(?error, "failed to capture output");
                capture.frame.failed();
            }
        }
    }

    fn automatic_position(&self, state: &Paw2) -> Point<i32, Logical> {
        let x = state
            .output_geometries()
            .into_iter()
            .map(|(_, geometry)| geometry)
            .map(|geometry| geometry.loc.x + geometry.size.w)
            .max()
            .unwrap_or(0);
        (x, 0).into()
    }

    fn position_overlaps(&self, state: &Paw2, proposed: Rectangle<i32, Logical>) -> bool {
        state
            .output_geometries()
            .into_iter()
            .map(|(_, geometry)| geometry)
            .any(|geometry| geometry.overlaps(proposed))
    }

    fn add_cursor(
        renderer: &mut GlesRenderer,
        cursor: &Option<crate::cursor::Cursor>,
        fallback: &SolidColorBuffer,
        state: &mut Paw2,
        output: &Output,
        elements: &mut Vec<PawRenderElements>,
    ) {
        let Some(geometry) = state.output_geometry(output) else {
            return;
        };
        let location = state.seat.get_pointer().unwrap().current_location();
        if !geometry.to_f64().contains(location) || state.cursor_status == CursorImageStatus::Hidden
        {
            return;
        }
        let scale = Scale::from(output.current_scale().fractional_scale());
        let local = location - geometry.loc.to_f64();

        if let CursorImageStatus::Surface(surface) = &state.cursor_status
            && surface.alive()
        {
            let hotspot = compositor::with_states(surface, |states| {
                states
                    .data_map
                    .get::<CursorImageSurfaceData>()
                    .and_then(|attributes| attributes.lock().ok().map(|value| value.hotspot))
                    .unwrap_or_default()
            });
            let position = (local - hotspot.to_f64()).to_physical(scale).to_i32_round();
            let cursor = AsRenderElements::<GlesRenderer>::render_elements::<
                WaylandSurfaceRenderElement<GlesRenderer>,
            >(
                &SurfaceTree::from_surface(surface),
                renderer,
                position,
                scale,
                1.0,
            );
            elements.splice(0..0, cursor.into_iter().map(PawRenderElements::Cursor));
            return;
        }

        if let Some(cursor) = cursor {
            let position = (local - cursor.hotspot.to_f64()).to_physical(scale);
            if let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                position,
                &cursor.buffer,
                None,
                None,
                None,
                Kind::Cursor,
            ) {
                elements.insert(0, PawRenderElements::Memory(element));
                return;
            }
        }

        let position: Point<i32, smithay::utils::Physical> =
            local.to_physical(scale).to_i32_round();
        elements.insert(
            0,
            PawRenderElements::Solid(SolidColorRenderElement::from_buffer(
                fallback,
                position,
                scale,
                1.0,
                Kind::Cursor,
            )),
        );
    }
}

fn frame_flags(fullscreen: bool, direct_scanout: bool) -> FrameFlags {
    if !direct_scanout {
        FrameFlags::empty()
    } else if fullscreen {
        FrameFlags::ALLOW_PRIMARY_PLANE_SCANOUT
    } else {
        FrameFlags::DEFAULT
    }
}

fn connector_name(connector: &connector::Info) -> String {
    format!(
        "{}-{}",
        connector.interface().as_str(),
        connector.interface_id()
    )
}

fn select_mode(
    connector: &connector::Info,
    config: Option<&MonitorConfig>,
) -> Result<DrmMode, io::Error> {
    let name = connector_name(connector);
    let preferred = connector
        .modes()
        .iter()
        .find(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| connector.modes().first())
        .copied()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{name} has no modes")))?;
    let Some(config) = config else {
        return Ok(preferred);
    };
    if config.resolution.is_none() && config.refresh.is_none() {
        return Ok(preferred);
    }

    let target_size = config
        .resolution
        .map(|[width, height]| (width, height))
        .unwrap_or_else(|| preferred.size());
    let preferred_refresh = Mode::from(preferred).refresh;
    let target_refresh = config
        .refresh
        .map(|refresh| (refresh * 1000.0).round() as i32)
        .unwrap_or(preferred_refresh);
    let modes = connector
        .modes()
        .iter()
        .map(|mode| (mode.size(), Mode::from(*mode).refresh))
        .collect::<Vec<_>>();
    let selected =
        best_mode(&modes, target_size, target_refresh).map(|index| connector.modes()[index]);
    let Some(selected) = selected else {
        warn!(connector = %name, ?target_size, "requested resolution unavailable; using preferred mode");
        return Ok(preferred);
    };
    if config.refresh.is_some() && !refresh_matches(Mode::from(selected).refresh, target_refresh) {
        warn!(connector = %name, refresh = target_refresh as f64 / 1000.0, "requested refresh unavailable; using preferred mode");
        Ok(preferred)
    } else {
        Ok(selected)
    }
}

fn best_mode(
    modes: &[((u16, u16), i32)],
    target_size: (u16, u16),
    target_refresh: i32,
) -> Option<usize> {
    modes
        .iter()
        .enumerate()
        .filter(|(_, (size, _))| *size == target_size)
        .min_by_key(|(_, (_, refresh))| (*refresh - target_refresh).abs())
        .map(|(index, _)| index)
}

fn refresh_matches(actual: i32, requested: i32) -> bool {
    (actual - requested).abs() <= 500
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_safe_scanout_flags() {
        assert_eq!(
            frame_flags(true, true),
            FrameFlags::ALLOW_PRIMARY_PLANE_SCANOUT
        );
        assert_eq!(frame_flags(false, true), FrameFlags::DEFAULT);
        assert_eq!(frame_flags(true, false), FrameFlags::empty());
        assert_eq!(frame_flags(false, false), FrameFlags::empty());
    }

    #[test]
    fn selects_closest_monitor_mode() {
        let modes = [
            ((2560, 1440), 120_000),
            ((2560, 1440), 199_999),
            ((1920, 1080), 240_000),
        ];
        assert_eq!(best_mode(&modes, (2560, 1440), 200_000), Some(1));
        assert!(refresh_matches(199_999, 200_000));
        assert!(!refresh_matches(199_000, 200_000));
    }
}
