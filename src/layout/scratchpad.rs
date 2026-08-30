//! Scratchpads: windows kept off the layout and slid in on demand.
//!
//! Covers both the named scratchpads a config declares and the nine numbered
//! ones a user assigns at runtime, their remembered geometry, and the show,
//! hide and toggle paths.

use super::*;

impl Paw2 {
    pub fn initialize_scratchpads(&mut self) {
        let path = scratchpad_state_path();
        self.scroller.scratchpad_geometry = path
            .as_ref()
            .and_then(|path| fs::read(path).ok())
            .and_then(|bytes| match serde_json::from_slice(&bytes) {
                Ok(state) => Some(state),
                Err(error) => {
                    tracing::warn!(?error, ?path, "could not read scratchpad state");
                    None
                }
            })
            .unwrap_or_default();
        self.scroller.scratchpad_state_path = path;
    }

    pub fn reconcile_scratchpads(&mut self) {
        let removed = self
            .scroller
            .scratchpads
            .keys()
            .filter(|name| {
                numbered_scratchpad_index(name).is_none()
                    && !self
                        .config
                        .scratchpads
                        .iter()
                        .any(|config| config.name == **name)
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut windows = Vec::new();
        for name in removed {
            if let Some(runtime) = self.scroller.scratchpads.remove(&name)
                && let Some(window) = runtime.window
            {
                for monitor in &mut self.scroller.monitors {
                    monitor.space.unmap_elem(&window);
                }
                windows.push(window);
            }
            self.scroller.scratchpad_order.retain(|item| item != &name);
            if self.scroller.focused_scratchpad.as_deref() == Some(&name) {
                self.scroller.focused_scratchpad = None;
            }
        }
        if windows.is_empty() {
            return;
        }
        if let Some(monitor) = self.scroller.focused_mut() {
            for window in windows {
                monitor.active_mut().add(window, true);
            }
        } else {
            for window in windows {
                self.scroller.parked[0].add(window, true);
            }
        }
        self.relayout(true);
        self.sync_focus();
    }

    fn save_scratchpad_geometry(&self) {
        let Some(path) = &self.scroller.scratchpad_state_path else {
            return;
        };
        let Some(parent) = path.parent() else {
            return;
        };
        let result = (|| -> std::io::Result<()> {
            fs::create_dir_all(parent)?;
            let temporary = path.with_extension("json.tmp");
            let bytes = serde_json::to_vec_pretty(&self.scroller.scratchpad_geometry)
                .map_err(std::io::Error::other)?;
            fs::write(&temporary, bytes)?;
            fs::rename(temporary, path)
        })();
        if let Err(error) = result {
            tracing::warn!(?error, ?path, "could not save scratchpad geometry");
        }
    }

    pub(super) fn scratchpad_rectangle(
        &self,
        name: &str,
        output: &str,
    ) -> Option<Rectangle<i32, Logical>> {
        let config = self
            .config
            .scratchpads
            .iter()
            .find(|scratchpad| scratchpad.name == name);
        if config.is_none() && numbered_scratchpad_index(name).is_none() {
            return None;
        }
        let monitor = self.scroller.monitor(output)?;
        let area = monitor.work_area()?;
        let saved = self
            .scroller
            .scratchpad_geometry
            .get(name)
            .and_then(|outputs| outputs.get(output));
        let rectangle = saved.map_or_else(
            || {
                config.map_or_else(
                    || {
                        scratchpad_rectangle_from_options(
                            [75.0, 60.0],
                            None,
                            ScratchpadEdge::Top,
                            24,
                            area,
                        )
                    },
                    |config| configured_scratchpad_rectangle(config, area),
                )
            },
            |saved| {
                let size: Size<i32, Logical> = (
                    saved.size[0].max(1).min(area.size.w.max(1)),
                    saved.size[1].max(1).min(area.size.h.max(1)),
                )
                    .into();
                let location = area.loc + Point::from((saved.location[0], saved.location[1]));
                Rectangle::new(constrain_window(area, size, location), size)
            },
        );
        Some(rectangle)
    }

    fn scratchpad_edge(&self, name: &str) -> Option<ScratchpadEdge> {
        self.config
            .scratchpads
            .iter()
            .find(|scratchpad| scratchpad.name == name)
            .map(|scratchpad| scratchpad.edge)
            .or_else(|| numbered_scratchpad_index(name).map(|_| ScratchpadEdge::Top))
    }

    pub(super) fn remember_scratchpad_geometry(&mut self, name: &str) {
        let Some(runtime) = self.scroller.scratchpads.get(name) else {
            return;
        };
        let (Some(window), Some(output)) = (&runtime.window, &runtime.output) else {
            return;
        };
        let Some(monitor) = self.scroller.monitor(output) else {
            return;
        };
        let Some(area) = monitor.work_area() else {
            return;
        };
        let Some(geometry) = monitor.space.element_geometry(window) else {
            return;
        };
        self.scroller
            .scratchpad_geometry
            .entry(name.into())
            .or_default()
            .insert(
                output.clone(),
                SavedScratchpadGeometry {
                    location: [geometry.loc.x - area.loc.x, geometry.loc.y - area.loc.y],
                    size: [geometry.size.w, geometry.size.h],
                },
            );
        self.save_scratchpad_geometry();
    }

    pub(super) fn scratchpad_name_for_window(&self, window: &Window) -> Option<String> {
        self.scroller
            .scratchpads
            .iter()
            .find_map(|(name, runtime)| {
                (runtime.window.as_ref() == Some(window)).then(|| name.clone())
            })
    }

    fn matching_scratchpad(&self, window: &Window) -> Option<String> {
        let (_, app_id) = crate::screencopy::window_metadata(window);
        self.config
            .scratchpads
            .iter()
            .find(|scratchpad| scratchpad.app_id == app_id)
            .map(|scratchpad| scratchpad.name.clone())
    }

    pub(super) fn claim_scratchpad(&mut self, window: &Window) -> bool {
        let Some(name) = self.matching_scratchpad(window) else {
            return false;
        };
        if self
            .scroller
            .scratchpads
            .get(&name)
            .and_then(|runtime| runtime.window.as_ref())
            .is_some()
        {
            return false;
        }
        let output = self.scroller.focused_output.clone().or_else(|| {
            self.scroller
                .monitors
                .first()
                .map(|monitor| monitor.name.clone())
        });
        let runtime = self.scroller.scratchpads.entry(name.clone()).or_default();
        runtime.window = Some(window.clone());
        runtime.launching = None;
        runtime.output = output.clone();
        let show = std::mem::take(&mut runtime.show_on_map);
        if let Some(output) = output
            && let Some(rectangle) = self.scratchpad_rectangle(&name, &output)
        {
            configure_window(window, rectangle, false);
        }
        if show {
            self.show_scratchpad(&name);
        } else {
            self.sync_focus();
        }
        true
    }

    pub fn refresh_scratchpad_membership(&mut self, window: &Window) {
        if self.scratchpad_name_for_window(window).is_some() {
            return;
        }
        let Some(name) = self.matching_scratchpad(window) else {
            return;
        };
        if self
            .scroller
            .scratchpads
            .get(&name)
            .and_then(|runtime| runtime.window.as_ref())
            .is_some()
        {
            return;
        }
        for monitor in &mut self.scroller.monitors {
            if monitor
                .remove_where(|candidate| candidate == window)
                .is_some()
            {
                break;
            }
        }
        for workspace in &mut self.scroller.parked {
            workspace.remove_where(|candidate| candidate == window);
        }
        self.claim_scratchpad(window);
        self.relayout(true);
    }

    pub(super) fn toggle_scratchpad(&mut self, index: usize) {
        let Some(config) = self.config.scratchpads.get(index).cloned() else {
            return;
        };
        let loaded = self
            .scroller
            .scratchpads
            .get(&config.name)
            .and_then(|runtime| runtime.window.as_ref())
            .is_some();
        if loaded {
            let visible = self
                .scroller
                .scratchpads
                .get(&config.name)
                .is_some_and(|runtime| runtime.visible && !runtime.hiding);
            if visible {
                self.hide_scratchpad(&config.name);
            } else {
                self.show_scratchpad(&config.name);
            }
            return;
        }

        let now = self.start_time.elapsed();
        let runtime = self
            .scroller
            .scratchpads
            .entry(config.name.clone())
            .or_default();
        runtime.show_on_map = true;
        if runtime
            .launching
            .is_some_and(|started| now.saturating_sub(started) < Duration::from_secs(5))
        {
            return;
        }
        runtime.launching = Some(now);
        if let Err(error) = crate::spawn(
            &config.command,
            &self.cursor_defaults.configured(
                self.config.theme.cursor_theme.as_deref(),
                self.config.theme.cursor_size,
            ),
        ) {
            runtime.launching = None;
            tracing::error!(?error, scratchpad = %config.name, "failed to launch scratchpad");
        }
    }

    pub(super) fn toggle_numbered_scratchpad(&mut self, index: usize) {
        if index >= NUMBERED_SCRATCHPAD_COUNT {
            return;
        }
        let name = numbered_scratchpad_name(index);
        let Some(runtime) = self.scroller.scratchpads.get(&name) else {
            return;
        };
        if runtime.visible && !runtime.hiding {
            self.hide_scratchpad(&name);
        } else {
            self.show_scratchpad(&name);
        }
    }

    pub(super) fn assign_numbered_scratchpad(&mut self, index: usize) {
        if index >= NUMBERED_SCRATCHPAD_COUNT {
            return;
        }
        let name = numbered_scratchpad_name(index);
        if self.scroller.focused_scratchpad.as_deref() == Some(&name) {
            self.release_numbered_scratchpad(&name);
            return;
        }
        if self
            .scroller
            .scratchpads
            .get(&name)
            .and_then(|runtime| runtime.window.as_ref())
            .is_some()
        {
            return;
        }
        let Some(output) = self.scroller.focused_output.clone() else {
            return;
        };
        let Some(window) = self
            .scroller
            .monitor(&output)
            .and_then(|monitor| {
                monitor
                    .active()
                    .focused
                    .and_then(|focused| monitor.active().windows.get(focused))
            })
            .cloned()
        else {
            return;
        };
        let fullscreen = self
            .scroller
            .monitor(&output)
            .is_some_and(|monitor| monitor.active().fullscreen.as_ref() == Some(&window));
        let Some(window) = self
            .scroller
            .monitor_mut(&output)
            .and_then(|monitor| monitor.remove_where(|candidate| candidate == &window))
        else {
            return;
        };
        if fullscreen {
            self.set_window_fullscreen(&window, None);
        }
        self.scroller.scratchpads.insert(
            name.clone(),
            ScratchpadRuntime {
                window: Some(window),
                output: Some(output),
                ..Default::default()
            },
        );
        self.scroller.scratchpad_order.retain(|item| item != &name);
        self.scroller.scratchpad_order.push(name);
        self.relayout(true);
        self.sync_focus();
        self.ipc.touch();
    }

    fn release_numbered_scratchpad(&mut self, name: &str) {
        let Some(runtime) = self.scroller.scratchpads.remove(name) else {
            return;
        };
        let Some(window) = runtime.window else {
            return;
        };
        self.scroller.animations.remove(&window.id());
        for monitor in &mut self.scroller.monitors {
            monitor.space.unmap_elem(&window);
        }
        self.scroller.scratchpad_order.retain(|item| item != name);
        self.scroller.focused_scratchpad = None;
        if let Some(monitor) = self.scroller.focused_mut() {
            monitor.active_mut().add(window, true);
        } else {
            self.scroller.parked[0].add(window, true);
        }
        self.relayout(true);
        self.sync_focus();
        self.ipc.touch();
    }

    pub fn show_scratchpad_by_name(&mut self, name: &str) {
        let Some(index) = self
            .config
            .scratchpads
            .iter()
            .position(|scratchpad| scratchpad.name == name)
        else {
            return;
        };
        if self
            .scroller
            .scratchpads
            .get(name)
            .and_then(|runtime| runtime.window.as_ref())
            .is_some()
        {
            self.show_scratchpad(name);
        } else {
            self.toggle_scratchpad(index);
        }
    }

    pub fn toggle_scratchpad_by_name(&mut self, name: &str) {
        if let Some(index) = self
            .config
            .scratchpads
            .iter()
            .position(|scratchpad| scratchpad.name == name)
        {
            self.toggle_scratchpad(index);
        }
    }

    pub fn scratchpad_ipc_state(&self) -> Vec<crate::ipc::ScratchpadState> {
        let mut scratchpads = self
            .config
            .scratchpads
            .iter()
            .map(|config| {
                let runtime = self.scroller.scratchpads.get(&config.name);
                crate::ipc::ScratchpadState {
                    name: config.name.clone(),
                    app_id: config.app_id.clone(),
                    loaded: runtime
                        .and_then(|runtime| runtime.window.as_ref())
                        .is_some(),
                    visible: runtime.is_some_and(|runtime| runtime.visible),
                    output: runtime.and_then(|runtime| runtime.output.clone()),
                }
            })
            .collect::<Vec<_>>();
        scratchpads.extend((0..NUMBERED_SCRATCHPAD_COUNT).filter_map(|index| {
            let name = numbered_scratchpad_name(index);
            let runtime = self.scroller.scratchpads.get(&name)?;
            let window = runtime.window.as_ref()?;
            let (_, app_id) = crate::screencopy::window_metadata(window);
            Some(crate::ipc::ScratchpadState {
                name: (index + 1).to_string(),
                app_id,
                loaded: true,
                visible: runtime.visible,
                output: runtime.output.clone(),
            })
        }));
        scratchpads
    }

    pub(super) fn show_scratchpad(&mut self, name: &str) {
        let Some(window) = self
            .scroller
            .scratchpads
            .get(name)
            .and_then(|runtime| runtime.window.clone())
        else {
            return;
        };
        let Some(output) = self.scroller.focused_output.clone() else {
            return;
        };
        let Some(rectangle) = self.scratchpad_rectangle(name, &output) else {
            return;
        };
        let Some(edge) = self.scratchpad_edge(name) else {
            return;
        };
        let previous_focus = self.scroller.focused_mut().and_then(|monitor| {
            monitor
                .active()
                .focused
                .and_then(|index| monitor.active().windows.get(index))
                .map(|window| window.id())
        });
        for monitor in &mut self.scroller.monitors {
            if monitor.name != output {
                monitor.space.unmap_elem(&window);
            }
        }
        let Some(monitor_index) = self.scroller.monitor_index(&output) else {
            return;
        };
        let current = {
            let monitor = &self.scroller.monitors[monitor_index];
            let Some(area) = monitor.work_area() else {
                return;
            };
            monitor
                .space
                .element_location(&window)
                .unwrap_or_else(|| hidden_scratchpad_location(edge, area, rectangle))
        };
        configure_window(&window, rectangle, false);
        let duration = self.animation_duration();
        let position = start_motion(
            &mut self.scroller.animations,
            window.id(),
            Some(current),
            rectangle.loc,
            self.start_time.elapsed(),
            duration,
        );
        let monitor = &mut self.scroller.monitors[monitor_index];
        monitor.space.map_element(window.clone(), position, true);
        monitor.space.raise_element(&window, false);

        let runtime = self.scroller.scratchpads.get_mut(name).unwrap();
        runtime.visible = true;
        runtime.hiding = false;
        runtime.output = Some(output);
        runtime.previous_focus = previous_focus;
        self.scroller.focused_scratchpad = Some(name.into());
        self.scroller.scratchpad_order.retain(|item| item != name);
        self.scroller.scratchpad_order.push(name.into());
        self.sync_focus();
        self.ipc.touch();
        crate::drm::redraw(self);
    }

    pub fn hide_scratchpad(&mut self, name: &str) {
        let Some(runtime) = self.scroller.scratchpads.get(name) else {
            return;
        };
        let (Some(window), Some(output)) = (runtime.window.clone(), runtime.output.clone()) else {
            return;
        };
        let previous_focus = runtime.previous_focus;
        let Some(rectangle) = self.scratchpad_rectangle(name, &output) else {
            return;
        };
        let Some(edge) = self.scratchpad_edge(name) else {
            return;
        };
        let Some(monitor_index) = self.scroller.monitor_index(&output) else {
            return;
        };
        let (area, current) = {
            let monitor = &self.scroller.monitors[monitor_index];
            let Some(area) = monitor.work_area() else {
                return;
            };
            (
                area,
                monitor
                    .space
                    .element_location(&window)
                    .unwrap_or(rectangle.loc),
            )
        };
        let hidden = hidden_scratchpad_location(edge, area, rectangle);
        let duration = self.animation_duration();
        let position = start_motion(
            &mut self.scroller.animations,
            window.id(),
            Some(current),
            hidden,
            self.start_time.elapsed(),
            duration,
        );
        let monitor = &mut self.scroller.monitors[monitor_index];
        monitor.space.map_element(window.clone(), position, true);
        if duration.is_none() {
            monitor.space.unmap_elem(&window);
        }
        let runtime = self.scroller.scratchpads.get_mut(name).unwrap();
        runtime.hiding = duration.is_some();
        runtime.visible = duration.is_some();
        if self.scroller.focused_scratchpad.as_deref() == Some(name) {
            self.scroller.focused_scratchpad = None;
            if let Some(id) = previous_focus {
                self.focus_window_by_id(id);
            } else {
                self.sync_focus();
            }
        }
        self.ipc.touch();
        crate::drm::redraw(self);
    }

    pub fn reset_scratchpad(&mut self, name: &str) {
        self.scroller.scratchpad_geometry.remove(name);
        self.save_scratchpad_geometry();
        if self
            .scroller
            .scratchpads
            .get(name)
            .is_some_and(|runtime| runtime.visible)
        {
            self.show_scratchpad(name);
        }
    }
}
