//! Minimizing: taking a window out of its workspace until it is asked for
//! again - from the shell's taskbar or dock (`focus-window`), the
//! `restore_minimized` binding, or the app itself.
//!
//! A minimized window stays alive and listed in the IPC snapshot under the
//! workspace it left, so a panel can still show it. While it is out, a
//! Wayland window is told it is suspended and an X11 one that it is hidden, so
//! a game or a video stops rendering frames nobody can see.

use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;

use super::*;

/// A window out of its workspace, and where it goes back to.
pub(super) struct MinimizedWindow {
    pub(super) window: Window,
    pub(super) output: String,
    pub(super) workspace: usize,
    pub(super) floating: bool,
}

impl Paw2 {
    /// Take `window` out of its workspace. Scratchpads have their own hiding,
    /// and a window on no monitor has nowhere to come back to, so both are
    /// left alone.
    pub(crate) fn minimize_window(&mut self, window: &Window) {
        if self.is_scratchpad_window(window) {
            return;
        }
        let Some((monitor, workspace)) = self.scroller.monitors.iter_mut().find_map(|monitor| {
            let workspace = monitor
                .workspaces
                .iter()
                .position(|workspace| workspace.windows.contains(window))?;
            Some((monitor, workspace))
        }) else {
            return;
        };
        let floating = monitor.workspaces[workspace].is_floating(window);
        let output = monitor.name.clone();
        monitor.remove_where(|item| item == window);
        set_suspended(window, true);
        self.scroller.minimized.push(MinimizedWindow {
            window: window.clone(),
            output,
            workspace,
            floating,
        });
        self.scroller.hovered_bar_button = None;
        self.relayout(true);
        self.sync_focus();
        self.ipc.touch();
        crate::drm::redraw(self);
    }

    /// Put window `id` back where it was minimized from and focus it. Returns
    /// false when it is not minimized.
    pub(crate) fn restore_window(&mut self, id: usize) -> bool {
        let Some(index) = self
            .scroller
            .minimized
            .iter()
            .position(|minimized| minimized.window.id() == id)
        else {
            return false;
        };
        let minimized = self.scroller.minimized.remove(index);
        // Its monitor may have been unplugged since; then it comes back where
        // the user is looking.
        let output = Some(minimized.output)
            .filter(|output| self.scroller.monitor(output).is_some())
            .or_else(|| self.scroller.focused_output.clone())
            .or_else(|| {
                self.scroller
                    .monitors
                    .first()
                    .map(|monitor| monitor.name.clone())
            });
        set_suspended(&minimized.window, false);
        match output
            .as_deref()
            .and_then(|output| self.scroller.monitor_mut(output))
        {
            Some(monitor) => monitor.workspaces[minimized.workspace]
                .add(minimized.window.clone(), minimized.floating),
            None => self.scroller.parked[minimized.workspace]
                .add(minimized.window.clone(), minimized.floating),
        }
        if let Some(output) = &output {
            self.focus_output_by_name(output);
            self.activate_workspace(output, minimized.workspace);
        }
        self.relayout(true);
        self.focus_window(&minimized.window);
        self.ipc.touch();
        crate::drm::redraw(self);
        true
    }

    /// Restore the most recently minimized window. Returns whether there was
    /// one.
    pub(crate) fn restore_last_minimized(&mut self) -> bool {
        let Some(id) = self
            .scroller
            .minimized
            .last()
            .map(|minimized| minimized.window.id())
        else {
            return false;
        };
        self.restore_window(id)
    }

    /// Drop a minimized window that closed. Returns it if it was one.
    pub(super) fn forget_minimized(
        &mut self,
        predicate: impl Fn(&Window) -> bool,
    ) -> Option<Window> {
        let index = self
            .scroller
            .minimized
            .iter()
            .position(|minimized| predicate(&minimized.window))?;
        Some(self.scroller.minimized.remove(index).window)
    }
}

/// Tell the app it is out of sight, or back.
fn set_suspended(window: &Window, suspended: bool) {
    if let Some(toplevel) = window.toplevel() {
        toplevel.with_pending_state(|state| {
            if suspended {
                state.states.set(xdg_toplevel::State::Suspended);
            } else {
                state.states.unset(xdg_toplevel::State::Suspended);
            }
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    } else if let Some(surface) = window.x11_surface()
        && let Err(error) = surface.set_hidden(suspended)
    {
        tracing::warn!(?error, "failed to mark X11 window hidden");
    }
}
