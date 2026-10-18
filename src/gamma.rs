//! `wlr-gamma-control-unstable-v1`: lets a night-light tool load a colour
//! ramp into an output's hardware gamma table.
//!
//! One client at a time owns an output's ramp; a second asking for the same
//! output is told `failed`. When the owner lets go - by destroying the control
//! or by exiting - the output goes back to a linear ramp, so a crashed
//! night-light can never leave the screen tinted. Ramps are reloaded when the
//! session comes back from a VT switch, which may have reset the hardware.
//!
//! Only the DRM backend has hardware gamma; nested, every request fails.

use std::{fs::File, os::unix::fs::FileExt};

use smithay::{
    output::Output,
    reexports::{
        wayland_protocols_wlr::gamma_control::v1::server::{
            zwlr_gamma_control_manager_v1::{self, ZwlrGammaControlManagerV1},
            zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
            backend::{ClientId, GlobalId},
        },
    },
};

use crate::state::Paw2;

pub struct GammaState {
    _global: GlobalId,
    controls: Vec<GammaControl>,
}

struct GammaControl {
    output: String,
    resource: ZwlrGammaControlV1,
    /// Red, green and blue tables back to back, `size` entries each. `None`
    /// until the client sets one.
    ramp: Option<Vec<u16>>,
}

/// The output a control was created for, carried as the resource's data.
pub struct GammaControlData {
    output: String,
    size: u32,
}

impl GammaState {
    pub fn new(display: &DisplayHandle) -> Self {
        Self {
            _global: display.create_global::<Paw2, ZwlrGammaControlManagerV1, _>(1, ()),
            controls: Vec::new(),
        }
    }

    /// Every ramp a client has set, for reloading after a VT switch.
    pub fn ramps(&self) -> impl Iterator<Item = (&str, &[u16])> {
        self.controls.iter().filter_map(|control| {
            control
                .ramp
                .as_deref()
                .map(|ramp| (control.output.as_str(), ramp))
        })
    }
}

impl Paw2 {
    /// Hand an output back to the linear ramp and forget its control.
    fn release_gamma(&mut self, output: &str) {
        let before = self.gamma.controls.len();
        self.gamma
            .controls
            .retain(|control| control.output != output);
        if self.gamma.controls.len() != before
            && let Some(output) = self.outputs().into_iter().find(|o| o.name() == output)
            && let Err(error) = crate::drm::set_gamma(self, &output, None)
        {
            tracing::warn!(%error, output = %output.name(), "could not restore the gamma ramp");
        }
    }

    /// An output is going away: its control can no longer do anything.
    pub(crate) fn gamma_output_removed(&mut self, output: &Output) {
        let name = output.name();
        for control in self.gamma.controls.iter().filter(|c| c.output == name) {
            control.resource.failed();
        }
        self.gamma.controls.retain(|control| control.output != name);
    }
}

/// Visible only to unsandboxed clients: a sandboxed app has no business
/// recolouring the whole screen.
impl GlobalDispatch<ZwlrGammaControlManagerV1, ()> for Paw2 {
    fn bind(
        _state: &mut Self,
        _display: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrGammaControlManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }

    fn can_view(client: Client, _global_data: &()) -> bool {
        crate::state::is_unsandboxed(&client)
    }
}

impl Dispatch<ZwlrGammaControlManagerV1, ()> for Paw2 {
    fn request(
        state: &mut Self,
        _client: &Client,
        _manager: &ZwlrGammaControlManagerV1,
        request: zwlr_gamma_control_manager_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let zwlr_gamma_control_manager_v1::Request::GetGammaControl { id, output } = request else {
            return;
        };
        let output = Output::from_resource(&output);
        let name = output.as_ref().map(Output::name).unwrap_or_default();
        state
            .gamma
            .controls
            .retain(|control| control.resource.is_alive());
        let taken = state
            .gamma
            .controls
            .iter()
            .any(|control| control.output == name);
        let size = output
            .as_ref()
            .filter(|_| !taken)
            .and_then(|output| crate::drm::gamma_size(state, output));
        let resource = data_init.init(
            id,
            GammaControlData {
                output: name.clone(),
                size: size.unwrap_or(0),
            },
        );
        let Some(size) = size else {
            resource.failed();
            return;
        };
        resource.gamma_size(size);
        state.gamma.controls.push(GammaControl {
            output: name,
            resource,
            ramp: None,
        });
    }
}

impl Dispatch<ZwlrGammaControlV1, GammaControlData> for Paw2 {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrGammaControlV1,
        request: zwlr_gamma_control_v1::Request,
        data: &GammaControlData,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_gamma_control_v1::Request::SetGamma { fd } => {
                if !state
                    .gamma
                    .controls
                    .iter()
                    .any(|control| control.resource == *resource)
                {
                    // Already failed; the client has not noticed yet.
                    return;
                }
                let applied = read_ramp(File::from(fd), data.size).and_then(|ramp| {
                    let output = state
                        .outputs()
                        .into_iter()
                        .find(|output| output.name() == data.output)
                        .ok_or_else(|| "the output is gone".to_string())?;
                    crate::drm::set_gamma(state, &output, Some(&ramp))?;
                    Ok(ramp)
                });
                match applied {
                    Ok(ramp) => {
                        if let Some(control) = state
                            .gamma
                            .controls
                            .iter_mut()
                            .find(|control| control.resource == *resource)
                        {
                            control.ramp = Some(ramp);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, output = %data.output, "rejected a gamma ramp");
                        resource.failed();
                        state.release_gamma(&data.output);
                    }
                }
            }
            zwlr_gamma_control_v1::Request::Destroy => state.release_gamma_of(resource, data),
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ZwlrGammaControlV1,
        data: &GammaControlData,
    ) {
        state.release_gamma_of(resource, data);
    }
}

impl Paw2 {
    /// Release `data.output` only if `resource` is the control that owns it; a
    /// refused second control must not reset the owner's ramp.
    fn release_gamma_of(&mut self, resource: &ZwlrGammaControlV1, data: &GammaControlData) {
        let owns = self
            .gamma
            .controls
            .iter()
            .any(|control| control.resource == *resource);
        if owns {
            self.release_gamma(&data.output);
        }
    }
}

/// Read a ramp of `size` entries per channel from the client's file.
///
/// Read from offset 0 rather than the file position: a client that wrote the
/// table and passed the fd without seeking back would otherwise hand over an
/// empty read.
fn read_ramp(file: File, size: u32) -> Result<Vec<u16>, String> {
    let entries = size as usize * 3;
    let mut bytes = vec![0u8; entries * 2];
    file.read_exact_at(&mut bytes, 0)
        .map_err(|error| format!("could not read the ramp: {error}"))?;
    Ok(bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_ne_bytes(*pair))
        .collect())
}

/// The identity ramp: what an output shows when nobody has set one.
pub fn linear_ramp(size: usize) -> Vec<u16> {
    let last = size.saturating_sub(1).max(1);
    let channel = (0..size).map(|index| (index * u16::MAX as usize / last) as u16);
    channel
        .clone()
        .chain(channel.clone())
        .chain(channel)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_linear_ramp_runs_from_black_to_white_on_every_channel() {
        let ramp = linear_ramp(4);
        assert_eq!(ramp.len(), 12);
        for channel in ramp.as_chunks::<4>().0 {
            assert_eq!(*channel, [0, 21845, 43690, 65535]);
        }
    }

    #[test]
    fn a_ramp_is_read_from_the_start_of_the_file_whatever_its_position() {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = tempfile_in_memory();
        let ramp: Vec<u16> = (0..6).map(|value| value * 1000).collect();
        for value in &ramp {
            file.write_all(&value.to_ne_bytes()).unwrap();
        }
        // Left at the end, as a client that just wrote it would leave it.
        file.seek(SeekFrom::End(0)).unwrap();
        assert_eq!(read_ramp(file, 2).unwrap(), ramp);
    }

    #[test]
    fn a_short_ramp_is_rejected() {
        use std::io::Write;
        let mut file = tempfile_in_memory();
        file.write_all(&[0, 1, 2]).unwrap();
        assert!(read_ramp(file, 2).is_err());
    }

    fn tempfile_in_memory() -> File {
        use smithay::reexports::rustix::fs::{MemfdFlags, memfd_create};
        File::from(memfd_create("paw2-gamma-test", MemfdFlags::CLOEXEC).unwrap())
    }
}
