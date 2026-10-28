//! Pointer-driven window manipulation: moving and resizing floating windows,
//! dragging tiles between columns and monitors, dragging tab groups, and
//! dragging the dividers between tiles.
//!
//! Every one of these begins in [`Paw2::begin_mouse_operation`], is carried by
//! [`Paw2::update_mouse_operation`], and ends in
//! [`Paw2::finish_mouse_operation`].

use smithay::{
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge,
    wayland::shell::xdg::ToplevelSurface,
};

use super::*;

/// How far the pointer travels before a press on a titlebar becomes a move,
/// in logical pixels. Below it the press is a click.
const TITLE_DRAG_THRESHOLD: f64 = 6.0;
/// The most time between two presses on a titlebar that still makes a
/// double-click, in milliseconds.
const DOUBLE_CLICK_MS: u32 = 400;

impl Paw2 {
    /// A left press on a bar, its window already focused. Returns the grab to
    /// start when the press may turn into a drag.
    pub(crate) fn press_title_bar(
        &mut self,
        window: Window,
        part: BarPart,
        time: u32,
    ) -> Option<MouseOperation> {
        match part {
            BarPart::Close => {
                self.run_action(Action::CloseWindow);
                None
            }
            BarPart::Minimize => {
                self.minimize_window(&window);
                None
            }
            BarPart::Float => {
                self.run_action(Action::ToggleFloating);
                None
            }
            // A tab only selects; its group moves with the tab-group drag.
            BarPart::Title if self.in_tab_group(&window) => None,
            BarPart::Title => {
                let previous = self.scroller.last_title_press.replace((window.id(), time));
                if previous.is_some_and(|(id, at)| {
                    id == window.id() && time.wrapping_sub(at) <= DOUBLE_CLICK_MS
                }) {
                    self.scroller.last_title_press = None;
                    self.run_action(Action::ToggleFloating);
                    return None;
                }
                Some(MouseOperation::PressTitle { window })
            }
        }
    }

    fn in_tab_group(&self, window: &Window) -> bool {
        self.scroller.monitors.iter().any(|monitor| {
            monitor.active().windows.contains(window)
                && monitor.active().tab_group_index(window).is_some()
        })
    }

    /// A titlebar always moves its window, never resizes it: a floating window
    /// follows the pointer, a tiled one becomes the drag preview Super+drag
    /// gives.
    fn begin_titlebar_move(
        &mut self,
        window: Window,
        location: Point<f64, Logical>,
    ) -> Option<MouseOperation> {
        let monitor = self
            .scroller
            .monitors
            .iter()
            .find(|monitor| monitor.active().windows.contains(&window))?;
        let workspace = monitor.active();
        if workspace.fullscreen.is_some() {
            return None;
        }
        if workspace.is_floating(&window) {
            return Some(MouseOperation::MoveFloating {
                initial_location: monitor.space.element_location(&window)?,
                window,
            });
        }
        self.begin_tiled_drag(window, location, false)
    }

    /// An app asked to move its window - a drag on its own header bar.
    pub(crate) fn app_move_operation(
        &mut self,
        surface: &ToplevelSurface,
        location: Point<f64, Logical>,
    ) -> Option<MouseOperation> {
        let window = self
            .find_window(|window| window.toplevel() == Some(surface))?
            .clone();
        self.begin_titlebar_move(window, location)
    }

    /// An app asked to resize its window from `edges`. Only a floating window
    /// can be: the layout sizes tiles.
    pub(crate) fn app_resize_operation(
        &mut self,
        surface: &ToplevelSurface,
        edges: ResizeEdge,
    ) -> Option<MouseOperation> {
        let window = self
            .find_window(|window| window.toplevel() == Some(surface))?
            .clone();
        let monitor = self
            .scroller
            .monitors
            .iter()
            .find(|monitor| monitor.active().windows.contains(&window))?;
        if !monitor.active().is_floating(&window) || window_is_fixed_size(&window) {
            return None;
        }
        let x = match edges {
            ResizeEdge::Left | ResizeEdge::TopLeft | ResizeEdge::BottomLeft => {
                Some(FloatingEdge::Start)
            }
            ResizeEdge::Right | ResizeEdge::TopRight | ResizeEdge::BottomRight => {
                Some(FloatingEdge::End)
            }
            _ => None,
        };
        let y = match edges {
            ResizeEdge::Top | ResizeEdge::TopLeft | ResizeEdge::TopRight => {
                Some(FloatingEdge::Start)
            }
            ResizeEdge::Bottom | ResizeEdge::BottomLeft | ResizeEdge::BottomRight => {
                Some(FloatingEdge::End)
            }
            _ => None,
        };
        Some(MouseOperation::ResizeFloating {
            initial_geometry: monitor.space.element_geometry(&window)?,
            window,
            x,
            y,
        })
    }

    pub(crate) fn begin_mouse_operation(
        &mut self,
        window: Window,
        location: Point<f64, Logical>,
    ) -> Option<MouseOperation> {
        if let Some(name) = self.scratchpad_name_for_window(&window) {
            let runtime = self.scroller.scratchpads.get(&name)?;
            let output = runtime.output.as_deref()?;
            let monitor = self.scroller.monitor(output)?;
            let geometry = monitor.space.element_geometry(&window)?;
            let (x, y) = floating_resize_edges(location, geometry, 8);
            if !window_is_fixed_size(&window) && (x.is_some() || y.is_some()) {
                return Some(MouseOperation::ResizeScratchpad {
                    name,
                    initial_geometry: geometry,
                    x,
                    y,
                });
            }
            return Some(MouseOperation::MoveScratchpad {
                name,
                initial_location: geometry.loc,
            });
        }
        let monitor = self
            .scroller
            .monitors
            .iter()
            .find(|monitor| monitor.active().windows.contains(&window))?;
        let workspace = monitor.active();
        if workspace.fullscreen.is_some() {
            return None;
        }
        if workspace.is_floating(&window) {
            let geometry = monitor.space.element_geometry(&window)?;
            let (x, y) = floating_resize_edges(location, geometry, 8);
            if !window_is_fixed_size(&window) && (x.is_some() || y.is_some()) {
                return Some(MouseOperation::ResizeFloating {
                    window,
                    initial_geometry: geometry,
                    x,
                    y,
                });
            }
            return Some(MouseOperation::MoveFloating {
                initial_location: monitor.space.element_location(&window)?,
                window,
            });
        }
        if workspace.layout != LayoutMode::Scrolling {
            if let Some(operation) =
                self.begin_layout_resize(monitor, workspace, window.clone(), location)
            {
                return Some(operation);
            }
            return self.begin_tiled_drag(window, location, false);
        }
        let geometry = monitor.space.element_geometry(&window)?;
        let column = workspace
            .columns
            .iter()
            .find(|column| column.windows.contains(&window))?;
        let row = column.windows.iter().position(|item| item == &window)?;
        let column_edge = column_edge(location.x, geometry, 8);
        let row_edge = row_boundary(location.y, geometry, row, column.windows.len(), 8);
        // A lone window held at full width has no column edge to drag.
        let full_width = lone_full_width(&self.config) && is_lone_window(&workspace.columns);
        let x = column_edge
            .filter(|_| !full_width)
            .map(|(edge, _)| AxisResize::Column {
                edge,
                initial_width: column
                    .width
                    .unwrap_or(self.config.layout.scrolling.column_width),
                initial_offset: workspace.offset,
            });
        let y = row_edge.and_then(|(boundary, _)| {
            let initial_heights = column
                .windows
                .iter()
                .map(|window| {
                    monitor
                        .space
                        .element_geometry(window)
                        .map(|geometry| f64::from(geometry.size.h))
                })
                .collect::<Option<Vec<_>>>()?;
            Some(AxisResize::Rows {
                master: false,
                start: 0,
                boundary,
                initial_heights,
            })
        });
        if x.is_some() || y.is_some() {
            return Some(MouseOperation::ResizeTiled { window, x, y });
        }
        self.begin_tiled_drag(window, location, false)
    }

    fn begin_tiled_drag(
        &mut self,
        window: Window,
        location: Point<f64, Logical>,
        tab_group: bool,
    ) -> Option<MouseOperation> {
        let monitor_index = self.scroller.monitor_index_holding(&window)?;
        let monitor = &self.scroller.monitors[monitor_index];
        let origin_workspace = monitor.active_workspace;
        let origin_geometry = monitor.space.element_geometry(&window)?;
        let preview_size = drag_preview_size(origin_geometry.size);
        let grab_offset = drag_grab_offset(location, origin_geometry);
        let area = monitor.work_area()?;
        let preview_location =
            constrain_window(area, preview_size, (location - grab_offset).to_i32_round());
        configure_window(
            &window,
            Rectangle::new(preview_location, preview_size),
            false,
        );
        let monitor = &mut self.scroller.monitors[monitor_index];
        monitor
            .space
            .map_element(window.clone(), preview_location, true);
        monitor.space.raise_element(&window, false);
        self.scroller.animations.remove(&window.id());
        self.scroller.drag_preview = Some(window.id());
        Some(MouseOperation::MoveTiled {
            window,
            tab_group,
            origin_output: monitor.name.clone(),
            origin_workspace,
            origin_geometry,
            preview_size,
            grab_offset,
        })
    }

    pub(crate) fn begin_tab_group_drag(
        &mut self,
        window: Window,
        location: Point<f64, Logical>,
    ) -> Option<MouseOperation> {
        let tiled = self.scroller.monitors.iter().any(|monitor| {
            let workspace = monitor.active();
            workspace.fullscreen.is_none() && !workspace.is_floating(&window)
        });
        tiled.then(|| self.begin_tiled_drag(window, location, true))?
    }

    fn begin_layout_resize(
        &self,
        monitor: &Scroller,
        workspace: &Workspace,
        window: Window,
        location: Point<f64, Logical>,
    ) -> Option<MouseOperation> {
        let area = monitor.work_area()?;
        let tiled = workspace
            .columns
            .iter()
            .flat_map(|column| column.windows.iter())
            .collect::<Vec<_>>();
        let index = tiled.iter().position(|candidate| *candidate == &window)?;
        let gap = effective_gap(
            workspace_gap(workspace, &self.config),
            area.size.w,
            area.size,
        );
        let inner = inset_rectangle(area, gap);
        match workspace.layout {
            LayoutMode::Dwindle => {
                let config = &self.config.layout.dwindle;
                let cuts = dwindle_cuts(inner, tiled.len(), gap, &workspace.dwindle_ratios, config);
                let (vertical, horizontal) = dwindle_dividers_at(&cuts, location, 8);
                let split = |hit: DividerHit| AxisResize::Split {
                    target: LayoutResizeTarget::Dwindle(hit.split),
                    initial_ratio: workspace
                        .dwindle_ratios
                        .get(hit.split)
                        .copied()
                        .flatten()
                        .unwrap_or(config.split_ratio),
                    length: hit.length,
                    reversed: false,
                };
                (vertical.is_some() || horizontal.is_some()).then(|| MouseOperation::ResizeTiled {
                    window,
                    x: vertical.map(split),
                    y: horizontal.map(split),
                })
            }
            LayoutMode::MasterStack if !tiled.is_empty() => {
                // Worked out with the master area on the left (see `MasterFrame`),
                // then handed back on the axes the output actually has.
                let config = &self.config.layout.master_stack;
                let frame = MasterFrame::new(inner, config.position);
                let inner = frame.area();
                let pointer = frame.point_to_frame(location);
                let master_count = workspace.effective_master_count(tiled.len(), config);
                let ratio = workspace.master_ratio(config);
                let split = (master_count < tiled.len())
                    .then(|| {
                        let (master, stack) = split_rectangle(inner, true, ratio, gap);
                        divider_distance(pointer, master, stack, true, 8).map(|_| {
                            AxisResize::Split {
                                target: LayoutResizeTarget::Master,
                                initial_ratio: ratio,
                                length: split_length(inner, true, gap),
                                // With the master area on the right or at the
                                // bottom, dragging towards it shrinks it.
                                reversed: config.position.mirrored(),
                            }
                        })
                    })
                    .flatten();
                let range = if index < master_count {
                    0..master_count
                } else {
                    master_count..tiled.len()
                };
                let frame_geometry = |window: &Window| {
                    monitor
                        .space
                        .element_geometry(window)
                        .map(|geometry| frame.to_frame(geometry))
                };
                let row = frame_geometry(&window).and_then(|geometry| {
                    row_boundary(pointer.y, geometry, index - range.start, range.len(), 8)
                });
                let rows = row.and_then(|(boundary, _)| {
                    let initial_heights = tiled[range.clone()]
                        .iter()
                        .map(|window| {
                            frame_geometry(window).map(|geometry| f64::from(geometry.size.h))
                        })
                        .collect::<Option<Vec<_>>>()?;
                    Some(AxisResize::Rows {
                        master: true,
                        start: range.start,
                        boundary,
                        initial_heights,
                    })
                });
                let (x, y) = if config.position.transposed() {
                    (rows, split)
                } else {
                    (split, rows)
                };
                (x.is_some() || y.is_some()).then(|| MouseOperation::ResizeTiled { window, x, y })
            }
            LayoutMode::MasterStack | LayoutMode::Monocle | LayoutMode::Scrolling => None,
        }
    }

    pub(crate) fn update_mouse_operation(
        &mut self,
        operation: &mut MouseOperation,
        start: Point<f64, Logical>,
        location: Point<f64, Logical>,
    ) {
        if let MouseOperation::PressTitle { window } = operation {
            let travel = location - start;
            if travel.x.hypot(travel.y) < TITLE_DRAG_THRESHOLD {
                return;
            }
            let Some(moving) = self.begin_titlebar_move(window.clone(), start) else {
                return;
            };
            *operation = moving;
        }
        match operation {
            MouseOperation::PressTitle { .. } => {}
            MouseOperation::MoveScratchpad {
                name,
                initial_location,
            } => self.drag_scratchpad(name, *initial_location, None, None, location - start),
            MouseOperation::ResizeScratchpad {
                name,
                initial_geometry,
                x,
                y,
            } => self.drag_scratchpad(
                name,
                initial_geometry.loc,
                Some(*initial_geometry),
                Some((*x, *y)),
                location - start,
            ),
            MouseOperation::MoveTiled {
                window,
                preview_size,
                grab_offset,
                ..
            } => self.drag_tiled_preview(window, *preview_size, *grab_offset, location),
            MouseOperation::MoveFloating {
                window,
                initial_location,
            } => self.drag_floating(window, *initial_location, location - start, location),
            MouseOperation::ResizeFloating {
                window,
                initial_geometry,
                x,
                y,
            } => self.drag_floating_resize(window, *initial_geometry, *x, *y, location - start),
            MouseOperation::ResizeTiled { window, x, y } => {
                if let Some(resize) = x {
                    self.drag_axis_resize(window, resize, location.x - start.x);
                }
                if let Some(resize) = y {
                    self.drag_axis_resize(window, resize, location.y - start.y);
                }
            }
        }
    }

    fn drag_scratchpad(
        &mut self,
        name: &str,
        initial_location: Point<i32, Logical>,
        initial_geometry: Option<Rectangle<i32, Logical>>,
        edges: Option<(Option<FloatingEdge>, Option<FloatingEdge>)>,
        delta: Point<f64, Logical>,
    ) {
        let Some(runtime) = self.scroller.scratchpads.get(name) else {
            return;
        };
        let (Some(window), Some(output)) = (runtime.window.clone(), runtime.output.clone()) else {
            return;
        };
        let Some(monitor) = self.scroller.monitor_mut(&output) else {
            return;
        };
        let Some(area) = monitor.work_area() else {
            return;
        };
        let rectangle = match (initial_geometry, edges) {
            (Some(initial), Some((x, y))) => {
                resized_floating_rectangle(initial, x, y, delta, area, MIN_ROW_HEIGHT)
            }
            _ => {
                let size = monitor
                    .space
                    .element_geometry(&window)
                    .map(|geometry| geometry.size)
                    .unwrap_or_else(|| floating_size(&window));
                Rectangle::new(
                    constrain_window(
                        area,
                        size,
                        (initial_location.to_f64() + delta).to_i32_round(),
                    ),
                    size,
                )
            }
        };
        configure_window(&window, rectangle, false);
        monitor
            .space
            .map_element(window.clone(), rectangle.loc, true);
        monitor.space.raise_element(&window, false);
        self.scroller.animations.remove(&window.id());
    }

    fn drag_axis_resize(&mut self, window: &Window, resize: &AxisResize, delta: f64) {
        match resize {
            AxisResize::Column {
                edge,
                initial_width,
                initial_offset,
            } => self.drag_column_edge(window, *edge, *initial_width, *initial_offset, delta),
            AxisResize::Rows {
                master,
                start,
                boundary,
                initial_heights,
            } if *master => {
                self.drag_master_row_boundary(window, *start, *boundary, initial_heights, delta)
            }
            AxisResize::Rows {
                boundary,
                initial_heights,
                ..
            } => self.drag_row_boundary(window, *boundary, initial_heights, delta),
            AxisResize::Split {
                target,
                initial_ratio,
                length,
                reversed,
            } => {
                let delta = if *reversed { -delta } else { delta };
                self.drag_layout_split(window, *target, *initial_ratio, delta, *length)
            }
        }
    }

    fn drag_floating(
        &mut self,
        window: &Window,
        initial_location: Point<i32, Logical>,
        delta: Point<f64, Logical>,
        pointer: Point<f64, Logical>,
    ) {
        let Some(source) = self.scroller.monitor_index_holding(window) else {
            return;
        };
        let size = self
            .scroller
            .manual_floating
            .get(&window.id())
            .and_then(|override_| override_.size)
            .unwrap_or_else(|| floating_size(window));
        let desired = initial_location.to_f64() + delta;
        let Some(target) = self.scroller.monitors.iter().position(|monitor| {
            monitor
                .space
                .output_geometry(&monitor.output)
                .is_some_and(|geometry| geometry.to_f64().contains(pointer))
        }) else {
            return;
        };
        if source != target {
            if self.scroller.monitors[target].active().fullscreen.is_some() {
                return;
            }
            let Some(window) =
                self.scroller.monitors[source].remove_where(|candidate| candidate == window)
            else {
                return;
            };
            self.scroller.monitors[target]
                .active_mut()
                .add(window, true);
            self.scroller.focused_output = Some(self.scroller.monitors[target].name.clone());
        }
        let monitor = &mut self.scroller.monitors[target];
        let Some(area) = monitor.work_area() else {
            return;
        };
        let location = constrain_window(area, size, desired.to_i32_round());
        monitor.space.map_element(window.clone(), location, true);
        self.scroller.animations.remove(&window.id());
        if let Some(override_) = self.scroller.manual_floating.get_mut(&window.id()) {
            override_.location = Some(location);
        }
        self.sync_focus();
    }

    fn drag_floating_resize(
        &mut self,
        window: &Window,
        initial: Rectangle<i32, Logical>,
        x: Option<FloatingEdge>,
        y: Option<FloatingEdge>,
        delta: Point<f64, Logical>,
    ) {
        let Some(monitor) = self
            .scroller
            .monitors
            .iter_mut()
            .find(|monitor| monitor.active().windows.contains(window))
        else {
            return;
        };
        let Some(area) = monitor.work_area() else {
            return;
        };
        let rectangle = resized_floating_rectangle(initial, x, y, delta, area, MIN_ROW_HEIGHT);
        configure_window(window, rectangle, false);
        monitor
            .space
            .map_element(window.clone(), rectangle.loc, true);
        monitor.space.raise_element(window, false);
        self.scroller.animations.remove(&window.id());
        let override_ =
            self.scroller
                .manual_floating
                .entry(window.id())
                .or_insert(ManualFloating {
                    floating: true,
                    width: None,
                    size: None,
                    location: None,
                });
        override_.size = Some(rectangle.size);
        override_.location = Some(rectangle.loc);
    }

    fn drag_tiled_preview(
        &mut self,
        window: &Window,
        size: Size<i32, Logical>,
        grab_offset: Point<f64, Logical>,
        pointer: Point<f64, Logical>,
    ) {
        let Some(target) = self.scroller.monitors.iter().position(|monitor| {
            monitor
                .space
                .output_geometry(&monitor.output)
                .is_some_and(|geometry| geometry.to_f64().contains(pointer))
        }) else {
            return;
        };
        if self.scroller.monitors[target].active().fullscreen.is_some() {
            return;
        }
        for (index, monitor) in self.scroller.monitors.iter_mut().enumerate() {
            if index != target {
                monitor.space.unmap_elem(window);
            }
        }
        let monitor = &mut self.scroller.monitors[target];
        let Some(area) = monitor.work_area() else {
            return;
        };
        let location = constrain_window(area, size, (pointer - grab_offset).to_i32_round());
        monitor.space.map_element(window.clone(), location, true);
        monitor.space.raise_element(window, false);
    }

    pub(crate) fn finish_mouse_operation(
        &mut self,
        operation: &MouseOperation,
        pointer: Point<f64, Logical>,
    ) {
        match operation {
            MouseOperation::MoveScratchpad { name, .. }
            | MouseOperation::ResizeScratchpad { name, .. } => {
                self.remember_scratchpad_geometry(name);
                return;
            }
            _ => {}
        }
        let MouseOperation::MoveTiled {
            window,
            tab_group,
            origin_output,
            origin_workspace,
            origin_geometry,
            ..
        } = operation
        else {
            return;
        };
        let Some(source) = self.scroller.monitor_index(origin_output) else {
            self.scroller.drag_preview = None;
            return;
        };
        if *tab_group {
            self.finish_tab_group_drag(window, source, *origin_workspace, pointer);
            return;
        }
        let Some(target) = self.scroller.monitors.iter().position(|monitor| {
            monitor
                .space
                .output_geometry(&monitor.output)
                .is_some_and(|geometry| geometry.to_f64().contains(pointer))
        }) else {
            self.restore_tiled_drag(window, source);
            return;
        };
        let same_origin = source == target
            && self.scroller.monitors[source].active_workspace == *origin_workspace;
        let target_window = if same_origin && origin_geometry.to_f64().contains(pointer) {
            None
        } else {
            tiled_drop_target(
                self.scroller.monitors[target].active(),
                &self.scroller.monitors[target].space,
                window,
                pointer,
            )
        };
        if same_origin {
            // Dropping onto another window on the screen you are already on is a
            // reorder, and trading places is what that should do.
            if let Some(target_window) = target_window {
                let workspace = &mut self.scroller.monitors[source].workspaces[*origin_workspace];
                if let Some(index) = workspace.tiled_slots().iter().position(|&(column, row)| {
                    workspace.columns[column].windows[row] == target_window
                }) {
                    workspace.swap_tiled_at(index);
                }
            }
        } else {
            // Onto another monitor it is a move, never a trade: nothing comes
            // back the other way. `target_window` only says *where* to land -
            // `tiled_drop_target` falls back to the nearest window by centre, so
            // it is set for any drop onto a workspace that is not empty, and
            // treating it as a swap partner is what made every drag exchange.
            let moved = self.scroller.monitors[source].workspaces[*origin_workspace]
                .remove_where(|candidate| candidate == window);
            if let Some(window) = moved {
                let target_workspace = self.scroller.monitors[target].active_mut();
                let column = target_window.and_then(|target| {
                    target_workspace
                        .columns
                        .iter()
                        .position(|column| column.windows.contains(&target))
                });
                match column {
                    Some(index) => target_workspace.add_group_at(
                        WindowGroup {
                            windows: vec![window.clone()],
                            tiled: vec![window],
                            row_weights: vec![1.0],
                            focused: 0,
                            width: None,
                            fullscreen: None,
                            floating: false,
                            tab_groups: Vec::new(),
                        },
                        index,
                    ),
                    // Nothing under the pointer to land beside: the workspace is
                    // empty, so the usual placement is the only placement.
                    None => target_workspace.add(window, false),
                }
                self.scroller.focused_output = Some(self.scroller.monitors[target].name.clone());
            }
        }
        self.scroller.drag_preview = None;
        for (index, monitor) in self.scroller.monitors.iter_mut().enumerate() {
            if index != target {
                monitor.space.unmap_elem(window);
            }
        }
        self.relayout(true);
        self.sync_focus();
    }

    fn finish_tab_group_drag(
        &mut self,
        window: &Window,
        source: usize,
        origin_workspace: usize,
        pointer: Point<f64, Logical>,
    ) {
        let target_monitor = self.scroller.monitors.iter().position(|monitor| {
            monitor
                .space
                .output_geometry(&monitor.output)
                .is_some_and(|geometry| geometry.to_f64().contains(pointer))
        });
        let Some(target_monitor) = target_monitor else {
            self.restore_tiled_drag(window, source);
            return;
        };
        let monitor = &self.scroller.monitors[target_monitor];
        let workspace = monitor.active();
        if workspace.fullscreen.is_some() {
            self.restore_tiled_drag(window, source);
            return;
        }
        let target_window = self
            .tab_at(pointer)
            .filter(|target| target != window && workspace.windows.contains(target))
            .or_else(|| {
                workspace
                    .columns
                    .iter()
                    .flat_map(|column| column.windows.iter())
                    .rev()
                    .find(|target| {
                        *target != window
                            && monitor
                                .space
                                .element_geometry(target)
                                .is_some_and(|geometry| geometry.to_f64().contains(pointer))
                    })
                    .cloned()
            });
        let Some(target_window) = target_window else {
            self.restore_tiled_drag(window, source);
            return;
        };
        let moved = self.scroller.monitors[source].workspaces[origin_workspace]
            .remove_where(|candidate| candidate == window);
        let Some(moved) = moved else {
            self.restore_tiled_drag(window, source);
            return;
        };
        let workspace = self.scroller.monitors[target_monitor].active_mut();
        workspace.windows.push(moved.clone());
        if !workspace.add_tab(moved.clone(), target_window) {
            workspace.windows.pop();
            self.scroller.monitors[source].workspaces[origin_workspace].add(moved, false);
            self.restore_tiled_drag(window, source);
            return;
        }
        self.scroller.focused_output = Some(self.scroller.monitors[target_monitor].name.clone());
        self.scroller.drag_preview = None;
        for (index, monitor) in self.scroller.monitors.iter_mut().enumerate() {
            if index != target_monitor {
                monitor.space.unmap_elem(window);
            }
        }
        self.relayout(true);
        self.sync_focus();
    }

    fn restore_tiled_drag(&mut self, window: &Window, source: usize) {
        self.scroller.drag_preview = None;
        for (index, monitor) in self.scroller.monitors.iter_mut().enumerate() {
            if index != source {
                monitor.space.unmap_elem(window);
            }
        }
        self.relayout(true);
        self.sync_focus();
    }

    fn drag_column_edge(
        &mut self,
        window: &Window,
        edge: ColumnEdge,
        initial_width: f64,
        initial_offset: i32,
        delta: f64,
    ) {
        let Some(monitor) = self
            .scroller
            .monitors
            .iter_mut()
            .find(|monitor| monitor.active().windows.contains(window))
        else {
            return;
        };
        let Some(area) = monitor.work_area() else {
            return;
        };
        let requested_width = monitor
            .active()
            .columns
            .iter()
            .map(|column| {
                let fraction = column
                    .width
                    .unwrap_or(self.config.layout.scrolling.column_width);
                (f64::from(area.size.w) * fraction).round() as i32
            })
            .min()
            .unwrap_or(area.size.w);
        let gap = effective_gap(
            workspace_gap(monitor.active(), &self.config),
            requested_width,
            area.size,
        );
        let (width, offset) =
            resized_column(initial_width, initial_offset, edge, delta, area.size.w, gap);
        let workspace = monitor.active_mut();
        let Some(column) = workspace
            .columns
            .iter_mut()
            .find(|column| column.windows.contains(window))
        else {
            return;
        };
        column.width = Some(width);
        workspace.offset = offset;
        self.relayout_with_animation(false, false);
    }

    fn drag_row_boundary(
        &mut self,
        window: &Window,
        boundary: usize,
        initial_heights: &[f64],
        delta: f64,
    ) {
        let Some(column) = self.scroller.monitors.iter_mut().find_map(|monitor| {
            monitor
                .active_mut()
                .columns
                .iter_mut()
                .find(|column| column.windows.contains(window))
        }) else {
            return;
        };
        if boundary + 1 >= column.windows.len() || initial_heights.len() != column.windows.len() {
            return;
        }
        let Some(weights) = resized_row_weights(initial_heights, boundary, delta, MIN_ROW_HEIGHT)
        else {
            return;
        };
        column.row_weights = weights;
        self.relayout_with_animation(false, false);
    }

    fn drag_layout_split(
        &mut self,
        window: &Window,
        target: LayoutResizeTarget,
        initial_ratio: f64,
        delta: f64,
        length: i32,
    ) {
        let Some(workspace) = self.scroller.monitors.iter_mut().find_map(|monitor| {
            monitor
                .active()
                .windows
                .contains(window)
                .then(|| monitor.active_mut())
        }) else {
            return;
        };
        let ratio = adjusted_split_ratio(initial_ratio, delta / f64::from(length.max(1)));
        match target {
            LayoutResizeTarget::Master if workspace.layout == LayoutMode::MasterStack => {
                workspace.master_ratio = Some(ratio);
            }
            LayoutResizeTarget::Dwindle(split) if workspace.layout == LayoutMode::Dwindle => {
                let tiled = workspace.tiled_slots().len();
                workspace.sync_layout_sizes(tiled);
                let Some(target) = workspace.dwindle_ratios.get_mut(split) else {
                    return;
                };
                *target = Some(ratio);
            }
            _ => return,
        }
        self.relayout_with_animation(false, false);
    }

    fn drag_master_row_boundary(
        &mut self,
        window: &Window,
        start: usize,
        boundary: usize,
        initial_heights: &[f64],
        delta: f64,
    ) {
        let Some(workspace) = self.scroller.monitors.iter_mut().find_map(|monitor| {
            let workspace = monitor.active_mut();
            (workspace.layout == LayoutMode::MasterStack && workspace.windows.contains(window))
                .then_some(workspace)
        }) else {
            return;
        };
        let Some(weights) = resized_row_weights(initial_heights, boundary, delta, MIN_ROW_HEIGHT)
        else {
            return;
        };
        workspace.sync_layout_sizes(workspace.tiled_slots().len());
        let Some(target) = workspace
            .master_weights
            .get_mut(start..start + weights.len())
        else {
            return;
        };
        target.copy_from_slice(&weights);
        self.relayout_with_animation(false, false);
    }
}
