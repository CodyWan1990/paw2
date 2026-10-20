//! Paw2's own part in the clipboard, on top of smithay's selection protocols.
//!
//! Clients normally hand selections to each other directly; Paw2 steps in for
//! two things a client cannot do for itself:
//!
//! - **Copy on select.** With `[input] copy_on_select`, highlighting text also
//!   puts it on the clipboard. Paw2 reads the new primary selection and offers
//!   it as a clipboard selection of its own, so no helper such as
//!   `wl-paste --watch wl-copy` has to run, and none opens a throwaway window to
//!   do it.
//! - **XWayland.** X11 and Wayland apps share one clipboard and one primary
//!   selection. A Wayland copy is announced to the X server, and a Wayland app
//!   pasting what an X11 app copied is served through the X11 window manager.

use std::{
    io::{ErrorKind, Read, Write},
    os::fd::OwnedFd,
    sync::Arc,
};

use smithay::{
    reexports::calloop::{Interest, Mode, PostAction, RegistrationToken, generic::Generic},
    wayland::selection::{
        SelectionSource, SelectionTarget, data_device::set_data_device_selection,
        primary_selection::request_primary_client_selection,
    },
};

use crate::state::Paw2;

/// Who provides a selection that Paw2 set on the seat itself, rather than one a
/// Wayland client set. Smithay hands this back whenever such a selection is read.
#[derive(Clone, Debug)]
pub enum SelectionOwner {
    /// An X11 app; reads go through the X11 window manager.
    X11,
    /// Text Paw2 copied, e.g. the primary selection under copy on select.
    Paw2(Arc<[u8]>),
}

/// The text formats copy on select reads and offers, best first. The X11 names
/// are what XWayland apps ask for.
const TEXT_MIME_TYPES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
    "STRING",
];

/// Anything bigger is not a highlighted piece of text, and is not worth holding.
const MAX_COPIED_TEXT: usize = 16 * 1024 * 1024;

/// The copy on select read in flight, if any. A newer selection replaces it.
#[derive(Default)]
pub struct ClipboardState {
    pending_copy: Option<RegistrationToken>,
}

impl Paw2 {
    /// A Wayland client set or cleared a selection.
    pub(crate) fn client_selection_changed(
        &mut self,
        target: SelectionTarget,
        source: Option<SelectionSource>,
    ) {
        let mime_types = source.map(|source| source.mime_types());
        if let Some(xwm) = self.xwm.as_mut()
            && let Err(error) = xwm.new_selection(target, mime_types.clone())
        {
            tracing::warn!(?error, ?target, "could not hand a Wayland selection to X11");
        }
        if target == SelectionTarget::Primary
            && let Some(mime_type) = mime_types.as_deref().and_then(preferred_text_mime)
        {
            // Smithay reports a selection before it stores it, so reading it
            // now would read the previous one. Idle runs once this dispatch is done.
            self.loop_handle.insert_idle(move |state| {
                state.copy_selected_text(|state, fd| {
                    request_primary_client_selection(&state.seat, mime_type, fd).is_ok()
                });
            });
        }
    }

    /// An X11 app highlighted text: under copy on select it goes to the
    /// clipboard too, read back through the X11 window manager.
    pub(crate) fn x11_primary_changed(&mut self, mime_types: &[String]) {
        let Some(mime_type) = preferred_text_mime(mime_types) else {
            return;
        };
        self.copy_selected_text(|state, fd| {
            state.xwm.as_mut().is_some_and(|xwm| {
                xwm.send_selection(SelectionTarget::Primary, mime_type, fd)
                    .is_ok()
            })
        });
    }

    /// A client is reading a selection Paw2 set on the seat.
    pub(crate) fn send_owned_selection(
        &mut self,
        target: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
        owner: &SelectionOwner,
    ) {
        match owner {
            SelectionOwner::X11 => {
                let Some(xwm) = self.xwm.as_mut() else {
                    return;
                };
                if let Err(error) = xwm.send_selection(target, mime_type, fd) {
                    tracing::warn!(?error, ?target, "could not read an X11 selection");
                }
            }
            SelectionOwner::Paw2(text) => {
                // A reader that never drains the pipe must not stall the
                // compositor, so the write gets its own short-lived thread.
                let text = Arc::clone(text);
                std::thread::spawn(move || {
                    let _ = std::fs::File::from(fd).write_all(&text);
                });
            }
        }
    }

    /// Read the newly highlighted text and put it on the clipboard. `request`
    /// asks the selection's owner to write it into the given fd, and says
    /// whether it could.
    fn copy_selected_text(&mut self, request: impl FnOnce(&mut Self, OwnedFd) -> bool) {
        if !self.config.input.copy_on_select || !self.config.input.middle_click_paste {
            return;
        }
        // Only the latest selection is worth finishing.
        if let Some(token) = self.clipboard.pending_copy.take() {
            self.loop_handle.remove(token);
        }
        let Ok((reader, writer)) = std::io::pipe() else {
            tracing::warn!("could not open a pipe for copy on select");
            return;
        };
        if !request(self, writer.into()) {
            return;
        }

        let mut text = Vec::new();
        let registered = self.loop_handle.insert_source(
            Generic::new(reader, Interest::READ, Mode::Level),
            move |_, reader, state| {
                let mut reader: &std::io::PipeReader = reader;
                let mut chunk = [0u8; 4096];
                match reader.read(&mut chunk) {
                    Ok(0) => {
                        state.clipboard.pending_copy = None;
                        state.set_copied_text(std::mem::take(&mut text));
                        Ok(PostAction::Remove)
                    }
                    Ok(read) if text.len() + read > MAX_COPIED_TEXT => {
                        state.clipboard.pending_copy = None;
                        Ok(PostAction::Remove)
                    }
                    Ok(read) => {
                        text.extend_from_slice(&chunk[..read]);
                        Ok(PostAction::Continue)
                    }
                    Err(error) if error.kind() == ErrorKind::Interrupted => {
                        Ok(PostAction::Continue)
                    }
                    Err(_) => {
                        state.clipboard.pending_copy = None;
                        Ok(PostAction::Remove)
                    }
                }
            },
        );
        match registered {
            Ok(token) => self.clipboard.pending_copy = Some(token),
            Err(error) => tracing::warn!(?error, "could not watch the copy on select pipe"),
        }
    }

    fn set_copied_text(&mut self, text: Vec<u8>) {
        if text.is_empty() {
            return;
        }
        let mime_types: Vec<String> = TEXT_MIME_TYPES.map(String::from).into();
        set_data_device_selection(
            &self.display_handle,
            &self.seat,
            mime_types.clone(),
            SelectionOwner::Paw2(text.into()),
        );
        // Smithay only reports selections clients set, so X11 is told here.
        if let Some(xwm) = self.xwm.as_mut()
            && let Err(error) = xwm.new_selection(SelectionTarget::Clipboard, Some(mime_types))
        {
            tracing::warn!(?error, "could not hand the copied text to X11");
        }
    }
}

fn preferred_text_mime(offered: &[String]) -> Option<String> {
    TEXT_MIME_TYPES
        .iter()
        .find(|mime_type| offered.iter().any(|offer| offer == *mime_type))
        .map(|mime_type| (*mime_type).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_utf8_text() {
        let offered = ["STRING", "text/plain", "text/plain;charset=utf-8"].map(String::from);
        assert_eq!(
            preferred_text_mime(&offered).as_deref(),
            Some("text/plain;charset=utf-8")
        );
    }

    #[test]
    fn skips_selections_without_text() {
        let offered = ["image/png", "text/uri-list"].map(String::from);
        assert_eq!(preferred_text_mime(&offered), None);
    }
}
