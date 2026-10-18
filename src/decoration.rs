//! Paw2's own titlebars, negotiated with `xdg-decoration`.
//!
//! An app that binds the protocol is told to leave decorating to Paw2 while
//! `appearance.titlebars` is on, and to draw its own while it is off. Apps that
//! never bind it - GTK, which always draws its own header bar - are left alone,
//! so no window ever gets two bars.

use smithay::{
    desktop::Window,
    reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode,
    wayland::shell::xdg::{ToplevelSurface, decoration::XdgDecorationHandler},
};

use crate::state::Paw2;

impl Paw2 {
    /// The mode Paw2 answers every decoration request with.
    fn decoration_mode(&self) -> Mode {
        if self.config.appearance.titlebars {
            Mode::ServerSide
        } else {
            Mode::ClientSide
        }
    }

    /// Tell `toplevel` who decorates it. Returns whether that changed.
    fn configure_decoration(&self, toplevel: &ToplevelSurface) -> bool {
        let mode = self.decoration_mode();
        let changed = toplevel.with_pending_state(|state| {
            let changed = state.decoration_mode != Some(mode);
            state.decoration_mode = Some(mode);
            changed
        });
        // Before its first configure the mode simply rides along with it.
        if changed && toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
        changed
    }

    /// After a config reload: re-answer every app that asked, in case
    /// `titlebars` was switched.
    pub(crate) fn reconcile_decorations(&mut self) {
        let negotiated = self
            .xdg_shell_state
            .toplevel_surfaces()
            .iter()
            .filter(|toplevel| toplevel.with_pending_state(|state| state.decoration_mode.is_some()))
            .cloned()
            .collect::<Vec<_>>();
        for toplevel in negotiated {
            self.configure_decoration(&toplevel);
        }
    }
}

/// Whether Paw2 draws `window`'s titlebar: it asked for decorations while
/// titlebars are on. X11 windows decorate themselves.
pub(crate) fn has_titlebar(window: &Window) -> bool {
    window.toplevel().is_some_and(|toplevel| {
        toplevel.with_pending_state(|state| state.decoration_mode == Some(Mode::ServerSide))
    })
}

impl XdgDecorationHandler for Paw2 {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        if self.configure_decoration(&toplevel) {
            self.relayout(true);
        }
    }

    /// Paw2 decides, whatever the app would prefer: its bar is the setting.
    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: Mode) {
        if self.configure_decoration(&toplevel) {
            self.relayout(true);
        }
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        if self.configure_decoration(&toplevel) {
            self.relayout(true);
        }
    }
}
