//! The workspace overview and the scratchpad overview.
//!
//! Both are transient view states on [`Scrollers`] rather than modes the rest
//! of the compositor knows about: while one is open the layout underneath is
//! untouched, and closing it either confirms a selection or does nothing.

use super::*;

impl Paw2 {
    pub(crate) fn overview_active(&self) -> bool {
        self.scroller.overview.is_some()
    }

    pub(crate) fn scratchpad_overview_active(&self) -> bool {
        self.scroller.scratchpad_overview.is_some()
    }

    pub(crate) fn scratchpad_overview_visible_on(&self, output: &Output) -> bool {
        self.scratchpad_overview_active()
            && self.scroller.focused_output.as_deref() == Some(output.name().as_str())
    }

    pub fn toggle_scratchpad_overview(&mut self) {
        if self.scroller.scratchpad_overview.is_some() {
            self.close_scratchpad_overview();
            return;
        }
        let names = self
            .config
            .scratchpads
            .iter()
            .filter(|config| {
                self.scroller
                    .scratchpads
                    .get(&config.name)
                    .and_then(|runtime| runtime.window.as_ref())
                    .is_some()
            })
            .map(|config| config.name.clone())
            .chain((0..NUMBERED_SCRATCHPAD_COUNT).filter_map(|index| {
                let name = numbered_scratchpad_name(index);
                self.scroller
                    .scratchpads
                    .get(&name)
                    .and_then(|runtime| runtime.window.as_ref())
                    .map(|_| name)
            }))
            .collect::<Vec<_>>();
        if names.is_empty() {
            return;
        }
        if self.overview_active() {
            self.close_overview();
        }
        self.scroller.scratchpad_overview =
            Some(ScratchpadOverviewSelection { names, selected: 0 });
        self.sync_focus();
        crate::drm::redraw(self);
    }

    pub(crate) fn move_scratchpad_overview(&mut self, delta: i32) {
        let Some(selection) = &mut self.scroller.scratchpad_overview else {
            return;
        };
        selection.selected = wrapping_index(selection.selected, selection.names.len(), delta);
        crate::drm::redraw(self);
    }

    pub(crate) fn confirm_scratchpad_overview(&mut self) {
        let Some(name) = self
            .scroller
            .scratchpad_overview
            .as_ref()
            .and_then(|selection| selection.names.get(selection.selected))
            .cloned()
        else {
            return;
        };
        let visible = self
            .scroller
            .scratchpads
            .get(&name)
            .is_some_and(|runtime| runtime.visible && !runtime.hiding);
        let window = self
            .scroller
            .scratchpads
            .get(&name)
            .and_then(|runtime| runtime.window.clone());
        self.close_scratchpad_overview();
        if visible {
            if let Some(window) = window {
                self.focus_window(&window);
            }
        } else {
            self.show_scratchpad(&name);
        }
    }

    pub(crate) fn close_scratchpad_overview(&mut self) {
        if self.scroller.scratchpad_overview.take().is_some() {
            self.sync_focus();
            crate::drm::redraw(self);
        }
    }

    pub(crate) fn scratchpad_overview_hit(&self, location: Point<f64, Logical>) -> Option<usize> {
        let output = self.default_output()?;
        let origin = self.output_geometry(&output)?.loc.to_f64();
        self.scratchpad_strip(&output)?
            .previews
            .iter()
            .find_map(|preview| {
                preview
                    .card
                    .to_f64()
                    .contains(location - origin)
                    .then_some(preview.index)
            })
    }

    pub(crate) fn select_scratchpad_overview(&mut self, index: usize) {
        if let Some(selection) = &mut self.scroller.scratchpad_overview
            && index < selection.names.len()
        {
            selection.selected = index;
            self.confirm_scratchpad_overview();
        }
    }

    /// True while the overview owns input. The closing animation outlives this,
    /// so anything that only draws should ask `overview_visible` instead.
    pub(crate) fn overview_visible(&self) -> bool {
        self.scroller.overview.is_some() || self.overview_progress() > 0.0
    }

    /// How far open the overview is drawn right now, 0.0 to 1.0.
    pub(super) fn overview_progress(&self) -> f64 {
        self.scroller.overview_zoom.value(self.start_time.elapsed())
    }

    /// Points both overview tweens at whatever the state now says. Every path
    /// that opens, closes or moves the selection ends here, so the strip always
    /// slides instead of jumping. The scroll only animates within one monitor -
    /// hopping to another output has nothing to slide along.
    pub(super) fn retarget_overview(&mut self, animate_scroll: bool) {
        let now = self.start_time.elapsed();
        let duration = self.animation_duration();
        let open = self.scroller.overview.is_some();
        let target = self
            .scroller
            .overview
            .as_ref()
            .map(|selection| (selection.output.clone(), Some(selection.workspace)))
            .or_else(|| Some((self.scroller.focused_output.clone()?, None)));
        let center = target
            .and_then(|(output, workspace)| {
                let monitor = self.scroller.monitor(&output)?;
                let workspace = workspace.unwrap_or(monitor.active_workspace);
                let visible = visible_workspaces(&monitor.workspaces, monitor.active_workspace);
                visible.iter().position(|candidate| *candidate == workspace)
            })
            .map_or(self.scroller.overview_scroll.to, |position| position as f64);
        let pan = self.overview_pan_for_selection();
        if let Some(selection) = self.scroller.overview.as_mut() {
            selection.pan = pan;
        }
        self.scroller
            .overview_zoom
            .retarget(if open { 1.0 } else { 0.0 }, now, duration);
        let scroll_duration = animate_scroll.then_some(duration).flatten();
        self.scroller
            .overview_scroll
            .retarget(center, now, scroll_duration);
        self.scroller
            .overview_pan
            .retarget(f64::from(pan), now, scroll_duration);
        crate::drm::redraw(self);
    }

    /// The preview offset the current selection asks for. Resolved separately
    /// from `retarget_overview` because it needs the work area, and that means
    /// reading the output geometry and layer map before anything takes a `&mut`.
    fn overview_pan_for_selection(&self) -> i32 {
        let Some(selection) = self.scroller.overview.as_ref() else {
            return self.scroller.overview_pan.to as i32;
        };
        let Some(monitor) = self.scroller.monitor(&selection.output) else {
            return selection.pan;
        };
        let Some(workspace) = monitor.workspaces.get(selection.workspace) else {
            return selection.pan;
        };
        if workspace.layout != LayoutMode::Scrolling || workspace.fullscreen.is_some() {
            return 0;
        }
        let Some(area) = monitor.work_area() else {
            return selection.pan;
        };
        overview_pan_target(
            &workspace.columns,
            area.size,
            selection
                .window
                .and_then(|window| workspace.column_of(window)),
            selection.pan,
            self.config.layout.scrolling.column_width,
            workspace_gap(workspace, &self.config),
            lone_full_width(&self.config),
            self.config.layout.scrolling.center_focused_column,
        )
    }

    pub(super) fn select_active_workspace(&mut self) {
        let Some(monitor) = self
            .scroller
            .focused_output
            .as_deref()
            .and_then(|output| self.scroller.monitor(output))
        else {
            return;
        };
        self.scroller.overview = Some(OverviewSelection::new(monitor, monitor.active_workspace));
        self.retarget_overview(true);
    }

    pub(crate) fn close_overview(&mut self) {
        if self.scroller.overview.take().is_some() {
            self.sync_focus();
            self.retarget_overview(true);
        }
    }

    pub(super) fn toggle_overview(&mut self) {
        if self.overview_active() {
            self.close_overview();
            return;
        }
        if self.has_exclusive_keyboard_layer() {
            return;
        }
        self.close_scratchpad_overview();
        let monitor = self
            .scroller
            .focused_output
            .as_deref()
            .and_then(|output| self.scroller.monitor(output))
            .or_else(|| self.scroller.monitors.first());
        let Some(monitor) = monitor else {
            return;
        };
        self.scroller.overview = Some(OverviewSelection::new(monitor, monitor.active_workspace));
        self.sync_focus();
        self.retarget_overview(false);
    }

    pub(crate) fn move_overview_workspace(&mut self, delta: i32) {
        let Some(selection) = self.scroller.overview.clone() else {
            return;
        };
        let Some(monitor) = self.scroller.monitor(&selection.output) else {
            return;
        };
        let visible = visible_workspaces(&monitor.workspaces, monitor.active_workspace);
        let Some(workspace) = wrapped_selection(&visible, selection.workspace, delta) else {
            return;
        };
        self.scroller.overview = Some(OverviewSelection::new(monitor, workspace));
        self.retarget_overview(true);
    }

    pub(crate) fn move_overview_column(&mut self, delta: i32) {
        let Some(selection) = self.scroller.overview.clone() else {
            return;
        };
        let Some(window) = selection.window else {
            return;
        };
        let Some(target) = self
            .scroller
            .monitor(&selection.output)
            .and_then(|monitor| monitor.workspaces.get(selection.workspace))
            .and_then(|workspace| workspace.overview_column_neighbor(window, delta))
        else {
            return;
        };
        self.scroller.overview.as_mut().unwrap().window = Some(target);
        self.retarget_overview(true);
    }

    pub(crate) fn move_overview_row(&mut self, delta: i32) {
        let Some(selection) = self.scroller.overview.clone() else {
            return;
        };
        let Some(window) = selection.window else {
            return;
        };
        let Some(target) = self
            .scroller
            .monitor(&selection.output)
            .and_then(|monitor| monitor.workspaces.get(selection.workspace))
            .and_then(|workspace| workspace.overview_row_neighbor(window, delta))
        else {
            return;
        };
        self.scroller.overview.as_mut().unwrap().window = Some(target);
        self.retarget_overview(true);
    }

    pub(crate) fn move_overview_monitor(&mut self, delta: i32) {
        let Some(selection) = self.scroller.overview.clone() else {
            return;
        };
        let direction = if delta < 0 {
            Direction::Left
        } else {
            Direction::Right
        };
        let geometries = self.output_geometries();
        let Some(output) = directional_neighbor(&selection.output, direction, &geometries) else {
            return;
        };
        let Some(monitor) = self.scroller.monitor(&output) else {
            return;
        };
        self.scroller.overview = Some(OverviewSelection::new(monitor, monitor.active_workspace));
        self.retarget_overview(false);
    }

    pub(crate) fn confirm_overview(&mut self) {
        let Some(selection) = self.scroller.overview.take() else {
            return;
        };
        self.focus_output_by_name(&selection.output);
        self.activate_workspace(&selection.output, selection.workspace);
        if let Some(window) = selection.window {
            self.focus_window_by_id(window);
        }
        self.sync_focus();
        self.retarget_overview(true);
    }

    /// What sits under the pointer in the overview: the workspace, and the
    /// window inside it when the click landed on one.
    pub(crate) fn overview_hit(
        &self,
        position: Point<f64, Logical>,
    ) -> Option<(String, usize, Option<usize>)> {
        let monitor = self.monitor_at(position)?;
        let local = position - monitor.space.output_geometry(&monitor.output)?.loc.to_f64();
        let workspaces = self.overview_workspaces(&monitor.output);
        // Windows first: they sit on top of their card, and a click on one is
        // meant for that window rather than the workspace behind it.
        for workspace in &workspaces {
            for preview in &workspace.windows {
                if overview_screen_rect(preview.geometry, workspace.card, workspace.zoom)
                    .to_f64()
                    .contains(local)
                {
                    return Some((
                        monitor.name.clone(),
                        workspace.workspace,
                        Some(preview.window.id()),
                    ));
                }
            }
        }
        workspaces
            .iter()
            .find(|workspace| workspace.card.to_f64().contains(local))
            .map(|workspace| (monitor.name.clone(), workspace.workspace, None))
    }

    /// Wheel over the strip moves the selection, the same as `j`/`k` would. It
    /// deliberately does not switch workspace for real: that would relayout the
    /// desktop underneath a view whose whole job is to preview it.
    pub(crate) fn scroll_overview_at(&mut self, position: Point<f64, Logical>, delta: i32) {
        let Some(monitor) = self.monitor_at(position) else {
            return;
        };
        let name = monitor.name.clone();
        let visible = visible_workspaces(&monitor.workspaces, monitor.active_workspace);
        let current = self
            .scroller
            .overview
            .as_ref()
            .filter(|selection| selection.output == name)
            .map_or(monitor.active_workspace, |selection| selection.workspace);
        let Some(target) = wrapped_selection(&visible, current, delta) else {
            return;
        };
        let Some(monitor) = self.scroller.monitor(&name) else {
            return;
        };
        self.scroller.overview = Some(OverviewSelection::new(monitor, target));
        self.retarget_overview(true);
    }
}
