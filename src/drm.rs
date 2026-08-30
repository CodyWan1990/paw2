use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    time::{Duration, Instant},
};

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
            Bind, BufferType, ExportMem, ImportDma, ImportEgl, ImportMemWl, Offscreen, buffer_type,
            damage::OutputDamageTracker,
            element::{
                AsRenderElements, Kind, RenderElement,
                memory::MemoryRenderBufferRenderElement,
                solid::{SolidColorBuffer, SolidColorRenderElement},
                surface::WaylandSurfaceRenderElement,
                utils::{CropRenderElement, Relocate, RelocateRenderElement},
            },
            gles::{GlesRenderer, GlesTarget, GlesTexture},
        },
        session::{
            Event as SessionEvent, Session,
            libseat::{LibSeatSession, LibSeatSessionNotifier},
        },
        udev::{UdevBackend, UdevEvent, primary_gpu},
    },
    desktop::{layer_map_for_output, space::SurfaceTree, utils::OutputPresentationFeedback},
    input::pointer::{CursorIcon, CursorImageStatus, CursorImageSurfaceData},
    output::{Mode, Output, PhysicalProperties, Scale as OutputScale},
    reexports::{
        calloop::{
            EventLoop, LoopHandle,
            timer::{TimeoutAction, Timer},
        },
        drm::control::{Mode as DrmMode, ModeTypeFlags, connector, crtc},
        input::Libinput,
        rustix::fs::OFlags,
        wayland_protocols::wp::presentation_time::server::wp_presentation_feedback,
        wayland_server::{backend::GlobalId, protocol::wl_shm},
    },
    utils::{
        DeviceFd, IsAlive, Logical, Monotonic, Physical, Point, Rectangle, Scale, Size, Time,
        Transform,
    },
    wayland::{
        compositor,
        dmabuf::{DmabufFeedbackBuilder, get_dmabuf},
        drm_syncobj::{DrmSyncobjState, supports_syncobj_eventfd},
        presentation::Refresh,
        shm::{with_buffer_contents, with_buffer_contents_mut},
    },
};
use smithay_drm_extras::{display_info, drm_scanner::DrmScanner};
use tracing::{debug, error, info, trace, warn};

use crate::{
    config::{MonitorConfig, ThemeConfig, VrrPolicy},
    cursor::CursorSettings,
    render::{Effects, PawRenderElements, clear_color},
    screencopy::CaptureTarget,
    state::Paw2,
};

type Allocator = GbmAllocator<DrmDeviceFd>;
type Exporter = GbmFramebufferExporter<DrmDeviceFd>;
type FrameFeedback = OutputPresentationFeedback;
type KmsOutput = DrmOutput<Allocator, Exporter, FrameFeedback, DrmDeviceFd>;
type OutputManager = DrmOutputManager<Allocator, Exporter, FrameFeedback, DrmDeviceFd>;
type CaptureRenderElement = RelocateRenderElement<CropRenderElement<PawRenderElements>>;
type OutputPlacement = (Size<i32, Logical>, Option<Point<i32, Logical>>);

fn logical_output_size(mode: Mode, transform: Transform, scale: f64) -> Size<i32, Logical> {
    transform
        .transform_size(mode.size)
        .to_f64()
        .to_logical(scale)
        .to_i32_ceil()
}

struct CaptureScene {
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    elements: Vec<CaptureRenderElement>,
    clear: [f32; 4],
}

/// How many times to retry a render fence before declaring the GPU wedged.
const SYNC_WAIT_ATTEMPTS: u32 = 8;

fn render_capture_frame<E>(
    renderer: &mut GlesRenderer,
    framebuffer: &mut GlesTarget<'_>,
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    elements: &[E],
    clear: [f32; 4],
) -> Result<(), Box<dyn std::error::Error>>
where
    E: RenderElement<GlesRenderer>,
{
    let mut tracker = OutputDamageTracker::new(size, scale, Transform::Normal);
    tracker
        .render_output(renderer, framebuffer, 0, elements, clear)?
        .sync
        .wait()?;
    Ok(())
}

fn copy_to_shm(
    buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    pixels: &[u8],
    size: Size<i32, Physical>,
) -> Result<(), Box<dyn std::error::Error>> {
    let row_bytes = usize::try_from(size.w)? * 4;
    let height = usize::try_from(size.h)?;
    if pixels.len() < row_bytes * height {
        return Err("renderer returned a short screenshot".into());
    }
    let copied = with_buffer_contents_mut(buffer, |pointer, length, data| {
        let Ok(offset) = usize::try_from(data.offset) else {
            return false;
        };
        let Ok(stride) = usize::try_from(data.stride) else {
            return false;
        };
        if stride < row_bytes
            || offset
                .checked_add(stride.saturating_mul(height.saturating_sub(1)))
                .and_then(|last_row| last_row.checked_add(row_bytes))
                .is_none_or(|required| required > length)
        {
            return false;
        }
        for row in 0..height {
            // The pool can be concurrently mapped by the client, so copy bytes
            // directly instead of creating a Rust reference to shared memory.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    pixels.as_ptr().add(row * row_bytes),
                    pointer.add(offset + row * stride),
                    row_bytes,
                );
            }
        }
        true
    })?;
    if !copied {
        return Err("screenshot SHM buffer is too small".into());
    }
    Ok(())
}

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
    // Declaration order is drop order: every field below borrows GPU resources
    // from `renderer`, so the renderer must be declared after all of them.
    manager: OutputManager,
    scanner: DrmScanner,
    outputs: Vec<ActiveOutput>,
    cursor: Option<crate::cursor::Cursor>,
    /// Every shape loaded for `cursor_settings`. Smithay asks for a cursor on
    /// every focus change and `wp_cursor_shape` lets a client ask as often as
    /// it likes, and each miss is an `XCURSOR_PATH` scan plus a file read.
    cursor_cache: HashMap<CursorIcon, crate::cursor::Cursor>,
    /// What `cursor_cache` was filled for; a theme change empties it.
    cursor_settings: CursorSettings,
    cursor_fallback: SolidColorBuffer,
    effects: Effects,
    pub renderer: GlesRenderer,
}

pub fn init(
    event_loop: &mut EventLoop<'static, Paw2>,
    state: &mut Paw2,
) -> Result<(), Box<dyn std::error::Error>> {
    let clock = Instant::now();
    let mut attempt = 1;
    let (session, session_notifier, path, fd) = loop {
        match take_seat(state.config.backend.drm_device.clone()) {
            Ok(taken) => break taken,
            Err(error) if attempt < SEAT_ATTEMPTS => {
                warn!(?error, attempt, "could not take the seat yet; retrying");
                std::thread::sleep(SEAT_RETRY);
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    };
    let node = DrmNode::from_path(&path)?;
    info!(device = %path.display(), "using DRM device");

    let (device, drm_notifier) = DrmDevice::new(fd.clone(), true)?;
    if supports_syncobj_eventfd(&fd) {
        state.drm_syncobj_state = Some(DrmSyncobjState::new::<Paw2>(
            &state.display_handle,
            fd.clone(),
        ));
        info!("enabled linux-drm-syncobj explicit synchronization");
    }
    crate::phase(&clock, "drm device opened");
    let gbm = GbmDevice::new(fd)?;
    let display = unsafe { EGLDisplay::new(gbm.clone())? };
    let context = EGLContext::new_with_priority(&display, ContextPriority::High)?;
    let mut renderer = unsafe { GlesRenderer::new(context)? };
    if let Err(error) = renderer.bind_wl_display(&state.display_handle) {
        warn!(?error, "legacy EGL Wayland buffer support unavailable");
    }
    crate::phase(&clock, "renderer created");
    state.shm_state.update_formats(renderer.shm_formats());
    let effects = Effects::new(&mut renderer, state.config.appearance);
    crate::phase(&clock, "effects compiled");
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
        Some(gbm.clone()),
        [Fourcc::Argb8888, Fourcc::Abgr8888],
        formats,
    );

    let cursor_settings = state.cursor_defaults.configured(
        state.config.theme.cursor_theme.as_deref(),
        state.config.theme.cursor_size,
    );
    let cursor = crate::cursor::Cursor::load(&cursor_settings, CursorIcon::Default)
        .map_err(|error| warn!(?error, "could not load cursor theme; using fallback cursor"))
        .ok();
    let mut backend = DirectBackend {
        handle: event_loop.handle(),
        _session: session.clone(),
        node,
        manager,
        scanner: DrmScanner::new(),
        outputs: Vec::new(),
        cursor_cache: cursor
            .clone()
            .map(|cursor| HashMap::from([(CursorIcon::Default, cursor)]))
            .unwrap_or_default(),
        cursor,
        cursor_settings,
        cursor_fallback: SolidColorBuffer::new((12, 12), [1.0, 1.0, 1.0, 1.0]),
        effects,
        renderer,
    };
    backend.rescan(state)?;
    crate::phase(&clock, "outputs enabled");
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

    // A `LoopHandle` owns an `Rc` to the loop, and this callback is stored *in*
    // the loop: keeping a strong handle here would leak every event source,
    // including the libseat session that must be closed on exit.
    let render_handle = event_loop.handle().downgrade();
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
                let scheduled = render_handle
                    .upgrade()
                    .is_some_and(|handle| schedule_repaint(&handle, crtc, delay));
                if !scheduled {
                    with_backend(state, |backend, _| {
                        if let Some(output) = backend
                            .outputs
                            .iter_mut()
                            .find(|output| output.crtc == crtc)
                        {
                            output.repaint_scheduled = false;
                        }
                    });
                }
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
                state.begin_shutdown();
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

    // Everything the first frame needs is wired up, so draw it before opening
    // the input devices below - libseat opens those one logind round trip at a
    // time, and it is the longest stretch left in a login. Nothing sees input
    // any later for it: the event loop that delivers those events does not
    // start until `main` runs it either way, so the wait moves behind the
    // desktop instead of in front of it.
    event_loop.dispatch(Duration::ZERO, state)?;
    crate::phase(&clock, "first frame");

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

    crate::phase(&clock, "input devices opened");
    Ok(())
}

/// How long to keep trying to take the seat before giving up.
///
/// SDDM hands the seat over asynchronously while its Wayland greeter is torn
/// down, and the first login after boot loses that race: the session exits
/// before it draws anything and the greeter comes straight back. Waiting it out
/// costs a second in the worst case and saves logging in twice.
// ponytail: a fixed budget; make it adaptive only if a slower machine needs more.
const SEAT_ATTEMPTS: u32 = 10;
const SEAT_RETRY: Duration = Duration::from_millis(100);

/// Take the seat and open the GPU, as one unit.
///
/// Session and notifier are returned together on purpose: the notifier holds
/// the only strong reference to the seat, so a caller that retries has to drop
/// both to release it before trying again.
fn take_seat(
    configured: Option<PathBuf>,
) -> Result<
    (LibSeatSession, LibSeatSessionNotifier, PathBuf, DrmDeviceFd),
    Box<dyn std::error::Error>,
> {
    let (mut session, notifier) = LibSeatSession::new()?;
    let path = select_device(&session, configured)?;
    let fd = session.open(
        &path,
        OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
    )?;
    Ok((session, notifier, path, DrmDeviceFd::new(DeviceFd::from(fd))))
}

fn select_device(session: &LibSeatSession, configured: Option<PathBuf>) -> io::Result<PathBuf> {
    if let Some(path) = configured {
        return Ok(path);
    }
    primary_gpu(session.seat())?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no DRM device found for this seat"))
}

/// Arms the next composition for `crtc`, reporting whether it was armed.
///
/// The caller owns `repaint_scheduled`, and only this timer's callback clears
/// it, so a failure that went unreported would leave the flag set and
/// `render_output` would skip that output forever. Panicking instead would take
/// the whole session down over fd exhaustion, which clears on its own.
#[must_use]
fn schedule_repaint(
    handle: &LoopHandle<'static, Paw2>,
    crtc: crtc::Handle,
    delay: Duration,
) -> bool {
    let scheduled = handle.insert_source(Timer::from_duration(delay), move |_, _, state| {
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
    });
    match scheduled {
        Ok(_) => true,
        Err(error) => {
            warn!(?error, "failed to schedule DRM repaint");
            false
        }
    }
}

/// Run `f` with the backend moved out of `state`, so both are mutable at once.
///
/// The backend is absent under `--nested`, and *also* absent to anything that
/// re-enters while `f` is running - a redraw reached from inside a render, say.
/// That second case silently drops a frame, so say so rather than letting it
/// look like the nested no-op.
fn with_backend(state: &mut Paw2, f: impl FnOnce(&mut DirectBackend, &mut Paw2)) {
    let Some(mut backend) = state.direct.take() else {
        trace!("no DRM backend for this call; nested session, or re-entered from within one");
        return;
    };
    f(&mut backend, state);
    state.direct = Some(backend);
}

pub fn redraw(state: &mut Paw2) {
    if crate::state::shutting_down() {
        return;
    }
    with_backend(state, |backend, state| backend.render(state));
}

/// Make `icon` the drawn cursor, loading it only on a cache miss.
///
/// A theme change empties the cache; within one theme every shape is read from
/// disk at most once. See [`DirectBackend::cursor_cache`] for why that matters.
fn set_cursor(
    state: &mut Paw2,
    settings: &CursorSettings,
    icon: CursorIcon,
) -> Result<(), String> {
    let mut result = Ok(());
    with_backend(state, |backend, _| {
        if backend.cursor_settings != *settings {
            backend.cursor_cache.clear();
            backend.cursor_settings = settings.clone();
        }
        if let Some(cursor) = backend.cursor_cache.get(&icon) {
            backend.cursor = Some(cursor.clone());
            return;
        }
        match crate::cursor::Cursor::load(settings, icon) {
            Ok(cursor) => {
                backend.cursor_cache.insert(icon, cursor.clone());
                backend.cursor = Some(cursor);
            }
            Err(error) => result = Err(format!("could not load cursor theme: {error}")),
        }
    });
    result
}

/// The configured theme changed; reload whatever shape is currently showing.
pub fn reload_cursor(state: &mut Paw2, theme: &ThemeConfig) -> Result<(), String> {
    let settings = state
        .cursor_defaults
        .configured(theme.cursor_theme.as_deref(), theme.cursor_size);
    let icon = match state.cursor_status {
        CursorImageStatus::Named(icon) => icon,
        _ => CursorIcon::Default,
    };
    set_cursor(state, &settings, icon)
}

/// A client asked for a named shape.
pub fn select_cursor_icon(state: &mut Paw2, icon: CursorIcon) {
    let settings = state.cursor_defaults.configured(
        state.config.theme.cursor_theme.as_deref(),
        state.config.theme.cursor_size,
    );
    if let Err(error) = set_cursor(state, &settings, icon) {
        warn!(%error, %icon, "could not load requested cursor shape");
    }
}

pub fn apply_monitor_changes(state: &mut Paw2, monitors: &[MonitorConfig]) -> Vec<String> {
    let mut errors = Vec::new();
    with_backend(state, |backend, state| {
        errors = backend.apply_monitor_changes(state, monitors)
    });
    errors
}

/// One refresh interval; `refresh` is in mHz.
///
/// `Mode::refresh` is a plain integer computation over the DRM mode with no
/// lower bound, and `Duration::from_secs_f64` panics on a non-finite value -
/// so a mode reporting zero must fall through to the default, not divide.
fn frame_duration(output: &Output) -> Duration {
    output
        .current_mode()
        .filter(|mode| mode.refresh > 0)
        .map_or(Duration::from_millis(16), |mode| {
            Duration::from_secs_f64(1_000.0 / f64::from(mode.refresh))
        })
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
    pub(crate) fn render_node(&self) -> DrmNode {
        self.node
    }

    pub(crate) fn capture_formats(&self) -> Vec<(Fourcc, Vec<Modifier>)> {
        let mut formats = Vec::<(Fourcc, Vec<Modifier>)>::new();
        for format in self.renderer.egl_context().dmabuf_render_formats().iter() {
            if !matches!(format.code, Fourcc::Xrgb8888 | Fourcc::Argb8888) {
                continue;
            }
            if let Some((_, modifiers)) = formats.iter_mut().find(|(code, _)| *code == format.code)
            {
                modifiers.push(format.modifier);
            } else {
                formats.push((format.code, vec![format.modifier]));
            }
        }
        formats
    }

    fn apply_monitor_changes(
        &mut self,
        state: &mut Paw2,
        monitors: &[MonitorConfig],
    ) -> Vec<String> {
        let mut errors = Vec::new();
        let mut geometry_changed = false;
        for active in &mut self.outputs {
            let Some(connector) = self
                .scanner
                .connectors()
                .values()
                .find(|connector| connector.handle() == active.connector)
            else {
                continue;
            };
            let name = active.output.name();
            let config = monitors.iter().find(|monitor| monitor.connector == name);
            let transform = config.map_or(Transform::Normal, |monitor| monitor.transform);
            let scale = config.map_or(1.0, |monitor| monitor.scale);
            let current_mode = active.output.current_mode();
            let current_transform = active.output.current_transform();
            let current_scale = active.output.current_scale().fractional_scale();
            let mut applied_mode = None;

            match requested_mode(connector, config) {
                Ok(target) if current_mode != Some(Mode::from(target)) => {
                    let mode = Mode::from(target);
                    let empty =
                        DrmOutputRenderElements::<GlesRenderer, PawRenderElements>::default();
                    let previous = active
                        .kms
                        .with_compositor(|compositor| compositor.current_mode());
                    match active.kms.use_mode(target, &mut self.renderer, &empty) {
                        Ok(()) => applied_mode = Some(mode),
                        Err(error) => {
                            warn!(connector = %name, ?error, "could not apply monitor mode");
                            let changed = active
                                .kms
                                .with_compositor(|compositor| compositor.pending_mode())
                                != previous;
                            let rollback = changed
                                .then(|| active.kms.use_mode(previous, &mut self.renderer, &empty))
                                .and_then(Result::err);
                            if let Some(rollback) = rollback {
                                error!(connector = %name, ?rollback, "could not restore previous monitor mode");
                                errors.push(format!(
                                    "{name}: could not apply requested mode ({error}) or restore the previous mode ({rollback})"
                                ));
                            } else {
                                errors.push(format!(
                                    "{name}: could not apply requested mode: {error}"
                                ));
                            }
                        }
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    warn!(connector = %name, %error, "could not apply monitor mode");
                    errors.push(format!("{name}: {error}"));
                }
            }

            let transform_changed = transform != current_transform;
            let scale_changed = scale != current_scale;
            if applied_mode.is_none() && !transform_changed && !scale_changed {
                continue;
            }
            let old_size = current_mode
                .map(|mode| logical_output_size(mode, current_transform, current_scale));
            let new_size = applied_mode
                .or(current_mode)
                .map(|mode| logical_output_size(mode, transform, scale));
            geometry_changed |= old_size != new_size;
            active.output.change_current_state(
                applied_mode,
                transform_changed.then_some(transform),
                scale_changed.then_some(OutputScale::Fractional(scale)),
                None,
            );
            info!(connector = %name, mode = ?active.output.current_mode(), ?scale, ?transform, "changed monitor configuration");
        }

        if geometry_changed {
            self.reflow_automatic_outputs(state);
            for output in &self.outputs {
                layer_map_for_output(&output.output).arrange();
            }
        }
        errors
    }

    fn reflow_automatic_outputs(&self, state: &mut Paw2) {
        let specifications = self
            .outputs
            .iter()
            .filter_map(|active| {
                let mode = active.output.current_mode()?;
                let name = active.output.name();
                let scale = active.output.current_scale().fractional_scale();
                let size = logical_output_size(mode, active.output.current_transform(), scale);
                let position = state
                    .config
                    .monitors
                    .iter()
                    .find(|monitor| monitor.connector == name)
                    .and_then(|monitor| monitor.position)
                    .map(|[x, y]| (x, y).into());
                Some((active.output.clone(), size, position))
            })
            .collect::<Vec<_>>();
        let positions = automatic_positions(
            &specifications
                .iter()
                .map(|(_, size, position)| (*size, *position))
                .collect::<Vec<_>>(),
        );
        for ((output, _, _), position) in specifications.into_iter().zip(positions) {
            state.reposition_output(&output, position);
        }

        let geometries = state.output_rects().collect::<Vec<_>>();
        let pointer = state.seat.get_pointer().unwrap();
        pointer.set_location(crate::input::constrain_pointer(
            pointer.current_location(),
            &geometries,
        ));
    }

    fn rescan(&mut self, state: &mut Paw2) -> Result<(), Box<dyn std::error::Error>> {
        let scan = self.scanner.scan_connectors(self.manager.device())?;
        // A changed connector keeps its layout; a disconnected one hands it over.
        for (connectors, preserve_layout) in [(&scan.disconnected, false), (&scan.changed, true)] {
            let known = connectors
                .iter()
                .filter_map(|(connector, _)| {
                    self.outputs
                        .iter()
                        .any(|output| output.connector == connector.handle())
                        .then_some(connector.handle())
                })
                .collect::<Vec<_>>();
            for connector in known {
                self.disconnect(state, connector, preserve_layout);
            }
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
        let preferred = preferred_mode(&connector).map_or(mode, Mode::from);
        let transform = config
            .as_ref()
            .map_or(Transform::Normal, |value| value.transform);
        let scale = config.as_ref().map_or(1.0, |value| value.scale);
        let size = logical_output_size(mode, transform, scale);
        let configured: Option<Point<i32, Logical>> = config
            .as_ref()
            .and_then(|value| value.position)
            .map(|[x, y]| (x, y).into());
        let overlaps = |position| {
            state
                .output_rects()
                .any(|geometry| geometry.overlaps(Rectangle::new(position, size)))
        };
        let position = match configured {
            Some(position) if overlaps(position) => {
                warn!(connector = %name, ?position, "monitor placement overlaps; using automatic placement");
                automatic_position(state.output_rects(), size)
            }
            Some(position) => position,
            None => automatic_position(state.output_rects(), size),
        };
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
        for advertised in connector.modes() {
            output.add_mode(Mode::from(*advertised));
        }
        output.set_preferred(preferred);
        output.change_current_state(
            Some(mode),
            Some(transform),
            Some(OutputScale::Fractional(scale)),
            Some(position),
        );

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
        info!(connector = %name, size = ?mode.size, refresh = mode.refresh, ?position, ?scale, ?transform, ?vrr_policy, ?vrr_support, "enabled output");
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
        // Removing the `wl_output` global is not a close. A bar or wallpaper
        // that pinned this output by name would otherwise keep a surface
        // nothing will ever draw.
        for layer in layer_map_for_output(&active.output).layers() {
            layer.layer_surface().send_close();
        }
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

    /// Hand the GPU back in the state the previous session left it in.
    ///
    /// Dropping the outputs clears each surface, and the device-wide reset
    /// disables any connector or plane left over, so the next compositor does
    /// not inherit a conflicting atomic state.
    pub fn shutdown(mut self) {
        self.outputs.clear();
        if let Err(error) = self.manager.device_mut().reset_state() {
            warn!(?error, "could not reset the DRM device on shutdown");
        }
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
        let fullscreen = state.renders_fullscreen(&output);
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
        let mut retry = false;
        match active.kms.render_frame(
            &mut self.renderer,
            &elements,
            clear_color(state, &output),
            frame_flags(fullscreen, direct_scanout),
        ) {
            Ok(frame) if !frame.is_empty => {
                let scanout_active =
                    matches!(&frame.primary_element, PrimaryPlaneElement::Element(_));
                // KMS cannot wait on this render fence, so finish it before
                // submitting the buffer. A fence that keeps failing means the
                // GPU is wedged; give up rather than spin the event loop
                // forever with no input and no way out.
                let synced = if frame.needs_sync()
                    && let PrimaryPlaneElement::Swapchain(element) = &frame.primary_element
                {
                    (0..SYNC_WAIT_ATTEMPTS).any(|_| element.sync.wait().is_ok())
                } else {
                    true
                };
                if synced {
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
                } else {
                    // Submitting now would scan out whatever the GPU had got to.
                    // Drop the frame and come back for it.
                    warn!("render fence never signalled; skipping the frame");
                    retry = true;
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
        // stops. Mid-animation that would strand a window short of its target,
        // and a frame skipped over a stuck fence would never be retried.
        let active = &mut self.outputs[index];
        if (state.animations_active() || retry)
            && !active.frame_pending
            && !active.repaint_scheduled
        {
            let delay = frame_duration(&active.output).mul_f64(0.6);
            let crtc = active.crtc;
            active.repaint_scheduled = schedule_repaint(&self.handle, crtc, delay);
        }
    }

    fn capture_output(&mut self, state: &mut Paw2, output: &Output) {
        let mut captures = state.screencopy_state.take_for_output(output);
        // A window capture renders from the window's own bounding box, not from
        // the output being composited, so whichever output reaches here first
        // can serve it. Waiting for a fixed index only added latency, and
        // stalled entirely whenever that output had a frame already pending.
        captures.extend(state.screencopy_state.take_window_captures());
        for capture in captures {
            let result = self.render_capture(state, capture);
            if let Err(error) = result {
                warn!(?error, "failed to capture image");
            }
        }
    }

    fn render_capture(
        &mut self,
        state: &mut Paw2,
        capture: crate::screencopy::PendingCapture,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let scene = self.capture_scene(state, &capture.target, capture.draw_cursor)?;
        let size = scene.size;
        let buffer = capture.frame.buffer();
        match buffer_type(&buffer) {
            Some(BufferType::Dma) => {
                let mut dmabuf = get_dmabuf(&buffer)?.clone();
                let mut framebuffer = self.renderer.bind(&mut dmabuf)?;
                render_capture_frame(
                    &mut self.renderer,
                    &mut framebuffer,
                    scene.size,
                    scene.scale,
                    &scene.elements,
                    scene.clear,
                )?;
            }
            Some(BufferType::Shm) => {
                let data = with_buffer_contents(&buffer, |_, _, data| data)?;
                let format = match data.format {
                    wl_shm::Format::Xrgb8888 => Fourcc::Xrgb8888,
                    wl_shm::Format::Argb8888 => Fourcc::Argb8888,
                    _ => return Err("unsupported screenshot SHM format".into()),
                };
                let mut texture = Offscreen::<GlesTexture>::create_buffer(
                    &mut self.renderer,
                    format,
                    (scene.size.w, scene.size.h).into(),
                )?;
                let mut framebuffer = self.renderer.bind(&mut texture)?;
                render_capture_frame(
                    &mut self.renderer,
                    &mut framebuffer,
                    scene.size,
                    scene.scale,
                    &scene.elements,
                    scene.clear,
                )?;
                let mapping = self.renderer.copy_framebuffer(
                    &framebuffer,
                    Rectangle::from_size((scene.size.w, scene.size.h).into()),
                    format,
                )?;
                let pixels = self.renderer.map_texture(&mapping)?;
                copy_to_shm(&buffer, pixels, scene.size)?;
            }
            _ => return Err("unsupported screenshot buffer".into()),
        }

        let now = Duration::from(state.clock.now());
        capture.frame.success(
            Transform::Normal,
            vec![Rectangle::<i32, smithay::utils::Buffer>::from_size(
                (size.w, size.h).into(),
            )],
            now,
        );
        Ok(())
    }

    fn capture_scene(
        &mut self,
        state: &mut Paw2,
        target: &CaptureTarget,
        draw_cursor: bool,
    ) -> Result<CaptureScene, Box<dyn std::error::Error>> {
        let (target_output, scale, crop, mut elements, clear) = match target {
            CaptureTarget::Output(target) => {
                let output = target.upgrade().ok_or("capture output disappeared")?;
                let geometry = state
                    .output_geometry(&output)
                    .ok_or("capture output disappeared")?;
                let scale = Scale::from(output.current_scale().fractional_scale());
                let crop = Rectangle::from_size(geometry.size)
                    .to_f64()
                    .to_physical(scale)
                    .to_i32_round();
                let elements = self
                    .effects
                    .output_elements(&mut self.renderer, state, &output)?;
                let clear = clear_color(state, &output);
                (output, scale, crop, elements, clear)
            }
            CaptureTarget::Window(target) => {
                let window = target.upgrade().ok_or("capture window disappeared")?;
                let output = state
                    .capture_output_for_window(&window)
                    .ok_or("capture window is not on an output")?;
                let output_geometry = state
                    .output_geometry(&output)
                    .ok_or("capture output disappeared")?;
                let window_location = state
                    .capture_window_location(&window)
                    .ok_or("capture window is not mapped")?;
                let scale = Scale::from(output.current_scale().fractional_scale());
                let bbox = window.bbox_with_popups();
                let crop =
                    Rectangle::new(window_location + bbox.loc - output_geometry.loc, bbox.size)
                        .to_f64()
                        .to_physical(scale)
                        .to_i32_round();
                let location = (window_location - output_geometry.loc)
                    .to_f64()
                    .to_physical(scale)
                    .to_i32_round();
                let elements = AsRenderElements::<GlesRenderer>::render_elements::<
                    WaylandSurfaceRenderElement<GlesRenderer>,
                >(&window, &mut self.renderer, location, scale, 1.0)
                .into_iter()
                .map(PawRenderElements::Cursor)
                .collect();
                (output, scale, crop, elements, [0.0, 0.0, 0.0, 0.0])
            }
        };
        if draw_cursor {
            Self::add_cursor(
                &mut self.renderer,
                &self.cursor,
                &self.cursor_fallback,
                state,
                &target_output,
                &mut elements,
            );
        }
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
            .collect();
        Ok(CaptureScene {
            size: crop.size,
            scale,
            elements,
            clear,
        })
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

/// Where a newly connected output of `size` goes, given everything already on
/// screen.
///
/// Defers to [`automatic_positions`] rather than carrying a second rule: this
/// used to place at the rightmost edge while a config reload gap-packed, so the
/// same monitor landed in two different spots depending on which path ran.
fn automatic_position(
    existing: impl Iterator<Item = Rectangle<i32, Logical>>,
    size: Size<i32, Logical>,
) -> Point<i32, Logical> {
    let mut placements = existing
        .map(|geometry| (geometry.size, Some(geometry.loc)))
        .collect::<Vec<OutputPlacement>>();
    placements.push((size, None));
    automatic_positions(&placements)
        .last()
        .copied()
        .unwrap_or_default()
}

fn automatic_positions(outputs: &[OutputPlacement]) -> Vec<Point<i32, Logical>> {
    let mut positions = vec![Point::default(); outputs.len()];
    let mut occupied = outputs
        .iter()
        .enumerate()
        .filter_map(|(index, (size, position))| {
            let position = (*position)?;
            positions[index] = position;
            Some(Rectangle::new(position, *size))
        })
        .collect::<Vec<_>>();

    for (index, (size, position)) in outputs.iter().enumerate() {
        if position.is_some() {
            continue;
        }
        let mut x = 0;
        loop {
            let proposed = Rectangle::new((x, 0).into(), *size);
            let Some(next_x) = occupied
                .iter()
                .filter(|geometry| geometry.overlaps(proposed))
                .map(|geometry| geometry.loc.x + geometry.size.w)
                .max()
            else {
                positions[index] = proposed.loc;
                occupied.push(proposed);
                break;
            };
            x = next_x;
        }
    }
    positions
}

fn connector_name(connector: &connector::Info) -> String {
    format!(
        "{}-{}",
        connector.interface().as_str(),
        connector.interface_id()
    )
}

/// The connector's preferred mode, falling back to its first advertised one.
fn preferred_mode(connector: &connector::Info) -> Option<DrmMode> {
    connector
        .modes()
        .iter()
        .find(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| connector.modes().first())
        .copied()
}

fn select_mode(
    connector: &connector::Info,
    config: Option<&MonitorConfig>,
) -> Result<DrmMode, io::Error> {
    let preferred = preferred_mode(connector)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "connector has no modes"))?;
    match requested_mode(connector, config) {
        Ok(mode) => Ok(mode),
        Err(error) => {
            warn!(connector = %connector_name(connector), %error, "requested mode unavailable; using preferred mode");
            Ok(preferred)
        }
    }
}

fn requested_mode(
    connector: &connector::Info,
    config: Option<&MonitorConfig>,
) -> Result<DrmMode, io::Error> {
    let preferred = preferred_mode(connector)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "connector has no modes"))?;
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
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "requested resolution {}x{} is unavailable",
                target_size.0, target_size.1
            ),
        ));
    };
    if config.refresh.is_some() && !refresh_matches(Mode::from(selected).refresh, target_refresh) {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "requested refresh {:.3} Hz is unavailable at {}x{}",
                target_refresh as f64 / 1000.0,
                target_size.0,
                target_size.1
            ),
        ))
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
        assert_eq!(best_mode(&modes, (3840, 2160), 120_000), None);
        assert!(refresh_matches(199_999, 200_000));
        assert!(refresh_matches(199_500, 200_000));
        assert!(!refresh_matches(199_000, 200_000));
    }

    #[test]
    fn a_new_output_lands_in_the_same_gap_a_reload_would_pack_it_into() {
        // Hot-plug and config-reload used to run different algorithms: this
        // placed at the rightmost edge, `automatic_positions` gap-packed. A
        // 1920-wide hole at x=0 has to swallow a 1920-wide monitor either way.
        let existing = [Rectangle::new(
            (1920, 0).into(),
            Size::<i32, Logical>::from((2560, 1440)),
        )];
        assert_eq!(
            automatic_position(existing.into_iter(), (1920, 1080).into()),
            Point::<i32, Logical>::from((0, 0))
        );
        assert_eq!(
            automatic_position(existing.into_iter(), (3840, 2160).into()),
            Point::<i32, Logical>::from((4480, 0))
        );
        assert_eq!(
            automatic_position(std::iter::empty(), (1920, 1080).into()),
            Point::<i32, Logical>::from((0, 0))
        );
    }

    #[test]
    fn packs_automatic_outputs_around_fixed_positions() {
        let outputs = [
            ((1920, 1080).into(), None),
            ((2560, 1440).into(), Some((2560, 0).into())),
            ((1920, 1080).into(), None),
        ];

        assert_eq!(
            automatic_positions(&outputs),
            [(0, 0).into(), (2560, 0).into(), (5120, 0).into()]
        );
    }

    #[test]
    fn computes_scaled_rotated_logical_size() {
        let mode = Mode {
            size: (2560, 1440).into(),
            refresh: 60_000,
        };

        assert_eq!(
            logical_output_size(mode, Transform::_90, 1.5),
            (960, 1707).into()
        );
    }
}
