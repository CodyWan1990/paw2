use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use smithay::{
    backend::renderer::element::{Id, RenderElementStates, default_primary_scanout_output_compare},
    desktop::{
        Space, Window, WindowSurfaceType, layer_map_for_output,
        utils::{
            OutputPresentationFeedback, surface_presentation_feedback_flags_from_states,
            surface_primary_scanout_output, update_surface_primary_scanout_output,
        },
    },
    output::Output,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{Client, Resource, protocol::wl_surface::WlSurface},
    },
    utils::{Logical, Monotonic, Point, Rectangle, SERIAL_COUNTER, Size, Time},
    wayland::{
        commit_timing::CommitTimerBarrierStateUserData,
        compositor::{CompositorHandler, with_states},
        fifo::FifoBarrierCachedState,
        shell::wlr_layer::{KeyboardInteractivity, Layer},
        shell::xdg::{
            SurfaceCachedState, ToplevelSurface, XdgToplevelSurfaceData, dialog::ToplevelDialogHint,
        },
    },
    xwayland::xwm::WmWindowType,
};

use crate::{config::Action, state::Paw2};

#[derive(Clone, Copy)]
enum Direction {
    Left,
    Right,
    Up,
    Down,
}

pub const WORKSPACE_COUNT: usize = 9;

struct Column<T = Window> {
    windows: Vec<T>,
    focused: usize,
    width: Option<f64>,
}

struct WindowGroup {
    windows: Vec<Window>,
    focused: usize,
    width: Option<f64>,
    fullscreen: Option<Window>,
    floating: bool,
}

impl<T> Column<T> {
    fn new(window: T) -> Self {
        Self {
            windows: vec![window],
            focused: 0,
            width: None,
        }
    }
}

#[derive(Default)]
struct Workspace {
    windows: Vec<Window>,
    focused: Option<usize>,
    columns: Vec<Column>,
    offset: i32,
    fullscreen: Option<Window>,
}

impl Workspace {
    fn add(&mut self, window: Window, floating: bool) {
        if !floating {
            self.columns.push(Column::new(window.clone()));
        }
        self.windows.push(window);
        self.focused = Some(self.windows.len() - 1);
    }

    fn add_column(&mut self, windows: Vec<Window>, focused: usize, width: Option<f64>) {
        self.windows.extend(windows.iter().cloned());
        self.columns.push(Column {
            windows,
            focused,
            width,
        });
        self.focused = self
            .columns
            .last()
            .and_then(|column| column.windows.get(column.focused))
            .and_then(|window| self.window_index(window));
    }

    fn add_group(&mut self, group: WindowGroup) {
        if group.floating {
            self.add(group.windows[0].clone(), true);
        } else {
            self.add_column(group.windows, group.focused, group.width);
        }
        if self.fullscreen.is_none() {
            self.fullscreen = group.fullscreen;
        }
    }

    fn window_index(&self, window: &Window) -> Option<usize> {
        self.windows.iter().position(|item| item == window)
    }

    fn focused_column(&self) -> Option<usize> {
        let window = self.windows.get(self.focused?)?;
        self.columns
            .iter()
            .position(|column| column.windows.contains(window))
    }

    fn focus_window(&mut self, window: &Window) {
        self.focused = self.window_index(window);
        if let Some(column) = self
            .columns
            .iter_mut()
            .find(|column| column.windows.contains(window))
        {
            column.focused = column
                .windows
                .iter()
                .position(|item| item == window)
                .unwrap();
        }
    }

    fn remove_from_column(&mut self, window: &Window) {
        let Some(column_index) = self
            .columns
            .iter()
            .position(|column| column.windows.contains(window))
        else {
            return;
        };
        let column = &mut self.columns[column_index];
        let row = column
            .windows
            .iter()
            .position(|item| item == window)
            .unwrap();
        column.windows.remove(row);
        column.focused = focus_after_removal(Some(column.focused), row, column.windows.len())
            .unwrap_or_default();
        if column.windows.is_empty() {
            self.columns.remove(column_index);
        }
    }

    fn remove_where(&mut self, predicate: impl Fn(&Window) -> bool) -> Option<Window> {
        let index = self.windows.iter().position(predicate)?;
        let window = self.windows.remove(index);
        if self.fullscreen.as_ref() == Some(&window) {
            self.fullscreen = None;
        }
        self.remove_from_column(&window);
        self.focused = focus_after_removal(self.focused, index, self.windows.len());
        Some(window)
    }

    fn take_focused(&mut self) -> Option<(Window, bool)> {
        let index = self.focused?;
        let window = self.windows.remove(index);
        let fullscreen = self.fullscreen.as_ref() == Some(&window);
        if fullscreen {
            self.fullscreen = None;
        }
        self.remove_from_column(&window);
        self.focused = focus_after_removal(Some(index), index, self.windows.len());
        Some((window, fullscreen))
    }

    fn take_focused_group(&mut self) -> Option<WindowGroup> {
        let focused = self.focused?;
        let focused_window = self.windows[focused].clone();
        let Some(column_index) = self.focused_column() else {
            let (window, fullscreen) = self.take_focused()?;
            return Some(WindowGroup {
                windows: vec![window.clone()],
                focused: 0,
                width: None,
                fullscreen: fullscreen.then_some(window),
                floating: true,
            });
        };
        let column = self.columns.remove(column_index);
        for window in &column.windows {
            let index = self.window_index(window).unwrap();
            self.windows.remove(index);
        }
        let fullscreen = self
            .fullscreen
            .take_if(|window| column.windows.contains(window));
        self.focused = self
            .columns
            .get(column_index.min(self.columns.len().saturating_sub(1)))
            .and_then(|column| column.windows.get(column.focused))
            .and_then(|window| self.window_index(window))
            .or_else(|| (!self.windows.is_empty()).then_some(0));
        debug_assert!(column.windows.contains(&focused_window));
        Some(WindowGroup {
            windows: column.windows,
            focused: column.focused,
            width: column.width,
            fullscreen,
            floating: false,
        })
    }

    fn drain_groups(&mut self) -> Vec<WindowGroup> {
        let mut groups = Vec::new();
        while let Some(column) = self.columns.first() {
            let window = column.windows[column.focused].clone();
            self.focused = self.window_index(&window);
            groups.push(self.take_focused_group().unwrap());
        }
        while !self.windows.is_empty() {
            self.focused = Some(0);
            groups.push(self.take_focused_group().unwrap());
        }
        groups
    }

    fn focus_horizontal(&mut self, delta: i32) -> bool {
        let Some(current) = self.focused_column() else {
            return false;
        };
        let Some(next) = neighbor(current, self.columns.len(), delta) else {
            return false;
        };
        let column = &self.columns[next];
        let Some(window) = column.windows.get(column.focused).cloned() else {
            return false;
        };
        self.focused = self.window_index(&window);
        true
    }

    fn focus_vertical(&mut self, delta: i32) -> bool {
        let Some(column_index) = self.focused_column() else {
            return false;
        };
        let column = &mut self.columns[column_index];
        let Some(next) = neighbor(column.focused, column.windows.len(), delta) else {
            return false;
        };
        column.focused = next;
        let window = column.windows[next].clone();
        self.focused = self.window_index(&window);
        true
    }

    fn swap(&mut self, delta: i32) -> bool {
        let Some(current) = self.focused_column() else {
            return false;
        };
        let Some(next) = neighbor(current, self.columns.len(), delta) else {
            return false;
        };
        self.columns.swap(current, next);
        true
    }

    fn swap_vertical(&mut self, delta: i32) -> bool {
        let Some(column) = self
            .focused_column()
            .and_then(|index| self.columns.get_mut(index))
        else {
            return false;
        };
        swap_column_window(column, delta)
    }

    fn set_floating(&mut self, window: &Window, floating: bool) {
        if floating {
            self.remove_from_column(window);
        } else if !self
            .columns
            .iter()
            .any(|column| column.windows.contains(window))
        {
            self.columns.push(Column::new(window.clone()));
        }
    }

    fn consume_or_expel(&mut self, delta: i32) -> bool {
        let Some(column_index) = self.focused_column() else {
            return false;
        };
        consume_or_expel_columns(&mut self.columns, column_index, delta)
    }

    fn cycle_column_width(&mut self, default: f64, presets: &[f64]) -> bool {
        let Some(column) = self
            .focused_column()
            .and_then(|index| self.columns.get_mut(index))
        else {
            return false;
        };
        cycle_column_width(column, default, presets);
        true
    }
}

struct Scroller {
    output: Output,
    space: Space<Window>,
    workspaces: [Workspace; WORKSPACE_COUNT],
    active_workspace: usize,
}

impl Scroller {
    fn new(output: Output, position: Point<i32, Logical>) -> Self {
        let mut space = Space::default();
        space.map_output(&output, position);
        Self {
            output,
            space,
            workspaces: Default::default(),
            active_workspace: 0,
        }
    }

    fn replace_output(&mut self, output: Output, position: Point<i32, Logical>) {
        self.space.unmap_output(&self.output);
        self.space.map_output(&output, position);
        self.output = output;
    }

    fn add(&mut self, window: Window, floating: bool) {
        self.active_mut().add(window, floating);
    }

    fn remove_where(&mut self, predicate: impl Fn(&Window) -> bool) -> Option<Window> {
        for workspace in &mut self.workspaces {
            if let Some(window) = workspace.remove_where(&predicate) {
                self.space.unmap_elem(&window);
                return Some(window);
            }
        }
        None
    }

    fn take_focused_group(&mut self) -> Option<WindowGroup> {
        let group = self.active_mut().take_focused_group()?;
        for window in &group.windows {
            self.space.unmap_elem(window);
        }
        Some(group)
    }

    fn add_group(&mut self, group: WindowGroup) {
        self.active_mut().add_group(group);
    }

    fn active(&self) -> &Workspace {
        &self.workspaces[self.active_workspace]
    }

    fn active_mut(&mut self) -> &mut Workspace {
        &mut self.workspaces[self.active_workspace]
    }

    fn switch_workspace(&mut self, workspace: usize) -> bool {
        if workspace == self.active_workspace || workspace >= WORKSPACE_COUNT {
            return false;
        }
        let active = self.active_workspace;
        for window in &self.workspaces[active].windows {
            self.space.unmap_elem(window);
        }
        self.active_workspace = workspace;
        true
    }

    fn move_focused_to_workspace(&mut self, workspace: usize) -> bool {
        if workspace == self.active_workspace || workspace >= WORKSPACE_COUNT {
            return false;
        }
        let Some(group) = self.take_focused_group() else {
            return false;
        };
        self.workspaces[workspace].add_group(group);
        true
    }
}

#[derive(Default)]
pub struct Scrollers {
    monitors: Vec<Scroller>,
    focused_output: Option<String>,
    parked: [Workspace; WORKSPACE_COUNT],
    floating: HashSet<usize>,
    animations: HashMap<usize, WindowAnimation>,
}

/// A window gliding to a new position, and fading in if it was just mapped.
/// Progress is derived from absolute elapsed time rather than accumulated
/// deltas, so advancing it more than once in a frame is harmless.
struct WindowAnimation {
    start: Duration,
    duration: Duration,
    from: Point<i32, Logical>,
    to: Point<i32, Logical>,
    fade_in: bool,
}

impl WindowAnimation {
    fn progress(&self, now: Duration) -> f64 {
        if self.duration.is_zero() {
            return 1.0;
        }
        (now.saturating_sub(self.start).as_secs_f64() / self.duration.as_secs_f64()).clamp(0.0, 1.0)
    }

    fn finished(&self, now: Duration) -> bool {
        self.progress(now) >= 1.0
    }

    fn position(&self, now: Duration) -> Point<i32, Logical> {
        let eased = ease_out_cubic(self.progress(now));
        let axis = |from: i32, to: i32| (from as f64 + (to - from) as f64 * eased).round() as i32;
        (axis(self.from.x, self.to.x), axis(self.from.y, self.to.y)).into()
    }

    fn opacity(&self, now: Duration) -> f32 {
        if self.fade_in {
            ease_out_cubic(self.progress(now)) as f32
        } else {
            1.0
        }
    }
}

fn ease_out_cubic(t: f64) -> f64 {
    let inverse = 1.0 - t.clamp(0.0, 1.0);
    1.0 - inverse * inverse * inverse
}

/// Records a glide toward `target` and reports where the window belongs right
/// now. A window with no current position has just been mapped, so it fades in
/// instead of sliding from nowhere. Re-running the layout while a glide is in
/// flight must not restart it, so an animation already aimed at `target` is
/// left alone.
fn start_motion(
    animations: &mut HashMap<usize, WindowAnimation>,
    id: usize,
    current: Option<Point<i32, Logical>>,
    target: Point<i32, Logical>,
    now: Duration,
    duration: Option<Duration>,
) -> Point<i32, Logical> {
    let Some(duration) = duration else {
        animations.remove(&id);
        return target;
    };
    if let Some(existing) = animations.get(&id)
        && existing.to == target
        && !existing.finished(now)
    {
        return existing.position(now);
    }
    let Some(current) = current else {
        animations.insert(
            id,
            WindowAnimation {
                start: now,
                duration,
                from: target,
                to: target,
                fade_in: true,
            },
        );
        return target;
    };
    if current == target {
        animations.remove(&id);
        return target;
    }
    animations.insert(
        id,
        WindowAnimation {
            start: now,
            duration,
            from: current,
            to: target,
            fade_in: false,
        },
    );
    current
}

impl Scrollers {
    pub fn focused_output(&self) -> Option<&str> {
        self.focused_output.as_deref()
    }

    fn monitor(&self, output: &str) -> Option<&Scroller> {
        self.monitors
            .iter()
            .find(|monitor| monitor.output.name() == output)
    }

    fn monitor_mut(&mut self, output: &str) -> Option<&mut Scroller> {
        self.monitors
            .iter_mut()
            .find(|monitor| monitor.output.name() == output)
    }

    fn set_focused_output(&mut self, output: &str) -> bool {
        if self
            .monitors
            .iter()
            .any(|monitor| monitor.output.name() == output)
        {
            let changed = self.focused_output.as_deref() != Some(output);
            self.focused_output = Some(output.into());
            changed
        } else {
            false
        }
    }

    fn focused_mut(&mut self) -> Option<&mut Scroller> {
        let output = self.focused_output.clone()?;
        self.monitor_mut(&output)
    }

    /// The focused workspace, or `None` while it holds a fullscreen window:
    /// layout actions must not disturb a fullscreen client.
    fn active_workspace(&mut self) -> Option<&mut Workspace> {
        let monitor = self.focused_mut()?;
        if monitor.active().fullscreen.is_some() {
            return None;
        }
        Some(monitor.active_mut())
    }

    fn set_floating(&mut self, window: &Window, floating: bool) -> bool {
        let changed = if floating {
            self.floating.insert(window.id())
        } else {
            self.floating.remove(&window.id())
        };
        if changed {
            for workspace in self
                .monitors
                .iter_mut()
                .flat_map(|monitor| &mut monitor.workspaces)
                .chain(&mut self.parked)
            {
                if workspace.windows.contains(window) {
                    workspace.set_floating(window, floating);
                    break;
                }
            }
        }
        changed
    }
}

impl Paw2 {
    pub fn output_added(&mut self, output: &Output, position: Point<i32, Logical>) {
        let name = output.name();
        if let Some(monitor) = self.scroller.monitor_mut(&name) {
            monitor.replace_output(output.clone(), position);
            self.relayout(true);
            return;
        }
        let mut monitor = Scroller::new(output.clone(), position);
        for (index, parked) in self.scroller.parked.iter_mut().enumerate() {
            for group in parked.drain_groups() {
                monitor.workspaces[index].add_group(group);
            }
        }
        self.scroller.monitors.push(monitor);
        if self.scroller.focused_output.is_none() {
            self.scroller.focused_output = Some(name);
        }
        let display = self.display_handle.clone();
        self.workspace_state.add_output(&display, output, 0);
        self.relayout(true);
        self.sync_focus();
    }

    pub fn output_removed(&mut self, name: &str, destination: Option<&str>) {
        let Some(index) = self
            .scroller
            .monitors
            .iter()
            .position(|monitor| monitor.output.name() == name)
        else {
            return;
        };
        let removed_was_focused = self.scroller.focused_output.as_deref() == Some(name);
        self.workspace_state.remove_output(name);
        let mut removed = self.scroller.monitors.remove(index);
        let active = removed.active_workspace;
        for window in &removed.workspaces[active].windows {
            removed.space.unmap_elem(window);
        }
        let mut restore_tiled = Vec::new();
        let mut keep_fullscreen = Vec::new();
        if let Some(destination) = destination
            && let Some(target) = self.scroller.monitor_mut(destination)
        {
            let target_output = target.output.clone();
            for (index, workspace) in removed.workspaces.iter_mut().enumerate() {
                for mut group in workspace.drain_groups() {
                    if let Some(window) = group.fullscreen.clone() {
                        if target.workspaces[index].fullscreen.is_none() {
                            keep_fullscreen.push((window, target_output.clone()));
                        } else {
                            group.fullscreen = None;
                            restore_tiled.push(window);
                        }
                    }
                    target.workspaces[index].add_group(group);
                }
            }
            if removed_was_focused {
                self.scroller.focused_output = Some(destination.into());
            }
        } else {
            for (index, workspace) in removed.workspaces.iter_mut().enumerate() {
                for mut group in workspace.drain_groups() {
                    if let Some(window) = group.fullscreen.take() {
                        restore_tiled.push(window);
                    }
                    self.scroller.parked[index].add_group(group);
                }
            }
            if removed_was_focused {
                self.scroller.focused_output = self
                    .scroller
                    .monitors
                    .first()
                    .map(|item| item.output.name());
            }
        }
        for window in restore_tiled {
            self.set_window_fullscreen(&window, None);
        }
        for (window, output) in keep_fullscreen {
            self.set_window_fullscreen(&window, Some(&output));
        }
        self.relayout(true);
        self.sync_focus();
    }

    pub fn output_suspended(&mut self, name: &str) {
        if let Some(monitor) = self.scroller.monitor_mut(name) {
            monitor.space.unmap_output(&monitor.output);
        }
    }

    pub fn focus_output_at(&mut self, position: Point<f64, Logical>) {
        let name = self
            .output_geometries()
            .into_iter()
            .find_map(|(name, geometry)| geometry.to_f64().contains(position).then_some(name));
        if let Some(name) = name
            && self.scroller.set_focused_output(&name)
        {
            self.sync_focus();
        }
    }

    pub fn add_window(&mut self, surface: ToplevelSurface) {
        self.add_managed_window(Window::new_wayland_window(surface), false, false);
    }

    pub fn add_x11_window(&mut self, surface: smithay::xwayland::X11Surface) {
        let fullscreen = surface.is_fullscreen();
        let floating = x11_should_float(&surface);
        self.add_managed_window(Window::new_x11_window(surface), fullscreen, floating);
    }

    pub fn add_x11_override(&mut self, surface: smithay::xwayland::X11Surface) {
        let location = surface.last_configure().loc;
        let window = Window::new_x11_window(surface);
        let output = self
            .monitor_at(location.to_f64())
            .map(|monitor| monitor.output.name())
            .or_else(|| self.scroller.focused_output.clone());
        if let Some(monitor) = output.and_then(|name| self.scroller.monitor_mut(&name)) {
            monitor.space.map_element(window, location, true);
        }
    }

    fn add_managed_window(&mut self, window: Window, fullscreen: bool, floating: bool) {
        let pointer = self.seat.get_pointer().unwrap().current_location();
        self.focus_output_at(pointer);
        if floating {
            self.scroller.floating.insert(window.id());
        }
        if let Some(output) = self.scroller.focused_output.clone()
            && let Some(monitor) = self.scroller.monitor_mut(&output)
        {
            monitor.add(window.clone(), floating);
            if fullscreen {
                monitor.active_mut().fullscreen = Some(window.clone());
            }
        } else {
            self.scroller.parked[0].add(window.clone(), floating);
        }
        if fullscreen && let Some(output) = self.default_output() {
            self.set_window_fullscreen(&window, Some(&output));
        }
        self.relayout(true);
        self.sync_focus();
    }

    pub fn refresh_wayland_floating(&mut self, surface: &ToplevelSurface) {
        let Some(window) = self.window_for_surface(surface.wl_surface()).cloned() else {
            return;
        };
        let floating = wayland_should_float(surface);
        let changed = self.scroller.set_floating(&window, floating);

        if changed {
            self.relayout(true);
            self.sync_focus();
        } else if floating {
            self.center_floating_window(&window);
        }
    }

    pub fn refresh_x11_floating(&mut self, surface: &smithay::xwayland::X11Surface) {
        let Some(window) = self.window_for_x11_surface(surface).cloned() else {
            return;
        };
        let floating = x11_should_float(surface);
        let changed = self.scroller.set_floating(&window, floating);
        if changed {
            self.relayout(true);
            self.sync_focus();
        }
    }

    fn center_floating_window(&mut self, window: &Window) {
        for monitor in &mut self.scroller.monitors {
            if !monitor.active().windows.contains(window) {
                continue;
            }
            let Some(output) = monitor.space.output_geometry(&monitor.output) else {
                return;
            };
            let area = work_area(
                output,
                layer_map_for_output(&monitor.output).non_exclusive_zone(),
            );
            let rectangle = centered_rectangle(area, window.geometry().size);
            monitor.space.relocate_element(window, rectangle.loc);
            return;
        }
    }

    pub fn remove_window(&mut self, surface: &WlSurface) {
        self.remove_managed_window(|window| window_surface(window).as_ref() == Some(surface));
    }

    pub fn remove_x11_window(&mut self, surface: &smithay::xwayland::X11Surface) {
        let predicate = |window: &Window| window.x11_surface() == Some(surface);
        if self.remove_managed_window(predicate) {
            return;
        }
        // Override-redirect surfaces live directly in a space, never in a workspace.
        let unmapped = self.scroller.monitors.iter_mut().find_map(|monitor| {
            let window = monitor
                .space
                .elements()
                .find(|window| predicate(window))?
                .clone();
            monitor.space.unmap_elem(&window);
            Some(window)
        });
        if let Some(window) = unmapped {
            self.forget_window(&window);
        }
    }

    /// Removes the window from whichever workspace holds it, mapped or parked.
    fn remove_managed_window(&mut self, predicate: impl Fn(&Window) -> bool) -> bool {
        let window = self
            .scroller
            .monitors
            .iter_mut()
            .find_map(|monitor| monitor.remove_where(&predicate))
            .or_else(|| {
                self.scroller
                    .parked
                    .iter_mut()
                    .find_map(|workspace| workspace.remove_where(&predicate))
            });
        let Some(window) = window else {
            return false;
        };
        self.forget_window(&window);
        true
    }

    fn forget_window(&mut self, window: &Window) {
        self.scroller.floating.remove(&window.id());
        self.scroller.animations.remove(&window.id());
        self.relayout(true);
        self.sync_focus();
    }

    pub fn configure_x11_override(
        &mut self,
        surface: &smithay::xwayland::X11Surface,
        location: Point<i32, Logical>,
    ) {
        for monitor in &mut self.scroller.monitors {
            let window = monitor
                .space
                .elements()
                .find(|window| window.x11_surface() == Some(surface))
                .cloned();
            if let Some(window) = window {
                monitor.space.map_element(window, location, true);
                return;
            }
        }
    }

    pub fn configure_x11_window(
        &mut self,
        surface: &smithay::xwayland::X11Surface,
        width: Option<u32>,
        height: Option<u32>,
    ) {
        let mut rectangle = surface.last_configure();
        rectangle.size.w = width.map_or(rectangle.size.w, |value| value as i32);
        rectangle.size.h = height.map_or(rectangle.size.h, |value| value as i32);
        let Some(window) = self.window_for_x11_surface(surface).cloned() else {
            if let Err(error) = surface.configure(rectangle) {
                tracing::warn!(?error, "failed to configure unmapped X11 window");
            }
            return;
        };
        if !self.scroller.floating.contains(&window.id()) {
            self.relayout(false);
            return;
        }

        let fullscreen = self
            .scroller
            .monitors
            .iter()
            .any(|monitor| monitor.active().fullscreen.as_ref() == Some(&window));
        if fullscreen {
            self.relayout(false);
            return;
        }
        for monitor in &mut self.scroller.monitors {
            if !monitor.active().windows.contains(&window) {
                continue;
            }
            let Some(output) = monitor.space.output_geometry(&monitor.output) else {
                return;
            };
            let area = work_area(
                output,
                layer_map_for_output(&monitor.output).non_exclusive_zone(),
            );
            rectangle.loc = centered_rectangle(area, rectangle.size).loc;
            let result = if surface.sync_request_supported() {
                surface.configure_with_sync(rectangle, None)
            } else {
                surface.configure(rectangle)
            };
            if let Err(error) = result {
                tracing::warn!(?error, "failed to configure floating X11 window");
            }
            monitor.space.relocate_element(&window, rectangle.loc);
            return;
        }
    }

    pub fn focus_window(&mut self, window: &Window) {
        let Some(monitor_index) = self
            .scroller
            .monitors
            .iter()
            .position(|monitor| monitor.active().windows.contains(window))
        else {
            return;
        };
        self.scroller.monitors[monitor_index]
            .active_mut()
            .focus_window(window);
        self.scroller.focused_output = Some(self.scroller.monitors[monitor_index].output.name());
        self.relayout(true);
        if self.scroller.floating.contains(&window.id()) {
            self.scroller.monitors[monitor_index]
                .space
                .raise_element(window, false);
        }
        self.sync_focus();
    }

    pub fn set_fullscreen(&mut self, surface: &ToplevelSurface, requested: Option<Output>) {
        let Some(window) = self.window_for_surface(surface.wl_surface()).cloned() else {
            surface.send_configure();
            return;
        };
        self.fullscreen_managed_window(window, requested);
    }

    pub fn set_x11_fullscreen(&mut self, surface: &smithay::xwayland::X11Surface) {
        if let Some(window) = self.window_for_x11_surface(surface).cloned() {
            self.fullscreen_managed_window(window, None);
        }
    }

    fn fullscreen_managed_window(&mut self, window: Window, requested: Option<Output>) {
        let Some((source_monitor, source_workspace, source_window)) =
            self.scroller
                .monitors
                .iter()
                .enumerate()
                .find_map(|(monitor_index, monitor)| {
                    monitor.workspaces.iter().enumerate().find_map(
                        |(workspace_index, workspace)| {
                            workspace
                                .windows
                                .iter()
                                .position(|candidate| candidate == &window)
                                .map(|window_index| (monitor_index, workspace_index, window_index))
                        },
                    )
                })
        else {
            return;
        };

        let target_monitor = requested
            .as_ref()
            .and_then(|output| {
                self.scroller
                    .monitors
                    .iter()
                    .position(|monitor| monitor.output == *output)
            })
            .unwrap_or(source_monitor);
        let target_workspace = self.scroller.monitors[target_monitor].active_workspace;

        if (source_monitor, source_workspace) != (target_monitor, target_workspace) {
            let source = &mut self.scroller.monitors[source_monitor];
            let workspace = &mut source.workspaces[source_workspace];
            let window = workspace.windows.remove(source_window);
            workspace.remove_from_column(&window);
            workspace.fullscreen = None;
            workspace.focused =
                focus_after_removal(workspace.focused, source_window, workspace.windows.len());
            source.space.unmap_elem(&window);

            let target = &mut self.scroller.monitors[target_monitor];
            let floating = self.scroller.floating.contains(&window.id());
            target.workspaces[target_workspace].add(window.clone(), floating);
        }

        let old_fullscreen = {
            let target = &mut self.scroller.monitors[target_monitor];
            let workspace = &mut target.workspaces[target_workspace];
            workspace.focus_window(&window);
            workspace.fullscreen.replace(window.clone())
        };
        if let Some(previous) = old_fullscreen.filter(|previous| previous != &window) {
            self.set_window_fullscreen(&previous, None);
        }

        let output = self.scroller.monitors[target_monitor].output.clone();
        self.set_window_fullscreen(&window, Some(&output));
        self.scroller.focused_output = Some(output.name());
        self.relayout(true);
        self.sync_focus();
        tracing::info!(output = %output.name(), "window entered fullscreen");
    }

    fn set_window_fullscreen(&self, window: &Window, output: Option<&Output>) {
        if let Some(surface) = window.toplevel() {
            let output_resource = output.and_then(|output| {
                self.display_handle
                    .get_client(surface.wl_surface().id())
                    .ok()
                    .and_then(|client| output.client_outputs(&client).next())
            });
            let size =
                output.and_then(|output| self.output_geometry(output).map(|value| value.size));
            set_fullscreen_state(surface, size, output_resource);
        } else if let Some(surface) = window.x11_surface() {
            let _ = surface.set_fullscreen(output.is_some());
        }
    }

    pub fn unset_fullscreen(&mut self, surface: &ToplevelSurface) {
        let Some(window) = self.window_for_surface(surface.wl_surface()).cloned() else {
            set_fullscreen_state(surface, None, None);
            return;
        };
        self.unfullscreen_window(&window);
    }

    pub fn unset_x11_fullscreen(&mut self, surface: &smithay::xwayland::X11Surface) {
        if let Some(window) = self.window_for_x11_surface(surface).cloned() {
            self.unfullscreen_window(&window);
        }
    }

    fn unfullscreen_window(&mut self, window: &Window) {
        for monitor in &mut self.scroller.monitors {
            for workspace in &mut monitor.workspaces {
                if workspace.fullscreen.as_ref() == Some(window) {
                    workspace.fullscreen = None;
                }
            }
        }
        self.set_window_fullscreen(window, None);
        self.relayout(true);
        self.sync_focus();
        tracing::info!("window left fullscreen");
    }

    fn toggle_fullscreen(&mut self) -> bool {
        let Some(window) = self.scroller.focused_mut().and_then(|scroller| {
            let workspace = scroller.active();
            workspace
                .focused
                .map(|index| workspace.windows[index].clone())
        }) else {
            return false;
        };
        let is_fullscreen = self
            .scroller
            .monitors
            .iter()
            .flat_map(|monitor| &monitor.workspaces)
            .any(|workspace| workspace.fullscreen.as_ref() == Some(&window));
        if is_fullscreen {
            self.unfullscreen_window(&window);
        } else {
            self.fullscreen_managed_window(window, None);
        }
        false
    }

    pub fn run_action(&mut self, action: Action) {
        let changed = match action {
            Action::FocusLeft => self.layout_action(|workspace| workspace.focus_horizontal(-1)),
            Action::FocusRight => self.layout_action(|workspace| workspace.focus_horizontal(1)),
            Action::FocusUp => self.layout_action(|workspace| workspace.focus_vertical(-1)),
            Action::FocusDown => self.layout_action(|workspace| workspace.focus_vertical(1)),
            Action::SwapLeft => self.layout_action(|workspace| workspace.swap(-1)),
            Action::SwapRight => self.layout_action(|workspace| workspace.swap(1)),
            Action::SwapUp => self.layout_action(|workspace| workspace.swap_vertical(-1)),
            Action::SwapDown => self.layout_action(|workspace| workspace.swap_vertical(1)),
            Action::ConsumeOrExpelLeft => {
                self.layout_action(|workspace| workspace.consume_or_expel(-1))
            }
            Action::ConsumeOrExpelRight => {
                self.layout_action(|workspace| workspace.consume_or_expel(1))
            }
            Action::CycleColumnWidth => {
                let default = self.config.layout.column_width;
                let presets = &self.config.layout.preset_column_widths;
                self.scroller
                    .active_workspace()
                    .is_some_and(|workspace| workspace.cycle_column_width(default, presets))
            }
            Action::CloseWindow => {
                if let Some(window) = self.scroller.focused_mut().and_then(|scroller| {
                    let workspace = scroller.active();
                    workspace.focused.map(|index| &workspace.windows[index])
                }) {
                    if let Some(surface) = window.toplevel() {
                        surface.send_close();
                    } else if let Some(surface) = window.x11_surface() {
                        let _ = surface.close();
                    }
                }
                false
            }
            Action::ToggleFullscreen => self.toggle_fullscreen(),
            Action::FocusMonitorLeft => self.change_monitor(Direction::Left, false),
            Action::FocusMonitorRight => self.change_monitor(Direction::Right, false),
            Action::FocusMonitorUp => self.change_monitor(Direction::Up, false),
            Action::FocusMonitorDown => self.change_monitor(Direction::Down, false),
            Action::MoveToMonitorLeft => self.change_monitor(Direction::Left, true),
            Action::MoveToMonitorRight => self.change_monitor(Direction::Right, true),
            Action::MoveToMonitorUp => self.change_monitor(Direction::Up, true),
            Action::MoveToMonitorDown => self.change_monitor(Direction::Down, true),
            Action::SwitchWorkspace(workspace) => self
                .scroller
                .focused_mut()
                .is_some_and(|scroller| scroller.switch_workspace(workspace)),
            Action::MoveToWorkspace(workspace) => self.move_focused_to_workspace(workspace),
            Action::PreviousWorkspace => self.change_workspace(-1, false),
            Action::NextWorkspace => self.change_workspace(1, false),
            Action::MoveToPreviousWorkspace => self.change_workspace(-1, true),
            Action::MoveToNextWorkspace => self.change_workspace(1, true),
            Action::LaunchCommand(index) => {
                let command = &self.config.binding_commands[index];
                if let Err(error) = crate::spawn(command) {
                    tracing::error!(?error, program = command[0], "failed to launch command");
                }
                false
            }
            Action::Quit => {
                self.loop_signal.stop();
                false
            }
        };
        if changed {
            self.relayout(true);
            self.sync_focus();
            self.sync_workspace_protocol();
            crate::drm::redraw(self);
        }
    }

    fn layout_action(&mut self, action: impl FnOnce(&mut Workspace) -> bool) -> bool {
        self.scroller.active_workspace().is_some_and(action)
    }

    fn change_monitor(&mut self, direction: Direction, move_window: bool) -> bool {
        self.try_change_monitor(direction, move_window).is_some()
    }

    fn change_workspace(&mut self, delta: i32, move_window: bool) -> bool {
        let Some(scroller) = self.scroller.focused_mut() else {
            return false;
        };
        let target = workspace_neighbor(scroller.active_workspace, delta);
        if move_window {
            let _ = scroller;
            self.move_focused_to_workspace(target)
        } else {
            scroller.switch_workspace(target)
        }
    }

    fn move_focused_to_workspace(&mut self, workspace: usize) -> bool {
        let Some(scroller) = self.scroller.focused_mut() else {
            return false;
        };
        let active = scroller.active_workspace;
        if active == workspace || workspace >= WORKSPACE_COUNT {
            return false;
        }
        let moved = scroller.active().focused.and_then(|index| {
            let window = &scroller.active().windows[index];
            (scroller.active().fullscreen.as_ref() == Some(window)).then(|| window.clone())
        });
        let conflict = moved.is_some() && scroller.workspaces[workspace].fullscreen.is_some();
        let changed = scroller.move_focused_to_workspace(workspace);
        if conflict && let Some(window) = moved {
            self.set_window_fullscreen(&window, None);
        }
        changed
    }

    pub(crate) fn workspace_outputs(&self) -> Vec<(Output, usize)> {
        self.scroller
            .monitors
            .iter()
            .map(|monitor| (monitor.output.clone(), monitor.active_workspace))
            .collect()
    }

    pub(crate) fn activate_workspace(&mut self, output: &str, workspace: usize) {
        let changed = self
            .scroller
            .monitor_mut(output)
            .is_some_and(|monitor| monitor.switch_workspace(workspace));
        if changed {
            self.scroller.focused_output = Some(output.into());
            self.relayout(true);
            self.sync_focus();
            self.sync_workspace_protocol();
            crate::drm::redraw(self);
        }
    }

    fn sync_workspace_protocol(&mut self) {
        let active = self
            .scroller
            .monitors
            .iter()
            .map(|monitor| (monitor.output.name(), monitor.active_workspace))
            .collect::<Vec<_>>();
        for (output, workspace) in active {
            self.workspace_state.set_active(&output, workspace);
        }
    }

    fn try_change_monitor(&mut self, direction: Direction, move_window: bool) -> Option<()> {
        let current = self.scroller.focused_output.clone()?;
        let geometries = self.output_geometries();
        let target = directional_neighbor(&current, direction, &geometries)?;
        let mut moved_fullscreen = None;

        if move_window {
            let target_has_fullscreen = self
                .scroller
                .monitor(&target)?
                .active()
                .fullscreen
                .is_some();
            let mut group = self.scroller.monitor_mut(&current)?.take_focused_group()?;
            let fullscreen = group.fullscreen.clone();
            if target_has_fullscreen {
                group.fullscreen = None;
            }
            let target_scroller = self.scroller.monitor_mut(&target)?;
            target_scroller.add_group(group);
            if let Some(window) = fullscreen {
                if target_has_fullscreen {
                    self.set_window_fullscreen(&window, None);
                } else {
                    moved_fullscreen = Some(window);
                }
            }
        }
        if let Some(window) = moved_fullscreen {
            let output = self.scroller.monitor(&target)?.output.clone();
            self.set_window_fullscreen(&window, Some(&output));
        }
        self.scroller.focused_output = Some(target.clone());
        self.relayout(true);
        self.sync_focus();

        if let Some((_, geometry)) = geometries.iter().find(|(name, _)| name == &target) {
            let location = geometry.loc.to_f64() + geometry.size.to_f64() / 2.0;
            let pointer = self.seat.get_pointer().unwrap();
            pointer.motion(
                self,
                self.surface_under(location),
                &smithay::input::pointer::MotionEvent {
                    location,
                    serial: SERIAL_COUNTER.next_serial(),
                    time: self.start_time.elapsed().as_millis() as u32,
                },
            );
            pointer.frame(self);
        }
        Some(())
    }

    pub fn nearest_output(&self, removed: &str) -> Option<String> {
        let geometries = self.output_geometries();
        let (_, source) = geometries.iter().find(|(name, _)| name == removed)?;
        let source = center(*source);
        geometries
            .iter()
            .filter(|(name, _)| name != removed)
            .min_by_key(|(_, geometry)| {
                let target = center(*geometry);
                let dx = target.x - source.x;
                let dy = target.y - source.y;
                dx * dx + dy * dy
            })
            .map(|(name, _)| name.clone())
    }

    pub fn output_geometries(&self) -> Vec<(String, Rectangle<i32, Logical>)> {
        self.scroller
            .monitors
            .iter()
            .filter_map(|monitor| {
                monitor
                    .space
                    .output_geometry(&monitor.output)
                    .map(|geometry| (monitor.output.name(), geometry))
            })
            .collect()
    }

    pub fn output_geometry(&self, output: &Output) -> Option<Rectangle<i32, Logical>> {
        let monitor = self.scroller.monitor(&output.name())?;
        monitor.space.output_geometry(&monitor.output)
    }

    pub fn outputs(&self) -> Vec<Output> {
        self.scroller
            .monitors
            .iter()
            .map(|monitor| monitor.output.clone())
            .collect()
    }

    pub fn default_output(&self) -> Option<Output> {
        self.scroller
            .focused_output
            .as_deref()
            .and_then(|name| self.scroller.monitor(name))
            .or_else(|| self.scroller.monitors.first())
            .map(|monitor| monitor.output.clone())
    }

    pub fn space_for_output(&self, output: &Output) -> Option<&Space<Window>> {
        self.scroller
            .monitor(&output.name())
            .map(|monitor| &monitor.space)
    }

    pub fn focused_window_geometry(&self, output: &Output) -> Option<Rectangle<i32, Logical>> {
        if self.scroller.focused_output.as_deref() != Some(output.name().as_str()) {
            return None;
        }
        let monitor = self.scroller.monitor(&output.name())?;
        let workspace = monitor.active();
        let window = &workspace.windows[workspace.focused?];
        monitor.space.element_geometry(window)
    }

    pub fn fullscreen_window(&self, output: &Output) -> Option<&Window> {
        self.scroller
            .monitor(&output.name())?
            .active()
            .fullscreen
            .as_ref()
    }

    pub fn x11_override_windows(&self, output: &Output) -> Vec<(Window, Point<i32, Logical>)> {
        let Some(monitor) = self.scroller.monitor(&output.name()) else {
            return Vec::new();
        };
        let Some(output_location) = monitor.space.output_geometry(output).map(|value| value.loc)
        else {
            return Vec::new();
        };
        monitor
            .space
            .elements()
            .filter(|window| {
                window
                    .x11_surface()
                    .is_some_and(|surface| surface.is_override_redirect())
            })
            .filter_map(|window| {
                monitor
                    .space
                    .element_location(window)
                    .map(|location| (window.clone(), location - output_location))
            })
            .collect()
    }

    /// Output-relative geometry and effective opacity for a rendered element,
    /// or `None` when the element is not a window's root surface. Matches only
    /// root surfaces, so subsurfaces fall through untouched.
    pub fn window_render_state(
        &self,
        output: &Output,
        element: &Id,
    ) -> Option<(Rectangle<i32, Logical>, f32)> {
        let monitor = self.scroller.monitor(&output.name())?;
        let window = monitor.active().windows.iter().find(|window| {
            window_surface(window)
                .is_some_and(|surface| Id::from_wayland_resource(&surface) == *element)
        })?;
        let mut geometry = monitor.space.element_geometry(window)?;
        geometry.loc -= monitor.space.output_geometry(output)?.loc;
        let fade = self
            .scroller
            .animations
            .get(&window.id())
            .map_or(1.0, |animation| {
                animation.opacity(self.start_time.elapsed())
            });
        Some((geometry, self.config.appearance.window_opacity() * fade))
    }

    pub fn pre_repaint(&mut self, output: &Output, frame_target: Time<Monotonic>) {
        let clients = {
            let mut clients = Vec::<Client>::new();
            let mut signal =
                |surface: &WlSurface, states: &smithay::wayland::compositor::SurfaceData| {
                    let Some(commit_timer) =
                        states.data_map.get::<CommitTimerBarrierStateUserData>()
                    else {
                        return;
                    };
                    commit_timer.lock().unwrap().signal_until(frame_target);
                    if let Some(client) = surface.client()
                        && !clients.iter().any(|known| known.id() == client.id())
                    {
                        clients.push(client);
                    }
                };
            if let Some(space) = self.space_for_output(output) {
                for window in space.elements() {
                    window.with_surfaces(&mut signal);
                }
            }
            let layers = layer_map_for_output(output);
            for layer in layers.layers() {
                layer.with_surfaces(&mut signal);
            }
            clients
        };

        let display_handle = self.display_handle.clone();
        for client in clients {
            self.client_compositor_state(&client)
                .blocker_cleared(self, &display_handle);
        }
    }

    pub fn send_frames(&mut self, output: &Output) {
        let time = self.start_time.elapsed();
        let clients = {
            let mut clients = Vec::<Client>::new();
            let mut signal =
                |surface: &WlSurface, states: &smithay::wayland::compositor::SurfaceData| {
                    if surface_primary_scanout_output(surface, states)
                        .as_ref()
                        .is_none_or(|primary| primary == output)
                        && let Some(barrier) = states
                            .cached_state
                            .get::<FifoBarrierCachedState>()
                            .current()
                            .barrier
                            .take()
                    {
                        barrier.signal();
                        if let Some(client) = surface.client()
                            && !clients.iter().any(|known| known.id() == client.id())
                        {
                            clients.push(client);
                        }
                    }
                };
            if let Some(space) = self.space_for_output(output) {
                for window in space.elements() {
                    window.with_surfaces(&mut signal);
                    window.send_frame(output, time, Some(Duration::ZERO), |_, _| {
                        Some(output.clone())
                    });
                }
            }
            let layers = layer_map_for_output(output);
            for layer in layers.layers() {
                layer.with_surfaces(&mut signal);
                layer.send_frame(output, time, Some(Duration::ZERO), |_, _| {
                    Some(output.clone())
                });
            }
            clients
        };

        let display_handle = self.display_handle.clone();
        for client in clients {
            self.client_compositor_state(&client)
                .blocker_cleared(self, &display_handle);
        }
    }

    pub fn take_presentation_feedback(
        &self,
        output: &Output,
        states: &RenderElementStates,
    ) -> OutputPresentationFeedback {
        let mut feedback = OutputPresentationFeedback::new(output);
        if let Some(space) = self.space_for_output(output) {
            for window in space.elements() {
                window.with_surfaces(|surface, data| {
                    update_surface_primary_scanout_output(
                        surface,
                        output,
                        data,
                        None,
                        states,
                        default_primary_scanout_output_compare,
                    );
                });
                window.take_presentation_feedback(
                    &mut feedback,
                    surface_primary_scanout_output,
                    |surface, _| {
                        surface_presentation_feedback_flags_from_states(surface, None, states)
                    },
                );
            }
        }
        for layer in layer_map_for_output(output).layers() {
            layer.with_surfaces(|surface, data| {
                update_surface_primary_scanout_output(
                    surface,
                    output,
                    data,
                    None,
                    states,
                    default_primary_scanout_output_compare,
                );
            });
            layer.take_presentation_feedback(
                &mut feedback,
                surface_primary_scanout_output,
                |surface, _| surface_presentation_feedback_flags_from_states(surface, None, states),
            );
        }
        feedback
    }

    pub fn finish_frame(&mut self) {
        for monitor in &mut self.scroller.monitors {
            monitor.space.refresh();
        }
        self.popups.cleanup();
        self.idle_inhibitors.retain(Resource::is_alive);
    }

    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<&Window> {
        self.find_window(|window| window_surface(window).as_ref() == Some(surface))
    }

    pub fn window_for_x11_surface(
        &self,
        surface: &smithay::xwayland::X11Surface,
    ) -> Option<&Window> {
        self.find_window(|window| window.x11_surface() == Some(surface))
    }

    /// Mapped windows first, then every workspace: a window on an inactive
    /// workspace belongs to no space.
    fn find_window(&self, predicate: impl Fn(&Window) -> bool) -> Option<&Window> {
        self.scroller
            .monitors
            .iter()
            .flat_map(|monitor| monitor.space.elements())
            .find(|window| predicate(window))
            .or_else(|| {
                self.scroller
                    .monitors
                    .iter()
                    .flat_map(|monitor| &monitor.workspaces)
                    .flat_map(|workspace| &workspace.windows)
                    .find(|window| predicate(window))
            })
    }

    pub fn window_location(&self, surface: &WlSurface) -> Option<Point<i32, Logical>> {
        self.scroller.monitors.iter().find_map(|monitor| {
            let window = monitor
                .workspaces
                .iter()
                .flat_map(|workspace| &workspace.windows)
                .find(|window| window_surface(window).as_ref() == Some(surface))?;
            monitor.space.element_location(window)
        })
    }

    pub fn window_under(&self, position: Point<f64, Logical>) -> Option<Window> {
        let monitor = self.monitor_at(position)?;
        if let Some(window) = monitor.active().fullscreen.as_ref() {
            let geometry = monitor.space.output_geometry(&monitor.output)?;
            let local = position - geometry.loc.to_f64();
            let layers = layer_map_for_output(&monitor.output);
            if layers
                .layer_under(smithay::wayland::shell::wlr_layer::Layer::Overlay, local)
                .and_then(|layer| {
                    let location = layers.layer_geometry(layer)?.loc;
                    layer.surface_under(local - location.to_f64(), WindowSurfaceType::ALL)
                })
                .is_some()
            {
                return None;
            }
            return aspect_fit(window.geometry(), geometry.size)
                .filter(|placement| placement.target.contains(local))
                .map(|_| window.clone());
        }
        monitor
            .space
            .element_under(position)
            .map(|(window, _)| window.clone())
    }

    pub fn keyboard_layer_under(&self, position: Point<f64, Logical>) -> Option<WlSurface> {
        let monitor = self.monitor_at(position)?;
        let geometry = monitor.space.output_geometry(&monitor.output)?;
        let local = position - geometry.loc.to_f64();
        let layers = layer_map_for_output(&monitor.output);
        let mut layer =
            layers.layer_under(smithay::wayland::shell::wlr_layer::Layer::Overlay, local);
        if layer.is_none() && monitor.active().fullscreen.is_none() {
            layer = layers.layer_under(smithay::wayland::shell::wlr_layer::Layer::Top, local);
        }
        let layer = layer.filter(|layer| layer.can_receive_keyboard_focus())?;
        let location = layers.layer_geometry(layer)?.loc;
        layer
            .surface_under(local - location.to_f64(), WindowSurfaceType::ALL)
            .map(|_| layer.wl_surface().clone())
    }

    pub fn surface_under(
        &self,
        position: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let monitor = self.monitor_at(position)?;
        let output_geometry = monitor.space.output_geometry(&monitor.output)?;
        let local = position - output_geometry.loc.to_f64();
        let layers = layer_map_for_output(&monitor.output);
        let layer_surface = |layer: &smithay::desktop::LayerSurface| {
            let layer_location = layers.layer_geometry(layer)?.loc;
            layer
                .surface_under(local - layer_location.to_f64(), WindowSurfaceType::ALL)
                .map(|(surface, offset)| {
                    (
                        surface,
                        (offset + layer_location + output_geometry.loc).to_f64(),
                    )
                })
        };
        use smithay::wayland::shell::wlr_layer::Layer;
        let overlay = layers
            .layer_under(Layer::Overlay, local)
            .and_then(layer_surface);
        if let Some(window) = monitor.active().fullscreen.as_ref() {
            return overlay.or_else(|| {
                let placement = aspect_fit(window.geometry(), output_geometry.size)?;
                if !placement.target.contains(local) {
                    return None;
                }
                let source_point = placement.source_point(local);
                window
                    .surface_under(source_point, WindowSurfaceType::ALL)
                    .map(|(surface, offset)| {
                        let surface_point = source_point - offset.to_f64();
                        (surface, position - surface_point)
                    })
            });
        }
        overlay
            .or_else(|| {
                layers
                    .layer_under(Layer::Top, local)
                    .and_then(layer_surface)
            })
            .or_else(|| {
                monitor
                    .space
                    .element_under(position)
                    .and_then(|(window, location)| {
                        window
                            .surface_under(position - location.to_f64(), WindowSurfaceType::ALL)
                            .map(|(surface, offset)| (surface, (offset + location).to_f64()))
                    })
            })
            .or_else(|| {
                layers
                    .layer_under(Layer::Bottom, local)
                    .or_else(|| layers.layer_under(Layer::Background, local))
                    .and_then(layer_surface)
            })
    }

    fn monitor_at(&self, position: Point<f64, Logical>) -> Option<&Scroller> {
        self.scroller.monitors.iter().find(|monitor| {
            monitor
                .space
                .output_geometry(&monitor.output)
                .is_some_and(|geometry| geometry.to_f64().contains(position))
        })
    }

    pub fn relayout(&mut self, reveal_focus: bool) {
        let work_areas = self
            .scroller
            .monitors
            .iter()
            .filter_map(|monitor| {
                let output = monitor.space.output_geometry(&monitor.output)?;
                let zone = layer_map_for_output(&monitor.output).non_exclusive_zone();
                Some((monitor.output.name(), output, work_area(output, zone)))
            })
            .collect::<Vec<_>>();
        let floating = &self.scroller.floating;
        let animations = &mut self.scroller.animations;
        let monitors = &mut self.scroller.monitors;
        let now = self.start_time.elapsed();
        let duration = self
            .config
            .animations
            .active()
            .then_some(self.config.animations.duration);
        for (name, output_geometry, geometry) in work_areas {
            let Some(monitor) = monitors
                .iter_mut()
                .find(|monitor| monitor.output.name() == name)
            else {
                continue;
            };
            let Scroller {
                space,
                workspaces,
                active_workspace,
                ..
            } = monitor;
            let workspace = &mut workspaces[*active_workspace];
            let fractions = workspace
                .columns
                .iter()
                .map(|column| column.width.unwrap_or(self.config.layout.column_width))
                .collect::<Vec<_>>();
            let requested_width = fractions
                .iter()
                .map(|fraction| (geometry.size.w as f64 * fraction).round() as i32)
                .min()
                .unwrap_or(geometry.size.w);
            let gap = effective_gap(self.config.appearance.gaps, requested_width, geometry.size);
            let widths = fractions
                .iter()
                .map(|fraction| column_width(geometry.size.w, *fraction, gap))
                .collect::<Vec<_>>();
            let focused_column = workspace.focused_column();
            workspace.offset = viewport_offset(
                workspace.offset,
                focused_column,
                &widths,
                geometry.size.w,
                gap,
                self.config.layout.center_focused_column,
                reveal_focus,
            );
            let offset = workspace.offset;
            let mut column_x = geometry.loc.x + gap - offset;
            for (column, width) in workspace.columns.iter().zip(widths) {
                let rows = stacked_rows(geometry.loc.y, geometry.size.h, column.windows.len(), gap);
                for (window, row) in column.windows.iter().zip(rows) {
                    let fullscreen = workspace.fullscreen.as_ref() == Some(window);
                    let rectangle = if fullscreen {
                        output_geometry
                    } else {
                        Rectangle::new(
                            (column_x, row.loc.y).into(),
                            (width - gap, row.size.h).into(),
                        )
                    };
                    configure_window(window, rectangle, false);
                    let current = space.element_location(window);
                    let position = start_motion(
                        animations,
                        window.id(),
                        current,
                        rectangle.loc,
                        now,
                        duration,
                    );
                    space.map_element(window.clone(), position, false);
                }
                column_x += width;
            }
            for window in workspace
                .windows
                .iter()
                .filter(|window| floating.contains(&window.id()))
            {
                let fullscreen = workspace.fullscreen.as_ref() == Some(window);
                let rectangle = if fullscreen {
                    output_geometry
                } else {
                    centered_rectangle(geometry, floating_size(window))
                };
                configure_window(window, rectangle, !fullscreen);
                let current = space.element_location(window);
                let position = start_motion(
                    animations,
                    window.id(),
                    current,
                    rectangle.loc,
                    now,
                    duration,
                );
                space.map_element(window.clone(), position, false);
            }
        }
    }

    /// Re-maps every animating window at its position for the current instant.
    /// Progress comes from absolute time, so calling this repeatedly within one
    /// frame - once per output, and again per screencopy capture - is a no-op
    /// beyond the first.
    pub fn advance_animations(&mut self) {
        if self.scroller.animations.is_empty() {
            return;
        }
        let now = self.start_time.elapsed();
        let animations = &mut self.scroller.animations;
        for monitor in &mut self.scroller.monitors {
            let moved = monitor.workspaces[monitor.active_workspace]
                .windows
                .iter()
                .filter_map(|window| {
                    let animation = animations.get(&window.id())?;
                    Some((window.clone(), animation.position(now)))
                })
                .collect::<Vec<_>>();
            for (window, position) in moved {
                monitor.space.map_element(window, position, false);
            }
        }
        animations.retain(|_, animation| !animation.finished(now));
    }

    pub fn animations_active(&self) -> bool {
        !self.scroller.animations.is_empty()
    }

    pub(crate) fn sync_focus(&mut self) {
        let active_output = self.scroller.focused_output.as_deref();
        let focused_surface = self.exclusive_keyboard_surface().or_else(|| {
            self.scroller
                .monitors
                .iter()
                .find(|monitor| Some(monitor.output.name().as_str()) == active_output)
                .and_then(|monitor| {
                    let workspace = monitor.active();
                    workspace
                        .fullscreen
                        .as_ref()
                        .and_then(window_surface)
                        .or_else(|| {
                            workspace
                                .focused
                                .and_then(|index| window_surface(&workspace.windows[index]))
                        })
                })
        });

        for monitor in &self.scroller.monitors {
            for (workspace_index, workspace) in monitor.workspaces.iter().enumerate() {
                for (index, window) in workspace.windows.iter().enumerate() {
                    window.set_activated(
                        Some(monitor.output.name().as_str()) == active_output
                            && workspace_index == monitor.active_workspace
                            && if let Some(fullscreen) = &workspace.fullscreen {
                                window == fullscreen
                            } else {
                                Some(index) == workspace.focused
                            },
                    );
                    if let Some(surface) = window.toplevel()
                        && surface.is_initial_configure_sent()
                    {
                        surface.send_pending_configure();
                    }
                }
            }
        }
        self.seat.get_keyboard().unwrap().set_focus(
            self,
            focused_surface,
            SERIAL_COUNTER.next_serial(),
        );
    }

    pub(crate) fn has_exclusive_keyboard_layer(&self) -> bool {
        self.exclusive_keyboard_surface().is_some()
    }

    fn exclusive_keyboard_surface(&self) -> Option<WlSurface> {
        let focused_output = self.scroller.focused_output.as_deref();
        self.scroller
            .monitors
            .iter()
            .filter(|monitor| Some(monitor.output.name().as_str()) == focused_output)
            .chain(
                self.scroller
                    .monitors
                    .iter()
                    .filter(|monitor| Some(monitor.output.name().as_str()) != focused_output),
            )
            .find_map(|monitor| {
                let layers = layer_map_for_output(&monitor.output);
                layers
                    .layers()
                    .rev()
                    .find(|surface| {
                        let state = surface.cached_state();
                        is_exclusive_keyboard_layer(state.layer, state.keyboard_interactivity)
                    })
                    .map(|surface| surface.wl_surface().clone())
            })
    }
}

pub(crate) fn is_exclusive_keyboard_layer(
    layer: Layer,
    interactivity: KeyboardInteractivity,
) -> bool {
    matches!(layer, Layer::Top | Layer::Overlay)
        && interactivity == KeyboardInteractivity::Exclusive
}

fn window_surface(window: &Window) -> Option<WlSurface> {
    window
        .toplevel()
        .map(|surface| surface.wl_surface().clone())
        .or_else(|| {
            window
                .x11_surface()
                .and_then(|surface| surface.wl_surface())
        })
}

fn wayland_should_float(surface: &ToplevelSurface) -> bool {
    let (min_size, max_size, dialog_hint) = with_states(surface.wl_surface(), |states| {
        let mut cached = states.cached_state.get::<SurfaceCachedState>();
        let cached = cached.current();
        let dialog_hint = states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .unwrap()
            .lock()
            .unwrap()
            .dialog_hint;
        (cached.min_size, cached.max_size, dialog_hint)
    });
    surface.parent().is_some()
        || dialog_hint != ToplevelDialogHint::Unknown
        || fixed_size(min_size, max_size)
}

fn x11_should_float(surface: &smithay::xwayland::X11Surface) -> bool {
    let dialog_type = x11_dialog_type(surface.window_type());
    dialog_type
        || surface.is_modal()
        || surface.is_transient_for().is_some()
        || surface
            .min_size()
            .zip(surface.max_size())
            .is_some_and(|(min_size, max_size)| fixed_size(min_size, max_size))
}

fn x11_dialog_type(window_type: Option<WmWindowType>) -> bool {
    matches!(
        window_type,
        Some(WmWindowType::Dialog | WmWindowType::Splash | WmWindowType::Utility)
    )
}

fn fixed_size(min_size: Size<i32, Logical>, max_size: Size<i32, Logical>) -> bool {
    min_size.w > 0 && min_size.h > 0 && min_size == max_size
}

fn floating_size(window: &Window) -> Size<i32, Logical> {
    window
        .x11_surface()
        .map(|surface| surface.last_configure().size)
        .unwrap_or_else(|| window.geometry().size)
}

fn configure_window(window: &Window, rectangle: Rectangle<i32, Logical>, client_sized: bool) {
    if let Some(surface) = window.toplevel() {
        surface.with_pending_state(|state| state.size = (!client_sized).then_some(rectangle.size));
        if surface.is_initial_configure_sent() {
            surface.send_pending_configure();
        }
    } else if let Some(surface) = window.x11_surface() {
        let result = if surface.sync_request_supported() {
            surface.configure_with_sync(rectangle, None)
        } else {
            surface.configure(rectangle)
        };
        if let Err(error) = result {
            tracing::warn!(?error, "failed to configure X11 window");
        }
    }
}

fn set_fullscreen_state(
    surface: &ToplevelSurface,
    size: Option<Size<i32, Logical>>,
    output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
) {
    surface.with_pending_state(|state| {
        if size.is_some() {
            state.states.set(xdg_toplevel::State::Fullscreen);
        } else {
            state.states.unset(xdg_toplevel::State::Fullscreen);
        }
        state.size = size;
        state.fullscreen_output = output;
    });
    if surface.is_initial_configure_sent() {
        surface.send_pending_configure();
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct FitPlacement {
    source: Rectangle<f64, Logical>,
    target: Rectangle<f64, Logical>,
    scale: f64,
}

impl FitPlacement {
    fn source_point(&self, target_point: Point<f64, Logical>) -> Point<f64, Logical> {
        let point = target_point - self.target.loc;
        self.source.loc + Point::<f64, Logical>::from((point.x / self.scale, point.y / self.scale))
    }
}

fn aspect_fit(source: Rectangle<i32, Logical>, target: Size<i32, Logical>) -> Option<FitPlacement> {
    if source.size.w <= 0 || source.size.h <= 0 || target.w <= 0 || target.h <= 0 {
        return None;
    }
    let source = source.to_f64();
    let target = target.to_f64();
    let scale = (target.w / source.size.w).min(target.h / source.size.h);
    let size = source.size * scale;
    let location = ((target.w - size.w) / 2.0, (target.h - size.h) / 2.0).into();
    Some(FitPlacement {
        source,
        target: Rectangle::new(location, size),
        scale,
    })
}

fn directional_neighbor(
    current: &str,
    direction: Direction,
    geometries: &[(String, Rectangle<i32, Logical>)],
) -> Option<String> {
    let source = center(geometries.iter().find(|(name, _)| name == current)?.1);
    geometries
        .iter()
        .filter(|(name, _)| name != current)
        .filter_map(|(name, geometry)| {
            let target = center(*geometry);
            let (primary, secondary) = match direction {
                Direction::Left => (source.x - target.x, (source.y - target.y).abs()),
                Direction::Right => (target.x - source.x, (source.y - target.y).abs()),
                Direction::Up => (source.y - target.y, (source.x - target.x).abs()),
                Direction::Down => (target.y - source.y, (source.x - target.x).abs()),
            };
            (primary > 0).then_some((
                primary as i64 * primary as i64 + secondary as i64 * secondary as i64,
                name,
            ))
        })
        .min_by_key(|(score, _)| *score)
        .map(|(_, name)| name.clone())
}

fn center(rectangle: Rectangle<i32, Logical>) -> Point<i32, Logical> {
    rectangle.loc + rectangle.size / 2
}

fn centered_rectangle(
    area: Rectangle<i32, Logical>,
    size: Size<i32, Logical>,
) -> Rectangle<i32, Logical> {
    Rectangle::new(area.loc + (area.size - size) / 2, size)
}

fn work_area(
    output: Rectangle<i32, Logical>,
    non_exclusive_zone: Rectangle<i32, Logical>,
) -> Rectangle<i32, Logical> {
    Rectangle::new(output.loc + non_exclusive_zone.loc, non_exclusive_zone.size)
}

fn neighbor(current: usize, len: usize, delta: i32) -> Option<usize> {
    let next = current as i64 + delta as i64;
    (0..len as i64).contains(&next).then_some(next as usize)
}

fn swap_column_window<T>(column: &mut Column<T>, delta: i32) -> bool {
    let Some(next) = neighbor(column.focused, column.windows.len(), delta) else {
        return false;
    };
    column.windows.swap(column.focused, next);
    column.focused = next;
    true
}

fn consume_or_expel_columns<T>(columns: &mut Vec<Column<T>>, current: usize, delta: i32) -> bool {
    if columns[current].windows.len() == 1 {
        let Some(target) = neighbor(current, columns.len(), delta) else {
            return false;
        };
        let window = columns.remove(current).windows.remove(0);
        let target = if target > current { target - 1 } else { target };
        columns[target].windows.push(window);
        columns[target].focused = columns[target].windows.len() - 1;
    } else {
        let column = &mut columns[current];
        let row = column.focused;
        let window = column.windows.remove(row);
        column.focused = row.min(column.windows.len() - 1);
        let width = column.width;
        let target = if delta < 0 { current } else { current + 1 };
        columns.insert(
            target,
            Column {
                windows: vec![window],
                focused: 0,
                width,
            },
        );
    }
    true
}

fn cycle_column_width<T>(column: &mut Column<T>, default: f64, presets: &[f64]) {
    let current = column.width.unwrap_or(default);
    let next = presets
        .iter()
        .position(|width| (*width - current).abs() < f64::EPSILON)
        .map_or(0, |index| (index + 1) % presets.len());
    column.width = Some(presets[next]);
}

fn workspace_neighbor(current: usize, delta: i32) -> usize {
    (current as i32 + delta).rem_euclid(WORKSPACE_COUNT as i32) as usize
}

fn focus_after_removal(focused: Option<usize>, removed: usize, remaining: usize) -> Option<usize> {
    if remaining == 0 {
        return None;
    }
    focused.map(|focused| {
        if focused > removed {
            focused - 1
        } else {
            focused.min(remaining - 1)
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn viewport_offset(
    current: i32,
    focused: Option<usize>,
    widths: &[i32],
    output_width: i32,
    gap: i32,
    center: bool,
    reveal: bool,
) -> i32 {
    let visible_width = output_width - gap * 2;
    let max_offset = widths
        .iter()
        .copied()
        .sum::<i32>()
        .saturating_sub(gap)
        .saturating_sub(visible_width)
        .max(0);
    let mut offset = current.clamp(0, max_offset);
    let Some(focused) = focused.filter(|index| *index < widths.len()) else {
        return offset;
    };
    let start = widths[..focused].iter().copied().sum::<i32>();
    let window_width = widths[focused] - gap;
    if center {
        return start - (visible_width - window_width) / 2;
    }
    if reveal {
        if start < offset {
            offset = start;
        } else if start + window_width > offset + visible_width {
            offset = start + window_width - visible_width;
        }
    }
    offset.clamp(0, max_offset)
}

fn effective_gap(
    requested: i32,
    column_width: i32,
    output_size: smithay::utils::Size<i32, Logical>,
) -> i32 {
    requested
        .min((column_width - 1).max(0))
        .min(((output_size.w - 1) / 2).max(0))
        .min(((output_size.h - 1) / 2).max(0))
}

#[cfg(test)]
fn column_dimensions(
    output_size: Size<i32, Logical>,
    width_fraction: f64,
    requested_gap: i32,
) -> (i32, i32, Size<i32, Logical>) {
    let requested_width =
        ((output_size.w as f64 * width_fraction).round() as i32).clamp(1, output_size.w.max(1));
    let gap = effective_gap(requested_gap, requested_width, output_size);
    let column_width = column_width(output_size.w, width_fraction, gap);
    let window_size = (column_width - gap, output_size.h - gap * 2).into();
    (column_width, gap, window_size)
}

fn column_width(output_width: i32, fraction: f64, gap: i32) -> i32 {
    (((output_width - gap) as f64 * fraction).round() as i32).clamp(gap + 1, output_width - gap)
}

fn stacked_rows(
    output_y: i32,
    output_height: i32,
    count: usize,
    requested_gap: i32,
) -> Vec<Rectangle<i32, Logical>> {
    if count == 0 {
        return Vec::new();
    }
    let count = count as i32;
    let gap = requested_gap.min(((output_height - count) / (count + 1)).max(0));
    let available = (output_height - gap * (count + 1)).max(count);
    let height = available / count;
    let remainder = available % count;
    let mut y = output_y + gap;
    (0..count)
        .map(|row| {
            let row_height = height + i32::from(row < remainder);
            let rectangle = Rectangle::new((0, y).into(), (0, row_height).into());
            y += row_height + gap;
            rectangle
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::output::{PhysicalProperties, Subpixel};

    fn output(name: &str) -> Output {
        Output::new(
            name.into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Paw2".into(),
                model: "Test".into(),
                serial_number: "Unknown".into(),
            },
        )
    }

    #[test]
    fn navigation_viewport_and_monitor_direction() {
        assert_eq!(neighbor(0, 3, -1), None);
        assert_eq!(workspace_neighbor(0, -1), 8);
        assert_eq!(workspace_neighbor(8, 1), 0);
        assert_eq!(focus_after_removal(Some(2), 1, 2), Some(1));
        assert!(fixed_size((640, 480).into(), (640, 480).into()));
        assert!(!fixed_size((640, 480).into(), (0, 0).into()));
        assert!(x11_dialog_type(Some(WmWindowType::Dialog)));
        assert!(!x11_dialog_type(Some(WmWindowType::Normal)));
        assert_eq!(
            centered_rectangle(
                Rectangle::new((100, 50).into(), (1920, 1040).into()),
                (640, 480).into(),
            ),
            Rectangle::new((740, 330).into(), (640, 480).into())
        );
        assert_eq!(effective_gap(15, 600, (1000, 1000).into()), 15);
        assert_eq!(effective_gap(15, 10, (20, 20).into()), 9);
        assert_eq!(
            column_dimensions((100, 100).into(), 1.0, 15),
            (85, 15, (70, 70).into())
        );
        assert_eq!(
            column_dimensions((100, 100).into(), 0.5, 10),
            (45, 10, (35, 80).into())
        );
        assert_eq!(
            viewport_offset(0, Some(0), &[45, 45], 100, 10, false, true),
            0
        );
        assert_eq!(
            viewport_offset(0, Some(1), &[45, 45], 100, 10, false, true),
            0
        );
        assert_eq!(
            viewport_offset(45, None, &[45, 45, 45], 100, 10, false, true),
            45
        );
        let offset = viewport_offset(0, Some(2), &[45, 45, 45], 100, 10, false, true);
        assert_eq!(offset, 45);
        let visible_windows = [1, 2].map(|index| 10 + index * 45 - offset);
        assert_eq!(visible_windows, [10, 55]);
        assert_eq!(visible_windows[1] + 35, 100 - 10);
        assert_eq!(
            stacked_rows(50, 100, 2, 10),
            [
                Rectangle::new((0, 60).into(), (0, 35).into()),
                Rectangle::new((0, 105).into(), (0, 35).into()),
            ]
        );
        assert_eq!(
            viewport_offset(0, Some(2), &[30, 50, 70], 100, 10, false, true),
            60
        );

        let mut columns = vec![Column::new(1), Column::new(2)];
        assert!(consume_or_expel_columns(&mut columns, 1, -1));
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].windows, [1, 2]);
        assert_eq!(columns[0].focused, 1);
        columns[0].width = Some(0.5);
        assert!(consume_or_expel_columns(&mut columns, 0, 1));
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].windows, [1]);
        assert_eq!(columns[1].windows, [2]);
        assert_eq!(columns[1].width, Some(0.5));

        let mut column = Column {
            windows: vec![1, 2, 3],
            focused: 1,
            width: None,
        };
        assert!(swap_column_window(&mut column, -1));
        assert_eq!(column.windows, [2, 1, 3]);
        assert_eq!(column.focused, 0);
        assert!(!swap_column_window(&mut column, -1));
        assert!(swap_column_window(&mut column, 1));
        assert_eq!(column.windows, [1, 2, 3]);
        assert_eq!(column.focused, 1);

        let mut column = Column::new(1);
        cycle_column_width(&mut column, 0.6, &[0.33333, 0.5, 0.66667]);
        assert_eq!(column.width, Some(0.33333));
        cycle_column_width(&mut column, 0.6, &[0.33333, 0.5, 0.66667]);
        assert_eq!(column.width, Some(0.5));

        let geometries = vec![
            (
                "left".into(),
                Rectangle::new((0, 0).into(), (100, 100).into()),
            ),
            (
                "right".into(),
                Rectangle::new((100, 0).into(), (100, 100).into()),
            ),
            (
                "down".into(),
                Rectangle::new((0, 100).into(), (100, 100).into()),
            ),
        ];
        assert_eq!(
            directional_neighbor("left", Direction::Right, &geometries).as_deref(),
            Some("right")
        );
        assert_eq!(
            directional_neighbor("left", Direction::Down, &geometries).as_deref(),
            Some("down")
        );
    }

    #[test]
    fn easing_is_bounded_and_monotonic() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        // Out-of-range input is clamped rather than extrapolated.
        assert_eq!(ease_out_cubic(-1.0), 0.0);
        assert_eq!(ease_out_cubic(2.0), 1.0);
        // Decelerating: past the halfway point in time, past it in distance.
        assert!(ease_out_cubic(0.5) > 0.5);
        let mut previous = 0.0;
        for step in 1..=10 {
            let current = ease_out_cubic(f64::from(step) / 10.0);
            assert!(current > previous, "not monotonic at {step}");
            previous = current;
        }
    }

    #[test]
    fn animation_interpolates_then_snaps_to_target() {
        let animation = WindowAnimation {
            start: Duration::from_millis(100),
            duration: Duration::from_millis(200),
            from: (0, 100).into(),
            to: (400, 100).into(),
            fade_in: false,
        };

        assert_eq!(
            animation.position(Duration::from_millis(100)),
            (0, 100).into()
        );
        assert!(!animation.finished(Duration::from_millis(100)));

        // Eased, so it is already past halfway at the halfway point.
        let middle = animation.position(Duration::from_millis(200));
        assert!(middle.x > 200 && middle.x < 400, "unexpected {middle:?}");
        assert_eq!(middle.y, 100);

        // Landing exactly on target matters: this is what stops a window from
        // being stranded a pixel short once repainting ceases.
        assert_eq!(
            animation.position(Duration::from_millis(300)),
            (400, 100).into()
        );
        assert!(animation.finished(Duration::from_millis(300)));
        assert_eq!(
            animation.position(Duration::from_secs(9)),
            (400, 100).into()
        );

        // Without fade_in the window is always fully opaque.
        assert_eq!(animation.opacity(Duration::from_millis(150)), 1.0);
    }

    #[test]
    fn a_newly_mapped_window_fades_instead_of_sliding() {
        let mut animations = HashMap::new();
        let now = Duration::from_millis(500);
        let duration = Some(Duration::from_millis(200));

        // No current position means the window was just mapped.
        let position = start_motion(&mut animations, 1, None, (10, 20).into(), now, duration);
        assert_eq!(position, (10, 20).into());
        let animation = &animations[&1];
        assert!(animation.fade_in);
        assert_eq!(animation.from, animation.to);
        assert_eq!(animation.opacity(now), 0.0);
        assert_eq!(animation.opacity(now + Duration::from_millis(200)), 1.0);
    }

    #[test]
    fn motion_starts_where_the_window_currently_sits() {
        let mut animations = HashMap::new();
        let now = Duration::from_millis(500);
        let duration = Some(Duration::from_millis(200));

        // A move records a glide and keeps the window at its old spot for now.
        let from = Point::from((0, 0));
        let position = start_motion(
            &mut animations,
            1,
            Some(from),
            (100, 0).into(),
            now,
            duration,
        );
        assert_eq!(position, from);
        assert_eq!(animations[&1].to, (100, 0).into());
        assert!(!animations[&1].fade_in);

        // Re-running the layout at the same target must not restart the glide.
        let later = now + Duration::from_millis(100);
        let mid = animations[&1].position(later);
        let position = start_motion(
            &mut animations,
            1,
            Some(mid),
            (100, 0).into(),
            later,
            duration,
        );
        assert_eq!(position, mid);
        assert_eq!(animations[&1].start, now, "glide was restarted");

        // Retargeting mid-flight starts from where it visually is.
        let position = start_motion(
            &mut animations,
            1,
            Some(mid),
            (0, 50).into(),
            later,
            duration,
        );
        assert_eq!(position, mid);
        assert_eq!(animations[&1].from, mid);
        assert_eq!(animations[&1].to, (0, 50).into());

        // Once the glide has run its course, a settled window holds no state.
        // While it is still in flight the entry survives, which is what keeps
        // repeated layouts from restarting it.
        let done = later + Duration::from_millis(200);
        let settled = Point::from((0, 50));
        start_motion(&mut animations, 1, Some(settled), settled, done, duration);
        assert!(animations.is_empty());
    }

    #[test]
    fn disabling_animations_snaps_and_forgets() {
        let mut animations = HashMap::new();
        let now = Duration::from_millis(500);
        start_motion(
            &mut animations,
            1,
            Some((0, 0).into()),
            (100, 0).into(),
            now,
            Some(Duration::from_millis(200)),
        );
        assert!(!animations.is_empty());

        // Turning animations off mid-glide drops straight to the target.
        let position = start_motion(
            &mut animations,
            1,
            Some((40, 0).into()),
            (100, 0).into(),
            now,
            None,
        );
        assert_eq!(position, (100, 0).into());
        assert!(animations.is_empty());
    }

    #[test]
    fn only_exclusive_upper_layers_take_keyboard_focus() {
        assert!(is_exclusive_keyboard_layer(
            Layer::Top,
            KeyboardInteractivity::Exclusive
        ));
        assert!(is_exclusive_keyboard_layer(
            Layer::Overlay,
            KeyboardInteractivity::Exclusive
        ));
        assert!(!is_exclusive_keyboard_layer(
            Layer::Top,
            KeyboardInteractivity::OnDemand
        ));
        assert!(!is_exclusive_keyboard_layer(
            Layer::Bottom,
            KeyboardInteractivity::Exclusive
        ));
    }

    #[test]
    fn layer_zones_are_positioned_inside_their_output() {
        let output = Rectangle::new((100, 50).into(), (1920, 1080).into());
        assert_eq!(
            work_area(output, Rectangle::new((0, 40).into(), (1920, 1040).into())),
            Rectangle::new((100, 90).into(), (1920, 1040).into())
        );
        assert_eq!(
            work_area(output, Rectangle::new((0, 0).into(), (1920, 1040).into())),
            Rectangle::new((100, 50).into(), (1920, 1040).into())
        );
        assert_eq!(
            work_area(output, Rectangle::new((40, 0).into(), (1880, 1080).into())),
            Rectangle::new((140, 50).into(), (1880, 1080).into())
        );
        assert_eq!(
            work_area(output, Rectangle::new((0, 0).into(), (1880, 1080).into())),
            Rectangle::new((100, 50).into(), (1880, 1080).into())
        );
    }

    #[test]
    fn fullscreen_aspect_fit_and_pointer_mapping() {
        let exact = aspect_fit(
            Rectangle::new((0, 0).into(), (1920, 1080).into()),
            (1920, 1080).into(),
        )
        .unwrap();
        assert_eq!(exact.scale, 1.0);
        assert_eq!(exact.target, Rectangle::from_size((1920.0, 1080.0).into()));

        let column = aspect_fit(
            Rectangle::new((0, 0).into(), (960, 1080).into()),
            (1920, 1080).into(),
        )
        .unwrap();
        assert_eq!(
            column.target,
            Rectangle::new((480.0, 0.0).into(), (960.0, 1080.0).into())
        );

        let lower_resolution = aspect_fit(
            Rectangle::new((0, 0).into(), (1280, 720).into()),
            (1920, 1080).into(),
        )
        .unwrap();
        assert_eq!(lower_resolution.scale, 1.5);
        assert_eq!(
            lower_resolution.source_point((960.0, 540.0).into()),
            (640.0, 360.0).into()
        );

        let four_three = aspect_fit(
            Rectangle::new((0, 0).into(), (1024, 768).into()),
            (1920, 1080).into(),
        )
        .unwrap();
        assert_eq!(four_three.target.loc, (240.0, 0.0).into());
        assert_eq!(four_three.target.size, (1440.0, 1080.0).into());
        assert!(aspect_fit(Rectangle::default(), (1920, 1080).into()).is_none());
    }

    #[test]
    fn monitor_spaces_only_contain_their_own_output() {
        let left = Scroller::new(output("left"), (0, 0).into());
        let right = Scroller::new(output("right"), (1920, 0).into());

        assert_eq!(
            left.space.outputs().map(Output::name).collect::<Vec<_>>(),
            ["left"]
        );
        assert_eq!(
            right.space.outputs().map(Output::name).collect::<Vec<_>>(),
            ["right"]
        );
    }
}
