use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use smithay::{
    backend::allocator::{Buffer, Fourcc, dmabuf::Dmabuf},
    output::Output,
    reexports::{
        wayland_protocols_wlr::screencopy::v1::server::{
            zwlr_screencopy_frame_v1::{self, Error, ZwlrScreencopyFrameV1},
            zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
            backend::GlobalId, protocol::wl_buffer::WlBuffer,
        },
    },
    utils::{Logical, Physical, Rectangle},
    wayland::dmabuf::get_dmabuf,
};

use crate::state::Paw2;

pub struct ScreencopyState {
    _global: GlobalId,
    pending: Vec<Capture>,
}

pub(crate) struct Capture {
    pub frame: ZwlrScreencopyFrameV1,
    pub output: Output,
    pub region: Rectangle<i32, Physical>,
    pub buffer: Dmabuf,
    pub overlay_cursor: bool,
    pub with_damage: bool,
}

#[derive(Clone)]
pub struct ManagerData;

pub struct FrameData(Mutex<FrameState>);

struct FrameState {
    output: Option<Output>,
    region: Rectangle<i32, Physical>,
    format: Fourcc,
    overlay_cursor: bool,
    used: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ExecutableId {
    device: u64,
    inode: u64,
}

impl ScreencopyState {
    pub fn new(display: &DisplayHandle) -> Self {
        let executables = Arc::new(trusted_executables());
        if executables.is_empty() {
            tracing::warn!("xdg-desktop-portal-wlr was not found; screencopy is disabled");
        }
        let filter_display = display.clone();
        let filter_executables = executables.clone();
        let global = display.create_global::<Paw2, ZwlrScreencopyManagerV1, _>(
            3,
            (filter_display, filter_executables),
        );
        Self {
            _global: global,
            pending: Vec::new(),
        }
    }

    pub(crate) fn take_for_output(&mut self, output: &Output) -> Vec<Capture> {
        let mut captures = Vec::new();
        let mut index = 0;
        while index < self.pending.len() {
            if self.pending[index].output == *output {
                captures.push(self.pending.swap_remove(index));
            } else {
                index += 1;
            }
        }
        captures
    }

    pub fn fail_output(&mut self, output: &Output) {
        for capture in self.take_for_output(output) {
            capture.frame.failed();
        }
    }
}

fn trusted_executables() -> Vec<ExecutableId> {
    let mut candidates = vec![
        PathBuf::from("/usr/libexec/xdg-desktop-portal-wlr"),
        PathBuf::from("/usr/lib/xdg-desktop-portal-wlr"),
    ];
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(
            std::env::split_paths(&path).map(|directory| directory.join("xdg-desktop-portal-wlr")),
        );
    }
    let mut ids = Vec::new();
    for id in candidates.iter().filter_map(|path| executable_id(path)) {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

fn executable_id(path: &Path) -> Option<ExecutableId> {
    let metadata = fs::metadata(path).ok()?;
    Some(ExecutableId {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn trusted_client(client: &Client, display: &DisplayHandle, trusted: &[ExecutableId]) -> bool {
    client
        .get_credentials(display)
        .ok()
        .and_then(|credentials| {
            executable_id(&PathBuf::from(format!("/proc/{}/exe", credentials.pid)))
        })
        .is_some_and(|executable| trusted.contains(&executable))
}

type GlobalData = (DisplayHandle, Arc<Vec<ExecutableId>>);

impl GlobalDispatch<ZwlrScreencopyManagerV1, GlobalData> for Paw2 {
    fn bind(
        _state: &mut Self,
        _display: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _global_data: &GlobalData,
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ManagerData);
    }

    fn can_view(client: Client, global_data: &GlobalData) -> bool {
        trusted_client(&client, &global_data.0, &global_data.1)
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ManagerData> for Paw2 {
    fn request(
        state: &mut Self,
        _client: &Client,
        _manager: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _data: &ManagerData,
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput {
                frame,
                overlay_cursor,
                output,
            } => state.new_capture(frame, overlay_cursor != 0, output, None, data_init),
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                output,
                x,
                y,
                width,
                height,
            } => state.new_capture(
                frame,
                overlay_cursor != 0,
                output,
                Some(Rectangle::new((x, y).into(), (width, height).into())),
                data_init,
            ),
            zwlr_screencopy_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Paw2 {
    fn new_capture(
        &mut self,
        frame: New<ZwlrScreencopyFrameV1>,
        overlay_cursor: bool,
        wl_output: smithay::reexports::wayland_server::protocol::wl_output::WlOutput,
        requested_region: Option<Rectangle<i32, Logical>>,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let output = Output::from_resource(&wl_output);
        let capture = output.as_ref().and_then(|output| {
            let geometry = self.output_geometry(output)?;
            let logical_bounds = Rectangle::from_size(geometry.size);
            let logical_region = requested_region
                .unwrap_or(logical_bounds)
                .intersection(logical_bounds)?;
            if logical_region.is_empty() {
                return None;
            }
            let scale = output.current_scale().fractional_scale();
            let region = logical_region.to_f64().to_physical(scale).to_i32_round();
            let format = self.capture_format()?;
            Some((output.clone(), region, format))
        });

        let data = if let Some((output, region, format)) = capture {
            FrameData(Mutex::new(FrameState {
                output: Some(output),
                region,
                format,
                overlay_cursor,
                used: false,
            }))
        } else {
            FrameData(Mutex::new(FrameState {
                output: None,
                region: Rectangle::from_size((0, 0).into()),
                format: Fourcc::Xrgb8888,
                overlay_cursor,
                used: false,
            }))
        };
        let frame = data_init.init(frame, data);
        let data = frame.data::<FrameData>().unwrap().0.lock().unwrap();
        if data.output.is_some() {
            frame.linux_dmabuf(
                data.format as u32,
                data.region.size.w as u32,
                data.region.size.h as u32,
            );
            frame.buffer_done();
        } else {
            frame.failed();
        }
    }

    fn capture_format(&self) -> Option<Fourcc> {
        let formats = self
            .direct
            .as_ref()?
            .renderer
            .egl_context()
            .dmabuf_render_formats();
        [Fourcc::Xrgb8888, Fourcc::Argb8888]
            .into_iter()
            .find(|wanted| formats.iter().any(|format| format.code == *wanted))
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, FrameData> for Paw2 {
    fn request(
        state: &mut Self,
        _client: &Client,
        frame: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        data: &FrameData,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let (buffer, with_damage) = match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => (buffer, false),
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => (buffer, true),
            zwlr_screencopy_frame_v1::Request::Destroy => return,
            _ => return,
        };
        queue_capture(state, frame, data, buffer, with_damage);
    }
}

fn queue_capture(
    state: &mut Paw2,
    frame: &ZwlrScreencopyFrameV1,
    data: &FrameData,
    buffer: WlBuffer,
    with_damage: bool,
) {
    let mut data = data.0.lock().unwrap();
    if data.used {
        frame.post_error(Error::AlreadyUsed, "screencopy frame was already used");
        return;
    }
    data.used = true;
    let Some(output) = data.output.clone() else {
        frame.failed();
        return;
    };
    let Ok(buffer) = get_dmabuf(&buffer) else {
        frame.post_error(Error::InvalidBuffer, "screencopy requires a DMA-BUF");
        return;
    };
    if buffer.size().w != data.region.size.w
        || buffer.size().h != data.region.size.h
        || buffer.format().code != data.format
    {
        frame.post_error(
            Error::InvalidBuffer,
            "DMA-BUF does not match capture constraints",
        );
        return;
    }
    let renderable = state.direct.as_ref().is_some_and(|backend| {
        backend
            .renderer
            .egl_context()
            .dmabuf_render_formats()
            .contains(&buffer.format())
    });
    if !renderable {
        frame.post_error(
            Error::InvalidBuffer,
            "DMA-BUF format or modifier is not renderable",
        );
        return;
    }
    state.screencopy_state.pending.push(Capture {
        frame: frame.clone(),
        output,
        region: data.region,
        buffer: buffer.clone(),
        overlay_cursor: data.overlay_cursor,
        with_damage,
    });
    crate::drm::redraw(state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_identity_is_stable() {
        let current = executable_id(Path::new("/proc/self/exe")).unwrap();
        let again = executable_id(Path::new("/proc/self/exe")).unwrap();
        assert_eq!((current.device, current.inode), (again.device, again.inode));
    }
}
