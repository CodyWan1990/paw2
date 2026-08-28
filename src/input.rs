use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    input::{
        keyboard::{FilterResult, Keysym, ModifiersState, keysyms},
        pointer::{
            AxisFrame, ButtonEvent, Focus, GestureHoldBeginEvent, GestureHoldEndEvent,
            GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
            GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent, GrabStartData,
            MotionEvent, PointerGrab, PointerInnerHandle, RelativeMotionEvent,
        },
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER},
    wayland::pointer_constraints::{PointerConstraint, with_pointer_constraint},
};

use crate::{
    config::{Action, ScrollDirection},
    layout::MouseOperation,
    state::Paw2,
};

const BTN_LEFT: u32 = 0x110;

enum KeyAction {
    Compositor(Action),
    Overview(OverviewKey),
    ScratchpadOverview(ScratchpadOverviewKey),
}

enum ScratchpadOverviewKey {
    Move { horizontal: bool, delta: i32 },
    Confirm,
    Cancel,
}

fn scratchpad_overview_key(keysym: Keysym) -> Option<ScratchpadOverviewKey> {
    match keysym.raw() {
        keysyms::KEY_Left | keysyms::KEY_h => Some(ScratchpadOverviewKey::Move {
            horizontal: true,
            delta: -1,
        }),
        keysyms::KEY_Right | keysyms::KEY_l | keysyms::KEY_Tab => {
            Some(ScratchpadOverviewKey::Move {
                horizontal: true,
                delta: 1,
            })
        }
        keysyms::KEY_Up | keysyms::KEY_k => Some(ScratchpadOverviewKey::Move {
            horizontal: false,
            delta: -1,
        }),
        keysyms::KEY_Down | keysyms::KEY_j => Some(ScratchpadOverviewKey::Move {
            horizontal: false,
            delta: 1,
        }),
        keysyms::KEY_Return | keysyms::KEY_KP_Enter => Some(ScratchpadOverviewKey::Confirm),
        keysyms::KEY_Escape => Some(ScratchpadOverviewKey::Cancel),
        _ => None,
    }
}

enum OverviewKey {
    Workspace(i32),
    Column(i32),
    Row(i32),
    Monitor(i32),
    Confirm,
    Cancel,
}

fn overview_key(modifiers: &ModifiersState, keysym: Keysym) -> Option<OverviewKey> {
    if modifiers.alt {
        return None;
    }
    if modifiers.ctrl {
        if modifiers.shift {
            return None;
        }
        return match keysym.raw() {
            keysyms::KEY_Left | keysyms::KEY_h => Some(OverviewKey::Monitor(-1)),
            keysyms::KEY_Right | keysyms::KEY_l => Some(OverviewKey::Monitor(1)),
            _ => None,
        };
    }
    if modifiers.shift {
        return matches!(keysym.raw(), keysyms::KEY_Tab | keysyms::KEY_ISO_Left_Tab)
            .then_some(OverviewKey::Row(-1));
    }
    match keysym.raw() {
        keysyms::KEY_Left | keysyms::KEY_h => Some(OverviewKey::Column(-1)),
        keysyms::KEY_Right | keysyms::KEY_l => Some(OverviewKey::Column(1)),
        keysyms::KEY_Up | keysyms::KEY_k => Some(OverviewKey::Workspace(-1)),
        keysyms::KEY_Down | keysyms::KEY_j => Some(OverviewKey::Workspace(1)),
        keysyms::KEY_Tab => Some(OverviewKey::Row(1)),
        keysyms::KEY_Return | keysyms::KEY_KP_Enter => Some(OverviewKey::Confirm),
        keysyms::KEY_Escape => Some(OverviewKey::Cancel),
        _ => None,
    }
}

struct MouseGrab {
    start_data: GrabStartData<Paw2>,
    operation: MouseOperation,
    location: Point<f64, Logical>,
}

impl PointerGrab<Paw2> for MouseGrab {
    fn motion(
        &mut self,
        data: &mut Paw2,
        handle: &mut PointerInnerHandle<'_, Paw2>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        self.location = event.location;
        // Smithay holds the pointer lock here. Rendering would read the cursor
        // position and deadlock; each backend redraws after input dispatch.
        data.update_mouse_operation(
            &mut self.operation,
            self.start_data.location,
            event.location,
        );
    }

    fn relative_motion(
        &mut self,
        data: &mut Paw2,
        handle: &mut PointerInnerHandle<'_, Paw2>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, None, event);
    }

    fn button(
        &mut self,
        data: &mut Paw2,
        handle: &mut PointerInnerHandle<'_, Paw2>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if !handle.current_pressed().contains(&BTN_LEFT) {
            // PointerGrab callbacks already hold Smithay's pointer lock. Use
            // the location delivered by motion instead of locking it again.
            data.finish_mouse_operation(&self.operation, self.location);
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut Paw2,
        handle: &mut PointerInnerHandle<'_, Paw2>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut Paw2, handle: &mut PointerInnerHandle<'_, Paw2>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut Paw2,
        handle: &mut PointerInnerHandle<'_, Paw2>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut Paw2,
        handle: &mut PointerInnerHandle<'_, Paw2>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut Paw2,
        handle: &mut PointerInnerHandle<'_, Paw2>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut Paw2,
        handle: &mut PointerInnerHandle<'_, Paw2>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut Paw2,
        handle: &mut PointerInnerHandle<'_, Paw2>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut Paw2,
        handle: &mut PointerInnerHandle<'_, Paw2>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut Paw2,
        handle: &mut PointerInnerHandle<'_, Paw2>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut Paw2,
        handle: &mut PointerInnerHandle<'_, Paw2>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &GrabStartData<Paw2> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut Paw2) {}
}

fn activate_pointer_constraint(
    pointer: &smithay::input::pointer::PointerHandle<Paw2>,
    location: Point<f64, Logical>,
    under: Option<(WlSurface, Point<f64, Logical>)>,
) {
    let Some((surface, surface_location)) = under else {
        return;
    };
    with_pointer_constraint(&surface, pointer, |constraint| {
        if let Some(constraint) = constraint.filter(|value| !value.is_active()) {
            let point = (location - surface_location).to_i32_round();
            if constraint
                .region()
                .is_none_or(|region| region.contains(point))
            {
                constraint.activate();
            }
        }
    });
}

impl Paw2 {
    pub fn process_input<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event, .. } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time_msec(&event);
                let key_state = event.state();
                let action = self.seat.get_keyboard().unwrap().input::<Option<_>, _>(
                    self,
                    event.key_code(),
                    key_state,
                    serial,
                    time,
                    |state, modifiers, handle| {
                        let Some(keysym) = handle.raw_latin_sym_or_raw_current_sym() else {
                            return FilterResult::Forward;
                        };

                        if key_state == KeyState::Pressed {
                            let overview = state.overview_active();
                            let scratchpad_overview = state.scratchpad_overview_active();
                            let action = if scratchpad_overview {
                                scratchpad_overview_key(keysym).map(KeyAction::ScratchpadOverview)
                            } else if overview {
                                overview_key(modifiers, keysym)
                                    .map(KeyAction::Overview)
                                    .or_else(|| {
                                        state
                                            .config
                                            .action(modifiers, keysym)
                                            .map(KeyAction::Compositor)
                                    })
                            } else {
                                state
                                    .config
                                    .action(modifiers, keysym)
                                    .map(KeyAction::Compositor)
                            };
                            if overview || scratchpad_overview || action.is_some() {
                                if !state.suppressed_keys.contains(&keysym) {
                                    state.suppressed_keys.push(keysym);
                                }
                                FilterResult::Intercept(action)
                            } else {
                                FilterResult::Forward
                            }
                        } else if let Some(index) = state
                            .suppressed_keys
                            .iter()
                            .position(|item| *item == keysym)
                        {
                            state.suppressed_keys.remove(index);
                            FilterResult::Intercept(None)
                        } else {
                            FilterResult::Forward
                        }
                    },
                );

                if let Some(action) = action.flatten() {
                    match action {
                        KeyAction::Compositor(action) => self.run_action(action),
                        KeyAction::Overview(OverviewKey::Workspace(delta)) => {
                            self.move_overview_workspace(delta)
                        }
                        KeyAction::Overview(OverviewKey::Column(delta)) => {
                            self.move_overview_column(delta)
                        }
                        KeyAction::Overview(OverviewKey::Row(delta)) => {
                            self.move_overview_row(delta)
                        }
                        KeyAction::Overview(OverviewKey::Monitor(delta)) => {
                            self.move_overview_monitor(delta)
                        }
                        KeyAction::Overview(OverviewKey::Confirm) => self.confirm_overview(),
                        KeyAction::Overview(OverviewKey::Cancel) => self.close_overview(),
                        KeyAction::ScratchpadOverview(ScratchpadOverviewKey::Move {
                            horizontal,
                            delta,
                        }) => self.move_scratchpad_overview(horizontal, delta),
                        KeyAction::ScratchpadOverview(ScratchpadOverviewKey::Confirm) => {
                            self.confirm_scratchpad_overview()
                        }
                        KeyAction::ScratchpadOverview(ScratchpadOverviewKey::Cancel) => {
                            self.close_scratchpad_overview()
                        }
                    }
                }
            }
            InputEvent::PointerMotion { event, .. } => {
                let geometries = self.output_rects().collect::<Vec<_>>();
                if geometries.is_empty() {
                    return;
                }
                let current = self.seat.get_pointer().unwrap().current_location();
                let pointer = self.seat.get_pointer().unwrap();
                if self.overview_active() || self.scratchpad_overview_active() {
                    let location = constrain_pointer(current + event.delta(), &geometries);
                    pointer.relative_motion(
                        self,
                        None,
                        &RelativeMotionEvent {
                            delta: event.delta(),
                            delta_unaccel: event.delta_unaccel(),
                            utime: event.time(),
                        },
                    );
                    pointer.motion(
                        self,
                        None,
                        &MotionEvent {
                            location,
                            serial: SERIAL_COUNTER.next_serial(),
                            time: event.time_msec(),
                        },
                    );
                    pointer.frame(self);
                    return;
                }
                let under = self.surface_under(current);
                let mut locked = false;
                let mut confined = None;
                if let Some((surface, surface_location)) = &under {
                    with_pointer_constraint(surface, &pointer, |constraint| {
                        let Some(constraint) = constraint.filter(|value| value.is_active()) else {
                            return;
                        };
                        if !constraint.region().is_none_or(|region| {
                            region.contains((current - *surface_location).to_i32_round())
                        }) {
                            return;
                        }
                        match &*constraint {
                            PointerConstraint::Locked(_) => locked = true,
                            PointerConstraint::Confined(value) => {
                                confined = value.region().cloned()
                            }
                        }
                    });
                }
                pointer.relative_motion(
                    self,
                    under.clone(),
                    &RelativeMotionEvent {
                        delta: event.delta(),
                        delta_unaccel: event.delta_unaccel(),
                        utime: event.time(),
                    },
                );
                if locked {
                    pointer.frame(self);
                    return;
                }

                let mut delta = constrain_pointer(current + event.delta(), &geometries) - current;
                if let (Some(region), Some((_, surface_location))) = (&confined, &under) {
                    if !region.contains(
                        (current + Point::new(delta.x, 0.0) - *surface_location).to_i32_round(),
                    ) {
                        delta.x = 0.0;
                    }
                    if !region.contains(
                        (current + Point::new(0.0, delta.y) - *surface_location).to_i32_round(),
                    ) {
                        delta.y = 0.0;
                    }
                }
                let location = current + delta;
                let new_under = self.surface_under(location);
                if confined.is_some()
                    && under.as_ref().map(|(surface, _)| surface)
                        != new_under.as_ref().map(|(surface, _)| surface)
                {
                    pointer.frame(self);
                    return;
                }
                self.focus_output_at(location);
                pointer.motion(
                    self,
                    new_under.clone(),
                    &MotionEvent {
                        location,
                        serial: SERIAL_COUNTER.next_serial(),
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
                if self.config.input.focus_follows_mouse
                    && !pointer.is_grabbed()
                    && let Some(window) = self.window_under(location)
                {
                    self.focus_window_on_hover(&window);
                }
                if !pointer.is_grabbed() {
                    activate_pointer_constraint(&pointer, location, new_under);
                }
            }
            InputEvent::PointerMotionAbsolute { event, .. } => {
                let Some(geometry) = self.focused_output_geometry() else {
                    return;
                };
                let location = event.position_transformed(geometry.size) + geometry.loc.to_f64();
                let pointer = self.seat.get_pointer().unwrap();
                if self.overview_active() || self.scratchpad_overview_active() {
                    pointer.motion(
                        self,
                        None,
                        &MotionEvent {
                            location,
                            serial: SERIAL_COUNTER.next_serial(),
                            time: event.time_msec(),
                        },
                    );
                    pointer.frame(self);
                    return;
                }
                self.focus_output_at(location);
                let under = self.surface_under(location);

                pointer.motion(
                    self,
                    under.clone(),
                    &MotionEvent {
                        location,
                        serial: SERIAL_COUNTER.next_serial(),
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
                if self.config.input.focus_follows_mouse
                    && !pointer.is_grabbed()
                    && let Some(window) = self.window_under(location)
                {
                    self.focus_window_on_hover(&window);
                }
                if !pointer.is_grabbed() {
                    activate_pointer_constraint(&pointer, location, under);
                }
            }
            InputEvent::PointerButton { event, .. } => {
                let pointer = self.seat.get_pointer().unwrap();
                let serial = SERIAL_COUNTER.next_serial();
                let location = pointer.current_location();

                if self.scratchpad_overview_active() {
                    if event.state() == ButtonState::Pressed
                        && event.button_code() == BTN_LEFT
                        && let Some(index) = self.scratchpad_overview_hit(location)
                    {
                        self.select_scratchpad_overview(index);
                    }
                    return;
                }

                if self.overview_active() {
                    if event.state() == ButtonState::Pressed
                        && event.button_code() == BTN_LEFT
                        && let Some((output, workspace, window)) = self.overview_hit(location)
                    {
                        self.focus_output_by_name(&output);
                        self.activate_workspace(&output, workspace);
                        if let Some(window) = window {
                            self.focus_window_by_id(window);
                        }
                        self.close_overview();
                    }
                    return;
                }

                if event.state() == ButtonState::Pressed
                    && event.button_code() == BTN_LEFT
                    && !pointer.is_grabbed()
                    && self.seat.get_keyboard().unwrap().modifier_state().logo
                    && let Some(window) = self.window_under(location)
                {
                    self.focus_output_at(location);
                    self.focus_window(&window);
                    if let Some(operation) = self.begin_mouse_operation(window, location) {
                        let focus = self.surface_under(location);
                        pointer.set_grab(
                            self,
                            MouseGrab {
                                start_data: GrabStartData {
                                    focus,
                                    button: BTN_LEFT,
                                    location,
                                },
                                operation,
                                location,
                            },
                            serial,
                            Focus::Clear,
                        );
                        pointer.button(
                            self,
                            &ButtonEvent {
                                button: event.button_code(),
                                state: event.state(),
                                serial,
                                time: event.time_msec(),
                            },
                        );
                        pointer.frame(self);
                        return;
                    }
                }

                if event.state() == ButtonState::Pressed && !pointer.is_grabbed() {
                    self.focus_output_at(location);
                    let window = self.window_under(location);
                    if let Some(window) = window {
                        self.focus_window(&window);
                    } else if let Some(surface) = self.keyboard_layer_under(location) {
                        self.seat
                            .get_keyboard()
                            .unwrap()
                            .set_focus(self, Some(surface), serial);
                    }
                }

                pointer.button(
                    self,
                    &ButtonEvent {
                        button: event.button_code(),
                        state: event.state(),
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }
            InputEvent::PointerAxis { event, .. } => {
                let source = event.source();
                if self.overview_active() || self.scratchpad_overview_active() {
                    let amount = event.amount_v120(Axis::Vertical).unwrap_or_default();
                    if source == AxisSource::Wheel && amount != 0.0 {
                        let delta = if amount < 0.0 { -1 } else { 1 };
                        if self.scratchpad_overview_active() {
                            self.move_scratchpad_overview(true, delta);
                        } else {
                            let location = self.seat.get_pointer().unwrap().current_location();
                            self.scroll_overview_at(location, delta);
                        }
                    }
                    return;
                }
                if source == AxisSource::Wheel
                    && let Some(amount) = event.amount_v120(Axis::Vertical)
                    && amount != 0.0
                {
                    let direction = if amount < 0.0 {
                        ScrollDirection::Up
                    } else {
                        ScrollDirection::Down
                    };
                    let modifiers = self.seat.get_keyboard().unwrap().modifier_state();
                    if let Some(action) = self.config.wheel_action(&modifiers, direction) {
                        if self.pending_wheel_action != Some(action) {
                            self.wheel_v120_remainder = 0.0;
                            self.pending_wheel_action = Some(action);
                        }
                        let detents = wheel_detents(&mut self.wheel_v120_remainder, amount);
                        for _ in 0..detents {
                            self.run_action(action);
                        }
                        return;
                    }
                    self.wheel_v120_remainder = 0.0;
                    self.pending_wheel_action = None;
                }
                let mut frame = AxisFrame::new(event.time_msec()).source(source);

                for axis in [Axis::Horizontal, Axis::Vertical] {
                    let amount = event
                        .amount(axis)
                        .unwrap_or_else(|| event.amount_v120(axis).unwrap_or(0.0) * 15.0 / 120.0);
                    if amount != 0.0 {
                        frame = frame.value(axis, amount);
                        if let Some(discrete) = event.amount_v120(axis) {
                            frame = frame.v120(axis, discrete as i32);
                        }
                    } else if source == AxisSource::Finger && event.amount(axis) == Some(0.0) {
                        frame = frame.stop(axis);
                    }
                }

                let pointer = self.seat.get_pointer().unwrap();
                pointer.axis(self, frame);
                pointer.frame(self);
            }
            _ => {}
        }
    }
}

fn wheel_detents(remainder: &mut f64, amount_v120: f64) -> u32 {
    if *remainder != 0.0 && remainder.signum() != amount_v120.signum() {
        *remainder = 0.0;
    }
    *remainder += amount_v120;
    let detents = (remainder.abs() / 120.0).floor() as u32;
    *remainder -= remainder.signum() * f64::from(detents) * 120.0;
    detents
}

pub(crate) fn constrain_pointer(
    point: Point<f64, Logical>,
    geometries: &[Rectangle<i32, Logical>],
) -> Point<f64, Logical> {
    if geometries
        .iter()
        .any(|geometry| geometry.to_f64().contains(point))
    {
        return point;
    }
    geometries
        .iter()
        .map(|geometry| {
            let min = geometry.loc.to_f64();
            let max = (geometry.loc + geometry.size).to_f64();
            let candidate: Point<f64, Logical> = (
                point.x.clamp(min.x, max.x - 1.0),
                point.y.clamp(min.y, max.y - 1.0),
            )
                .into();
            let dx = candidate.x - point.x;
            let dy = candidate.y - point.y;
            (dx * dx + dy * dy, candidate)
        })
        .min_by(|left, right| left.0.total_cmp(&right.0))
        .map(|(_, point)| point)
        .unwrap_or(point)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_stays_inside_output() {
        let geometries = [
            Rectangle::new((10, 20).into(), (100, 50).into()),
            Rectangle::new((110, 20).into(), (100, 50).into()),
        ];
        assert_eq!(
            constrain_pointer((-5.0, 500.0).into(), &geometries),
            (10.0, 69.0).into()
        );
        assert_eq!(
            constrain_pointer((110.0, 30.0).into(), &geometries),
            (110.0, 30.0).into()
        );
    }

    #[test]
    fn overview_maps_navigation_keys_and_modifiers() {
        let none = ModifiersState::default();
        assert!(matches!(
            overview_key(&none, keysyms::KEY_j.into()),
            Some(OverviewKey::Workspace(1))
        ));
        assert!(matches!(
            overview_key(&none, keysyms::KEY_Left.into()),
            Some(OverviewKey::Column(-1))
        ));
        assert!(matches!(
            overview_key(&none, keysyms::KEY_Tab.into()),
            Some(OverviewKey::Row(1))
        ));
        assert!(matches!(
            overview_key(
                &ModifiersState {
                    shift: true,
                    ..Default::default()
                },
                keysyms::KEY_ISO_Left_Tab.into()
            ),
            Some(OverviewKey::Row(-1))
        ));
        assert!(matches!(
            overview_key(
                &ModifiersState {
                    ctrl: true,
                    ..Default::default()
                },
                keysyms::KEY_l.into()
            ),
            Some(OverviewKey::Monitor(1))
        ));
        assert!(matches!(
            overview_key(&none, keysyms::KEY_Return.into()),
            Some(OverviewKey::Confirm)
        ));
        assert!(matches!(
            overview_key(
                &ModifiersState {
                    logo: true,
                    ..Default::default()
                },
                keysyms::KEY_j.into()
            ),
            Some(OverviewKey::Workspace(1))
        ));
        assert!(
            overview_key(
                &ModifiersState {
                    alt: true,
                    ..Default::default()
                },
                keysyms::KEY_j.into()
            )
            .is_none()
        );
    }

    #[test]
    fn wheel_input_accumulates_complete_detents() {
        let mut remainder = 0.0;
        assert_eq!(wheel_detents(&mut remainder, 40.0), 0);
        assert_eq!(wheel_detents(&mut remainder, 80.0), 1);
        assert_eq!(remainder, 0.0);
        assert_eq!(wheel_detents(&mut remainder, -240.0), 2);
        assert_eq!(remainder, 0.0);

        assert_eq!(wheel_detents(&mut remainder, 60.0), 0);
        assert_eq!(wheel_detents(&mut remainder, -120.0), 1);
        assert_eq!(remainder, 0.0);
    }
}
