//! Screen locking (`ext-session-lock-v1`) and idle reporting
//! (`ext-idle-notify-v1`), the two protocols a shell's lock screen and idle
//! manager are built on.
//!
//! # What locked means
//!
//! From the moment a client asks to lock until it unlocks, the only things on
//! screen are its lock surfaces over black, and the only client that gets
//! input is the lock surface on the focused output. Nothing else is drawn - not
//! windows, not layer surfaces, not the overview - and no keybinding runs
//! except `quit`, the same escape hatch keyboard-shortcut inhibitors leave.
//! Per-window capture is refused; output capture sees the lock screen.
//!
//! The protocol only lets a locker believe the screen is locked once it truly
//! is, so `locked` is sent after every output has presented a frame drawn in
//! the locked state. A locker that crashes leaves the session locked and
//! black; starting a new locker takes over.

use std::collections::{HashMap, HashSet};

use smithay::{
    output::Output,
    reexports::wayland_server::{
        Resource,
        protocol::{wl_output::WlOutput, wl_surface::WlSurface},
    },
    utils::{Logical, Point},
    wayland::{
        idle_inhibit::IdleInhibitHandler,
        idle_notify::{IdleNotifierHandler, IdleNotifierState},
        session_lock::{LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker},
    },
};

use crate::state::Paw2;

pub struct SessionState {
    pub lock_manager: SessionLockManagerState,
    pub idle_notifier: IdleNotifierState<Paw2>,
    lock: LockStatus,
    /// One lock surface per output, by output name.
    lock_surfaces: HashMap<String, LockSurface>,
    /// Surfaces holding an `zwp_idle_inhibitor_v1`.
    idle_inhibitors: Vec<WlSurface>,
}

enum LockStatus {
    Unlocked,
    /// Blanking the outputs. `queued` holds outputs that have queued a locked
    /// frame, `presented` those that have shown one.
    Pending {
        locker: SessionLocker,
        queued: HashSet<String>,
        presented: HashSet<String>,
    },
    Locked,
}

impl SessionState {
    pub fn new(
        display: &smithay::reexports::wayland_server::DisplayHandle,
        loop_handle: smithay::reexports::calloop::LoopHandle<'static, Paw2>,
    ) -> Self {
        Self {
            // Not for sandboxed clients: a Flatpak app has no business locking
            // the session or watching how long the user has been away.
            lock_manager: SessionLockManagerState::new::<Paw2, _>(
                display,
                crate::state::is_unsandboxed,
            ),
            idle_notifier: IdleNotifierState::new(display, loop_handle),
            lock: LockStatus::Unlocked,
            lock_surfaces: HashMap::new(),
            idle_inhibitors: Vec::new(),
        }
    }
}

impl Paw2 {
    /// Whether the session is locked or locking. Everything that hides the
    /// desktop keys off this, not off the locker having been told `locked`.
    pub fn session_locked(&self) -> bool {
        !matches!(self.session.lock, LockStatus::Unlocked)
    }

    /// The lock surface that should have keyboard focus: the focused output's,
    /// or any if that output has none yet.
    pub(crate) fn lock_keyboard_focus(&self) -> Option<WlSurface> {
        let focused = self.default_output().map(|output| output.name());
        focused
            .and_then(|name| self.session.lock_surfaces.get(&name))
            .or_else(|| self.session.lock_surfaces.values().next())
            .map(|surface| surface.wl_surface().clone())
    }

    /// The lock surface under `position`, with its global origin. Lock surfaces
    /// cover their output, so this is just the output's surface.
    pub(crate) fn lock_surface_under(
        &self,
        position: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.outputs().into_iter().find_map(|output| {
            let geometry = self.output_geometry(&output)?;
            if !geometry.to_f64().contains(position) {
                return None;
            }
            let surface = self.session.lock_surfaces.get(&output.name())?;
            Some((surface.wl_surface().clone(), geometry.loc.to_f64()))
        })
    }

    pub(crate) fn lock_surface_for(&self, output: &Output) -> Option<&LockSurface> {
        self.session.lock_surfaces.get(&output.name())
    }

    /// Size every lock surface to its output again, after a mode, scale or
    /// layout change.
    pub(crate) fn configure_lock_surfaces(&self) {
        for output in self.outputs() {
            if let Some(surface) = self.session.lock_surfaces.get(&output.name()) {
                self.configure_lock_surface(surface, &output);
            }
        }
    }

    fn configure_lock_surface(&self, surface: &LockSurface, output: &Output) {
        let Some(geometry) = self.output_geometry(output) else {
            return;
        };
        let size = (geometry.size.w.max(1) as u32, geometry.size.h.max(1) as u32);
        surface.with_pending_state(|state| state.size = Some(size.into()));
        surface.send_configure();
        // Lock surfaces are in no space or layer map, so nothing else tells
        // the client what scale to draw at; without this a HiDPI lock screen
        // renders at 1x and is upscaled.
        let scale = output.current_scale();
        smithay::wayland::compositor::with_states(surface.wl_surface(), |data| {
            smithay::wayland::compositor::send_surface_state(
                surface.wl_surface(),
                data,
                scale.integer_scale(),
                output.current_transform(),
            );
            smithay::wayland::fractional_scale::with_fractional_scale(data, |fractional| {
                fractional.set_preferred_scale(scale.fractional_scale());
            });
        });
    }

    /// Frame callbacks for `output`'s lock surface; it is drawn by nothing
    /// that sends them on its behalf.
    pub(crate) fn send_lock_frames(&self, output: &Output, time: std::time::Duration) {
        if let Some(surface) = self.lock_surface_for(output) {
            smithay::desktop::utils::send_frames_surface_tree(
                surface.wl_surface(),
                output,
                time,
                Some(std::time::Duration::ZERO),
                |_, _| Some(output.clone()),
            );
        }
    }

    /// The DRM backend queued a frame for `output`. Called after every queued
    /// frame; only one queued while locking counts towards confirming it.
    pub(crate) fn session_lock_frame_queued(&mut self, output: &Output) {
        if let LockStatus::Pending { queued, .. } = &mut self.session.lock {
            queued.insert(output.name());
        }
    }

    /// `output` has shown the frame it queued last. Once every output has
    /// shown a locked frame, the locker is told the session is locked.
    pub(crate) fn session_lock_frame_presented(&mut self, output: &Output) {
        let outputs = self.outputs();
        let LockStatus::Pending {
            queued, presented, ..
        } = &mut self.session.lock
        else {
            return;
        };
        if queued.contains(&output.name()) {
            presented.insert(output.name());
        }
        let blanked = outputs
            .iter()
            .all(|output| presented.contains(&output.name()));
        if blanked {
            self.confirm_lock();
        }
    }

    fn confirm_lock(&mut self) {
        if let LockStatus::Pending { locker, .. } =
            std::mem::replace(&mut self.session.lock, LockStatus::Locked)
        {
            locker.lock();
            tracing::info!("session locked");
        }
    }

    fn update_idle_inhibition(&mut self) {
        self.session
            .idle_inhibitors
            .retain(|surface| surface.is_alive());
        let inhibited = !self.session.idle_inhibitors.is_empty();
        self.session.idle_notifier.set_is_inhibited(inhibited);
    }

    /// Any input at all resets every client's idle timers.
    pub(crate) fn note_user_activity(&mut self) {
        let seat = self.seat.clone();
        self.session.idle_notifier.notify_activity(&seat);
    }
}

impl SessionLockHandler for Paw2 {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session.lock_manager
    }

    fn lock(&mut self, locker: SessionLocker) {
        // A new locker taking over from one that crashed: the screen is
        // already blank, so there is nothing to wait for.
        if matches!(self.session.lock, LockStatus::Locked) {
            locker.lock();
            tracing::info!("session lock taken over by a new client");
            return;
        }
        if matches!(self.session.lock, LockStatus::Pending { .. }) {
            // Dropping the locker tells this client it lost the race.
            return;
        }
        tracing::info!("locking the session");
        self.session.lock = LockStatus::Pending {
            locker,
            queued: HashSet::new(),
            presented: HashSet::new(),
        };
        // Nothing behind the lock may keep input. A popup or drag grab would
        // keep routing events to its client whatever has focus, so end those
        // first; then close the overviews and move focus to the lock.
        let keyboard = self.keyboard();
        keyboard.unset_grab(self);
        let pointer = self.pointer();
        pointer.unset_grab(
            self,
            smithay::utils::SERIAL_COUNTER.next_serial(),
            self.clock.now().as_millis(),
        );
        if self.overview_active() {
            self.close_overview();
        }
        if self.scratchpad_overview_active() {
            self.close_scratchpad_overview();
        }
        self.sync_focus();
        if self.outputs().is_empty() {
            self.confirm_lock();
        }
        crate::drm::redraw(self);
    }

    fn unlock(&mut self) {
        tracing::info!("session unlocked");
        self.session.lock = LockStatus::Unlocked;
        self.session.lock_surfaces.clear();
        self.sync_focus();
        crate::drm::redraw(self);
    }

    fn new_surface(&mut self, surface: LockSurface, wl_output: WlOutput) {
        let Some(output) = Output::from_resource(&wl_output) else {
            return;
        };
        output.enter(surface.wl_surface());
        self.configure_lock_surface(&surface, &output);
        self.session.lock_surfaces.insert(output.name(), surface);
        self.sync_focus();
    }
}

impl IdleNotifierHandler for Paw2 {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        &mut self.session.idle_notifier
    }
}

impl IdleInhibitHandler for Paw2 {
    // A game or video player holds an inhibitor while it plays; until every
    // one is gone, no idle client is told the user went away.
    fn inhibit(&mut self, surface: WlSurface) {
        if !self.session.idle_inhibitors.contains(&surface) {
            self.session.idle_inhibitors.push(surface);
        }
        self.update_idle_inhibition();
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        self.session.idle_inhibitors.retain(|held| *held != surface);
        self.update_idle_inhibition();
    }
}
