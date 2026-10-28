use std::{collections::HashMap, env, fs, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};

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
        compositor::{
            CompositorHandler, SurfaceData, TraversalAction, with_states,
            with_surface_tree_downward,
        },
        fifo::FifoBarrierCachedState,
        fractional_scale::with_fractional_scale,
        keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitorSeat,
        seat::WaylandFocus,
        shell::wlr_layer::{KeyboardInteractivity, Layer},
        shell::xdg::{
            SurfaceCachedState, ToplevelSurface, XdgToplevelSurfaceData, dialog::ToplevelDialogHint,
        },
    },
    xwayland::xwm::WmWindowType,
};

mod drag;
mod minimize;
mod overview;
mod scratchpad;

use crate::{
    config::{
        Action, Config, DwindleConfig, DwindleSplit, LayoutConfig, LayoutMode, MasterPosition,
        MasterStackConfig, NUMBERED_SCRATCHPAD_COUNT, NUMBERED_SCRATCHPAD_PREFIX, ScratchpadConfig,
        ScratchpadEdge, ScratchpadOverviewPosition,
    },
    handlers::KeyboardFocus,
    state::Paw2,
};

fn refresh_fractional_scale(surface: &WlSurface, states: &SurfaceData) {
    if let Some(output) = surface_primary_scanout_output(surface, states) {
        with_fractional_scale(states, |fractional_scale| {
            fractional_scale.set_preferred_scale(output.current_scale().fractional_scale());
        });
    }
}

#[derive(Clone, Copy)]
enum Direction {
    Left,
    Right,
    Up,
    Down,
}

pub const WORKSPACE_COUNT: usize = 9;
/// How often a surface that is on no screen still gets a frame callback. Just
/// under a second, so a 1 Hz client timer is never pushed into the next tick.
const HIDDEN_SURFACE_FRAME_INTERVAL: Duration = Duration::from_millis(995);
const MIN_ROW_HEIGHT: i32 = 48;
pub(crate) const TAB_BAR_HEIGHT: i32 = 24;
/// Screen-space distance between workspace cards once the overview is fully
/// open. It scales with the opening progress, so at rest the active card lands
/// exactly on the output rectangle and the transition has no seam.
const OVERVIEW_GAP: i32 = 24;

#[derive(Clone)]
struct OverviewSelection {
    output: String,
    workspace: usize,
    window: Option<usize>,
    /// Where the preview of this workspace is scrolled to, in workspace-logical
    /// pixels. It starts at the workspace's own offset and then follows the
    /// selection, so a column that scrolled off the card can still be reached.
    pan: i32,
}

#[derive(Clone)]
struct ScratchpadOverviewSelection {
    names: Vec<String>,
    selected: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SavedScratchpadGeometry {
    location: [i32; 2],
    size: [i32; 2],
}

#[derive(Default)]
struct ScratchpadRuntime {
    window: Option<Window>,
    visible: bool,
    hiding: bool,
    show_on_map: bool,
    output: Option<String>,
    previous_focus: Option<usize>,
    launching: Option<Duration>,
}

pub(crate) struct ScratchpadPreview {
    pub window: Window,
    pub card: Rectangle<i32, Logical>,
    pub selected: bool,
    /// Position in the overview's name list. Cards scrolled out of the panel
    /// are never built, so this is not the position in `previews`.
    pub index: usize,
}

/// The scratchpad overview: a bordered panel and the cards currently inside it.
pub(crate) struct ScratchpadStrip {
    pub panel: Rectangle<i32, Logical>,
    pub previews: Vec<ScratchpadPreview>,
}

/// A window as the overview shows it. `geometry` is where the window really
/// sits relative to the output, *unscaled* - the renderer applies the card's
/// zoom to the surface itself, so the preview is the live window rather than a
/// redrawn stand-in.
pub(crate) struct OverviewWindow {
    pub window: Window,
    pub geometry: Rectangle<i32, Logical>,
    pub selected: bool,
}

/// One workspace card in the strip. `card` is where it lands on the output and
/// `zoom` is the factor its contents shrink by; together they are the whole
/// transform, which is why nothing here is pre-scaled.
pub(crate) struct OverviewWorkspace {
    pub workspace: usize,
    pub card: Rectangle<i32, Logical>,
    pub zoom: f64,
    pub windows: Vec<OverviewWindow>,
}

/// The bar above a window: its titlebar, or its tab group's bar, which is a
/// titlebar with a title per tab.
pub(crate) struct TitleBar {
    pub key: BarKey,
    pub root: Id,
    /// Output-local.
    pub geometry: Rectangle<i32, Logical>,
    pub titles: Vec<String>,
    /// The windows behind `titles`, in the same order.
    pub windows: Vec<Window>,
    pub active: usize,
    /// Drawn in the focus ring's colour, as the ring's top edge.
    pub focused: bool,
    /// The minimize, float/tile and close buttons, shown on titlebars but not
    /// on a bare tab bar.
    pub buttons: bool,
    /// The button under the pointer, if any.
    pub hovered: Option<BarPart>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum BarKey {
    TabGroup(usize),
    Window(usize),
}

/// Where on a bar a click lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BarPart {
    Title,
    Minimize,
    Float,
    Close,
}

/// A titlebar's buttons, left to right.
pub(crate) const BAR_BUTTONS: [BarPart; 3] = [BarPart::Minimize, BarPart::Float, BarPart::Close];

/// The button `offset` pixels into a titlebar's button area.
fn bar_button_at(offset: f64, height: i32) -> BarPart {
    let index = (offset / f64::from(height.max(1))) as usize;
    BAR_BUTTONS[index.min(BAR_BUTTONS.len() - 1)]
}

/// The buttons at a titlebar's right end are squares as tall as the bar.
pub(crate) fn bar_buttons_width(buttons: bool, height: i32) -> i32 {
    if buttons {
        height * BAR_BUTTONS.len() as i32
    } else {
        0
    }
}

#[derive(Clone, Copy)]
pub(crate) struct WindowRenderState {
    pub client: Rectangle<i32, Logical>,
    pub frame: Rectangle<i32, Logical>,
    pub opacity: f32,
    pub scratchpad: bool,
    /// Keyed by one of the window's subsurfaces rather than its root. These
    /// only share the root's clip; blur, shadow and rings belong to the root.
    pub subsurface: bool,
}

/// One column of the scrolling layout, and the row it remembers.
///
/// `focused` is **not** a duplicate of [`Workspace::focused`]. It is this
/// column's own memory of where the user last was in it, which is what lets
/// focus move out of a column and back into the same row - see
/// [`column_neighbor`], which reads it on a column that does *not* have focus.
/// Do not try to derive it from the workspace's focus; that loses the memory.
///
/// It is kept in range by [`Workspace::focus_window`] and
/// [`Workspace::remove_tile_from_column`], the only two places that can put it
/// out of date. Read it through [`Self::focused_window`] rather than indexing.
struct Column<T = Window> {
    windows: Vec<T>,
    row_weights: Vec<f64>,
    focused: usize,
    width: Option<f64>,
}

struct WindowGroup {
    windows: Vec<Window>,
    tiled: Vec<Window>,
    row_weights: Vec<f64>,
    focused: usize,
    width: Option<f64>,
    fullscreen: Option<Window>,
    floating: bool,
    tab_groups: Vec<TabGroup>,
}

#[derive(Clone)]
struct TabGroup<T = Window> {
    id: usize,
    windows: Vec<T>,
    active: usize,
}

#[derive(Clone, Copy)]
struct ManualFloating {
    floating: bool,
    width: Option<f64>,
    size: Option<Size<i32, Logical>>,
    location: Option<Point<i32, Logical>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColumnEdge {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FloatingEdge {
    Start,
    End,
}

pub(crate) enum MouseOperation {
    /// A press on a titlebar: a click until the pointer travels, then a move.
    PressTitle { window: Window },
    MoveScratchpad {
        name: String,
        initial_location: Point<i32, Logical>,
    },
    ResizeScratchpad {
        name: String,
        initial_geometry: Rectangle<i32, Logical>,
        x: Option<FloatingEdge>,
        y: Option<FloatingEdge>,
    },
    MoveTiled {
        window: Window,
        tab_group: bool,
        origin_output: String,
        origin_workspace: usize,
        origin_geometry: Rectangle<i32, Logical>,
        preview_size: Size<i32, Logical>,
        grab_offset: Point<f64, Logical>,
    },
    MoveFloating {
        window: Window,
        initial_location: Point<i32, Logical>,
    },
    ResizeFloating {
        window: Window,
        initial_geometry: Rectangle<i32, Logical>,
        x: Option<FloatingEdge>,
        y: Option<FloatingEdge>,
    },
    ResizeTiled {
        window: Window,
        x: Option<AxisResize>,
        y: Option<AxisResize>,
    },
}

pub(crate) enum AxisResize {
    Column {
        edge: ColumnEdge,
        initial_width: f64,
        initial_offset: i32,
    },
    Rows {
        master: bool,
        start: usize,
        boundary: usize,
        initial_heights: Vec<f64>,
    },
    Split {
        target: LayoutResizeTarget,
        initial_ratio: f64,
        length: i32,
        /// The pointer moving forward on its axis shrinks the first part.
        reversed: bool,
    },
}

#[derive(Clone, Copy)]
pub(crate) enum LayoutResizeTarget {
    Master,
    Dwindle(usize),
}

impl<T> Column<T> {
    fn new(window: T) -> Self {
        Self {
            windows: vec![window],
            row_weights: vec![1.0],
            focused: 0,
            width: None,
        }
    }

    /// The window in this column's remembered row.
    ///
    /// `None` only if `focused` has gone stale, which the two resync sites are
    /// there to prevent - but this is what every reader uses, so a bug there
    /// costs a missed focus rather than a panicking index.
    fn focused_window(&self) -> Option<&T> {
        self.windows.get(self.focused)
    }
}

/// Where a tiled window sits: an index into `Workspace.columns`, then into that
/// column's windows.
type Slot = (usize, usize);

struct Workspace {
    windows: Vec<Window>,
    /// Index into `windows`. Kept in step by `focus_after_removal` and by every
    /// mutation that touches `windows`; readers still go through `get` rather
    /// than indexing, because a desync here would panic the whole session
    /// rather than lose one window's focus.
    focused: Option<usize>,
    columns: Vec<Column>,
    tab_groups: Vec<TabGroup>,
    offset: i32,
    fullscreen: Option<Window>,
    layout: LayoutMode,
    /// Picked by a binding or the socket. Until then the workspace follows
    /// `layout.default`, so changing that applies to it straight away.
    layout_chosen: bool,
    /// The rest are this workspace's adjustments; `None` follows the config.
    master_count: Option<usize>,
    master_ratio: Option<f64>,
    master_weights: Vec<f64>,
    dwindle_ratios: Vec<Option<f64>>,
    /// Whether each dwindle split cut side by side, as of the last layout pass.
    /// Keyboard resizing looks a window's split up here: under
    /// `split = "longest"` the direction depends on the geometry.
    dwindle_vertical: Vec<bool>,
    /// The newest window and the one that had focus when it opened. Closing the
    /// newest window while it still has focus goes back there, so a window that
    /// flashes open and shut - `wl-copy` grabbing the clipboard, a launcher -
    /// leaves you where you were instead of on whatever sits next to it.
    opened_from: Option<(Window, Window)>,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            windows: Vec::new(),
            focused: None,
            columns: Vec::new(),
            tab_groups: Vec::new(),
            offset: 0,
            fullscreen: None,
            layout: LayoutMode::Scrolling,
            layout_chosen: false,
            master_count: None,
            master_ratio: None,
            master_weights: Vec::new(),
            dwindle_ratios: Vec::new(),
            dwindle_vertical: Vec::new(),
            opened_from: None,
        }
    }
}

impl Workspace {
    fn adopt_layout_state(&mut self, source: &Self) {
        self.layout = source.layout;
        self.layout_chosen = source.layout_chosen;
        self.master_count = source.master_count;
        self.master_ratio = source.master_ratio;
        self.master_weights.clone_from(&source.master_weights);
        self.dwindle_ratios.clone_from(&source.dwindle_ratios);
        self.dwindle_vertical.clone_from(&source.dwindle_vertical);
    }

    fn add(&mut self, window: Window, floating: bool) {
        self.opened_from = self
            .focused
            .and_then(|index| self.windows.get(index))
            .map(|previous| (window.clone(), previous.clone()));
        if !floating {
            let index = column_insertion_index(self.focused_column(), self.columns.len());
            self.columns.insert(index, Column::new(window.clone()));
        }
        self.windows.push(window);
        self.focused = Some(self.windows.len() - 1);
    }

    fn add_group(&mut self, group: WindowGroup) {
        let index = self.columns.len();
        self.add_group_at(group, index);
    }

    fn add_group_at(&mut self, group: WindowGroup, column_index: usize) {
        if group.floating {
            self.add(group.windows[0].clone(), true);
        } else {
            let focused = group.tiled.get(group.focused).cloned();
            self.windows.extend(group.windows.iter().cloned());
            self.columns.insert(
                column_index.min(self.columns.len()),
                Column {
                    windows: group.tiled,
                    row_weights: group.row_weights,
                    focused: group.focused,
                    width: group.width,
                },
            );
            self.tab_groups.extend(group.tab_groups);
            self.focused = focused.and_then(|window| self.window_index(&window));
        }
        if self.fullscreen.is_none() {
            self.fullscreen = group.fullscreen;
        }
    }

    fn window_index(&self, window: &Window) -> Option<usize> {
        self.windows.iter().position(|item| item == window)
    }

    fn column_of(&self, window: usize) -> Option<usize> {
        self.columns
            .iter()
            .position(|column| column.windows.iter().any(|tile| tile.id() == window))
    }

    fn focused_column(&self) -> Option<usize> {
        self.column_of(self.windows.get(self.focused?)?.id())
    }

    fn focus_window(&mut self, window: &Window) {
        if let Some(group_index) = self
            .tab_groups
            .iter()
            .position(|group| group.windows.contains(window))
        {
            let group = &self.tab_groups[group_index];
            let active = group.windows[group.active].clone();
            let next = group
                .windows
                .iter()
                .position(|item| item == window)
                .unwrap();
            if active != *window
                && let Some(tile) = self
                    .columns
                    .iter_mut()
                    .flat_map(|column| &mut column.windows)
                    .find(|tile| **tile == active)
            {
                *tile = window.clone();
                self.tab_groups[group_index].active = next;
            }
        }
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

    fn remove_from_column(&mut self, window: &Window) -> Option<Option<f64>> {
        if let Some((active, width)) = self.detach_tab(window) {
            return active.then_some(width);
        }
        self.remove_tile_from_column(window)
    }

    fn remove_tile_from_column(&mut self, window: &Window) -> Option<Option<f64>> {
        let column_index = self
            .columns
            .iter()
            .position(|column| column.windows.contains(window))?;
        let column = &mut self.columns[column_index];
        let width = column.width;
        let row = column
            .windows
            .iter()
            .position(|item| item == window)
            .unwrap();
        column.windows.remove(row);
        column.row_weights.remove(row);
        column.focused = focus_after_removal(Some(column.focused), row, column.windows.len())
            .unwrap_or_default();
        if column.windows.is_empty() {
            self.columns.remove(column_index);
        }
        Some(width)
    }

    /// Removes one member without disturbing the tile occupied by the rest of
    /// its group. The boolean says whether the removed member occupied that
    /// tile, which callers need when turning only the active tab floating.
    fn detach_tab(&mut self, window: &Window) -> Option<(bool, Option<f64>)> {
        let group_index = self
            .tab_groups
            .iter()
            .position(|group| group.windows.contains(window))?;
        let member = self.tab_groups[group_index]
            .windows
            .iter()
            .position(|item| item == window)?;
        let active_window =
            self.tab_groups[group_index].windows[self.tab_groups[group_index].active].clone();
        let active = member == self.tab_groups[group_index].active;
        let column_index = self
            .columns
            .iter()
            .position(|column| column.windows.contains(&active_window));
        let width = column_index.and_then(|index| self.columns[index].width);

        let group = &mut self.tab_groups[group_index];
        group.windows.remove(member);
        group.active = focus_after_removal(Some(group.active), member, group.windows.len())
            .unwrap_or_default();
        if active
            && let Some(column_index) = column_index
            && let Some(tile) = self.columns[column_index]
                .windows
                .iter_mut()
                .find(|tile| **tile == active_window)
            && let Some(next) = group.windows.get(group.active)
        {
            *tile = next.clone();
        }
        if group.windows.len() < 2 {
            self.tab_groups.remove(group_index);
        }
        Some((active, width))
    }

    fn tab_group_index(&self, window: &Window) -> Option<usize> {
        self.tab_groups
            .iter()
            .position(|group| group.windows.contains(window))
    }

    fn cycle_tab(&mut self, delta: i32) -> bool {
        let focused = self
            .focused
            .and_then(|index| self.windows.get(index))
            .cloned();
        let Some(group_index) = focused
            .as_ref()
            .and_then(|window| self.tab_group_index(window))
        else {
            return false;
        };
        let len = self.tab_groups[group_index].windows.len();
        let next = wrapping_index(self.tab_groups[group_index].active, len, delta);
        let window = self.tab_groups[group_index].windows[next].clone();
        self.focus_window(&window);
        true
    }

    fn group_or_expel_tab(&mut self, target: Option<Window>, delta: i32) -> bool {
        let Some(focused) = self
            .focused
            .and_then(|index| self.windows.get(index))
            .cloned()
        else {
            return false;
        };
        if self.tab_group_index(&focused).is_some() {
            let Some(column) = self
                .columns
                .iter()
                .position(|column| column.windows.contains(&focused))
            else {
                return false;
            };
            self.detach_tab(&focused);
            let index = if delta < 0 { column } else { column + 1 };
            self.columns
                .insert(index.min(self.columns.len()), Column::new(focused.clone()));
            self.focus_window(&focused);
            return true;
        }

        let Some(target) = target.filter(|target| *target != focused) else {
            return false;
        };
        if self.remove_tile_from_column(&focused).is_none() {
            return false;
        }
        self.add_tab(focused, target)
    }

    fn add_tab(&mut self, focused: Window, target: Window) -> bool {
        if let Some(group_index) = self.tab_group_index(&target) {
            let old =
                self.tab_groups[group_index].windows[self.tab_groups[group_index].active].clone();
            self.tab_groups[group_index].windows.push(focused.clone());
            self.tab_groups[group_index].active = self.tab_groups[group_index].windows.len() - 1;
            if let Some(tile) = self
                .columns
                .iter_mut()
                .flat_map(|column| &mut column.windows)
                .find(|tile| **tile == old)
            {
                *tile = focused.clone();
            }
        } else if let Some(tile) = self
            .columns
            .iter_mut()
            .flat_map(|column| &mut column.windows)
            .find(|tile| **tile == target)
        {
            *tile = focused.clone();
            self.tab_groups.push(TabGroup {
                id: target.id(),
                windows: vec![target, focused.clone()],
                active: 1,
            });
        } else {
            return false;
        }
        self.focus_window(&focused);
        true
    }

    fn dissolve_tab_groups(&mut self) -> bool {
        if self.tab_groups.is_empty() {
            return false;
        }
        let focused = self
            .focused
            .and_then(|index| self.windows.get(index))
            .cloned();
        for group in std::mem::take(&mut self.tab_groups) {
            let active = group.windows[group.active].clone();
            let Some(column) = self
                .columns
                .iter()
                .position(|column| column.windows.contains(&active))
            else {
                continue;
            };
            self.remove_tile_from_column(&active);
            for (offset, window) in group.windows.into_iter().enumerate() {
                self.columns.insert(column + offset, Column::new(window));
            }
        }
        if let Some(focused) = focused {
            self.focus_window(&focused);
        }
        true
    }

    fn remove_where(&mut self, predicate: impl Fn(&Window) -> bool) -> Option<Window> {
        let index = self.windows.iter().position(predicate)?;
        let window = self.windows.remove(index);
        if self.fullscreen.as_ref() == Some(&window) {
            self.fullscreen = None;
        }
        self.remove_from_column(&window);
        let was_focused = self.focused == Some(index);
        self.focused = focus_after_removal(self.focused, index, self.windows.len());
        match self.opened_from.take() {
            Some((newest, previous)) if newest == window => {
                if was_focused && let Some(previous) = self.window_index(&previous) {
                    self.focused = Some(previous);
                }
            }
            Some((_, previous)) if previous == window => {}
            other => self.opened_from = other,
        }
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
                tiled: vec![window.clone()],
                row_weights: vec![1.0],
                focused: 0,
                width: None,
                fullscreen: fullscreen.then_some(window),
                floating: true,
                tab_groups: Vec::new(),
            });
        };
        let column = self.columns.remove(column_index);
        let tab_groups = self
            .tab_groups
            .extract_if(.., |group| {
                column
                    .windows
                    .iter()
                    .any(|window| group.windows.contains(window))
            })
            .collect::<Vec<_>>();
        let mut windows = column.windows.clone();
        for group in &tab_groups {
            for window in &group.windows {
                if !windows.contains(window) {
                    windows.push(window.clone());
                }
            }
        }
        for window in &windows {
            let index = self.window_index(window).unwrap();
            self.windows.remove(index);
        }
        let fullscreen = self
            .fullscreen
            .take_if(|window| column.windows.contains(window));
        self.focused = self
            .columns
            .get(column_index.min(self.columns.len().saturating_sub(1)))
            .and_then(Column::focused_window)
            .and_then(|window| self.window_index(window))
            .or_else(|| (!self.windows.is_empty()).then_some(0));
        debug_assert!(column.windows.contains(&focused_window));
        Some(WindowGroup {
            windows,
            tiled: column.windows,
            row_weights: column.row_weights,
            focused: column.focused,
            width: column.width,
            fullscreen,
            floating: false,
            tab_groups,
        })
    }

    fn drain_groups(&mut self) -> Vec<WindowGroup> {
        let mut groups = Vec::new();
        while let Some(column) = self.columns.first() {
            let Some(window) = column.focused_window().cloned() else {
                break;
            };
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
        let Some(window) = column.focused_window().cloned() else {
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

    /// How many windows the tiled layout places.
    fn tiled_count(&self) -> usize {
        self.columns.iter().map(|column| column.windows.len()).sum()
    }

    fn tiled_slots(&self) -> Vec<Slot> {
        self.columns
            .iter()
            .enumerate()
            .flat_map(|(column, item)| (0..item.windows.len()).map(move |row| (column, row)))
            .collect()
    }

    /// The focused window, every tiled slot in order, and which slot the focus
    /// occupies. `None` when nothing is focused, or the focus is floating.
    fn focused_tiled_slot(&self) -> Option<(Window, Vec<Slot>, usize)> {
        let focused = self
            .focused
            .and_then(|index| self.windows.get(index))?
            .clone();
        let slots = self.tiled_slots();
        let current = slots
            .iter()
            .position(|&(column, row)| self.columns[column].windows[row] == focused)?;
        Some((focused, slots, current))
    }

    fn focused_tiled_index(&self) -> Option<usize> {
        self.focused_tiled_slot().map(|(_, _, index)| index)
    }

    fn sync_layout_sizes(&mut self, tiled: usize) {
        self.master_weights.resize(tiled, 1.0);
        self.master_weights.truncate(tiled);
        self.dwindle_ratios.resize(tiled.saturating_sub(1), None);
        self.dwindle_ratios.truncate(tiled.saturating_sub(1));
    }

    fn effective_master_count(&self, tiled: usize, config: &MasterStackConfig) -> usize {
        self.master_count
            .unwrap_or(usize::from(config.master_count))
            .clamp(1, tiled.max(1))
            .min(tiled)
    }

    fn master_ratio(&self, config: &MasterStackConfig) -> f64 {
        self.master_ratio.unwrap_or(config.master_ratio)
    }

    fn adjust_master_count(&mut self, delta: i32, config: &MasterStackConfig) -> bool {
        if self.layout != LayoutMode::MasterStack {
            return false;
        }
        let tiled = self.tiled_slots().len();
        if tiled == 0 {
            return false;
        }
        let current = self.effective_master_count(tiled, config);
        let adjusted = (current as i32 + delta).clamp(1, tiled as i32) as usize;
        if adjusted == current {
            return false;
        }
        self.master_count = Some(adjusted);
        true
    }

    fn focus_tiled(&mut self, delta: i32) -> bool {
        let Some((_, slots, current)) = self.focused_tiled_slot() else {
            return false;
        };
        let Some(next) = neighbor(current, slots.len(), delta) else {
            return false;
        };
        self.focus_tiled_at(next)
    }

    fn focus_tiled_at(&mut self, target: usize) -> bool {
        let Some((column, row)) = self.tiled_slots().get(target).copied() else {
            return false;
        };
        let window = self.columns[column].windows[row].clone();
        self.focus_window(&window);
        true
    }

    fn swap_tiled(&mut self, delta: i32) -> bool {
        let Some((_, slots, current)) = self.focused_tiled_slot() else {
            return false;
        };
        let Some(next) = neighbor(current, slots.len(), delta) else {
            return false;
        };
        self.swap_tiled_at(next)
    }

    fn swap_tiled_at(&mut self, target: usize) -> bool {
        let Some((focused, slots, current)) = self.focused_tiled_slot() else {
            return false;
        };
        let Some(&(to_column, to_row)) = slots.get(target) else {
            return false;
        };
        if current == target {
            return false;
        }
        let (from_column, from_row) = slots[current];
        let other = self.columns[to_column].windows[to_row].clone();
        self.columns[from_column].windows[from_row] = other;
        self.columns[to_column].windows[to_row] = focused.clone();
        self.focus_window(&focused);
        true
    }

    fn switch_layout(&mut self, layout: LayoutMode, enabled: &[LayoutMode]) -> bool {
        if self.layout == layout || !enabled.contains(&layout) {
            return false;
        }
        self.layout = layout;
        self.layout_chosen = true;
        self.offset = 0;
        true
    }

    fn cycle_layout(&mut self, enabled: &[LayoutMode]) -> bool {
        let current = enabled
            .iter()
            .position(|layout| *layout == self.layout)
            .unwrap_or_default();
        self.switch_layout(enabled[(current + 1) % enabled.len()], enabled)
    }

    fn focus_direction(&mut self, horizontal: bool, delta: i32) -> bool {
        match (self.layout, horizontal) {
            (LayoutMode::Scrolling, true) => self.focus_horizontal(delta),
            (LayoutMode::Scrolling, false) => self.focus_vertical(delta),
            _ => self.focus_tiled(delta),
        }
    }

    fn swap_direction(&mut self, horizontal: bool, delta: i32) -> bool {
        match (self.layout, horizontal) {
            (LayoutMode::Scrolling, true) => self.swap(delta),
            (LayoutMode::Scrolling, false) => self.swap_vertical(delta),
            _ => self.swap_tiled(delta),
        }
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
        let Some(column) = self.focused_column_mut() else {
            return false;
        };
        swap_column_window(column, delta)
    }

    /// A window the layout does not tile belongs to no column. That membership
    /// is the only record of floating state; nothing tracks it separately.
    fn is_floating(&self, window: &Window) -> bool {
        !self
            .columns
            .iter()
            .any(|column| column.windows.contains(window))
            && self.tab_group_index(window).is_none()
    }

    fn overview_window(&self) -> Option<usize> {
        self.fullscreen
            .as_ref()
            .or_else(|| self.focused.and_then(|index| self.windows.get(index)))
            .or_else(|| self.windows.first())
            .map(Window::id)
    }

    fn overview_column_neighbor(&self, current: usize, delta: i32) -> Option<usize> {
        if self.fullscreen.is_some() {
            return None;
        }
        let current = self.windows.iter().find(|window| window.id() == current)?;
        let floating = self
            .windows
            .iter()
            .filter(|window| self.is_floating(window))
            .cloned()
            .collect::<Vec<_>>();
        column_neighbor(&self.columns, &floating, current, delta).map(|window| window.id())
    }

    fn overview_row_neighbor(&self, current: usize, delta: i32) -> Option<usize> {
        if self.fullscreen.is_some() {
            return None;
        }
        let current = self.windows.iter().find(|window| window.id() == current)?;
        row_neighbor(&self.columns, current, delta).map(|window| window.id())
    }

    fn tile_at(&mut self, window: Window, index: usize, width: Option<f64>) {
        self.columns.insert(
            index.min(self.columns.len()),
            Column {
                windows: vec![window],
                row_weights: vec![1.0],
                focused: 0,
                width,
            },
        );
    }

    fn consume_or_expel(&mut self, delta: i32) -> bool {
        let Some(column_index) = self.focused_column() else {
            return false;
        };
        consume_or_expel_columns(&mut self.columns, column_index, delta)
    }

    fn cycle_column_width(&mut self, config: &LayoutConfig) -> bool {
        let presets = &config.scrolling.preset_column_widths;
        if self.layout == LayoutMode::MasterStack {
            let tiled = self.tiled_slots().len();
            if self.effective_master_count(tiled, &config.master_stack) == tiled
                || self.focused_tiled_index().is_none()
            {
                return false;
            }
            self.master_ratio = Some(adjusted_split_ratio(
                next_preset(self.master_ratio(&config.master_stack), presets),
                0.0,
            ));
            return true;
        }
        if self.layout != LayoutMode::Scrolling {
            return false;
        }
        let Some(column) = self.focused_column_mut() else {
            return false;
        };
        cycle_column_width(column, config.scrolling.column_width, presets);
        true
    }

    fn focused_column_mut(&mut self) -> Option<&mut Column> {
        let index = self.focused_column()?;
        self.columns.get_mut(index)
    }

    /// The dwindle split that owns the focused window on one axis, nudged by
    /// `delta`. Identical for width and height but for which axis the split is
    /// looked up on, which is what `vertical` picks.
    fn adjust_dwindle_split(
        &mut self,
        tiled: usize,
        vertical: bool,
        delta: f64,
        config: &DwindleConfig,
    ) -> bool {
        let Some(focused) = self.focused_tiled_index() else {
            return false;
        };
        let Some(split) = dwindle_split(focused, tiled, vertical, &self.dwindle_vertical) else {
            return false;
        };
        self.sync_layout_sizes(tiled);
        let delta = if focused == split { delta } else { -delta };
        let ratio = self.dwindle_ratios[split].get_or_insert(config.split_ratio);
        adjust_split_ratio(ratio, delta)
    }

    fn adjust_column_width(&mut self, config: &LayoutConfig, delta: f64) -> bool {
        let tiled = self.tiled_slots().len();
        let focused = self.focused_tiled_index();
        match self.layout {
            LayoutMode::MasterStack if tiled > 1 => {
                let Some(focused) = focused else {
                    return false;
                };
                let master = self.effective_master_count(tiled, &config.master_stack);
                if master == tiled {
                    return false;
                }
                let delta = if focused < master { delta } else { -delta };
                let current = self.master_ratio(&config.master_stack);
                let adjusted = adjusted_split_ratio(current, delta);
                if adjusted == current {
                    return false;
                }
                self.master_ratio = Some(adjusted);
                return true;
            }
            LayoutMode::Dwindle => {
                return self.adjust_dwindle_split(tiled, true, delta, &config.dwindle);
            }
            LayoutMode::Monocle => return false,
            LayoutMode::Scrolling => {}
            LayoutMode::MasterStack => return false,
        }
        let Some(column) = self.focused_column_mut() else {
            return false;
        };
        let current = column.width.unwrap_or(config.scrolling.column_width);
        let adjusted = adjusted_column_width(current, delta);
        if adjusted == current {
            return false;
        }
        column.width = Some(adjusted);
        true
    }

    fn adjust_row_height(&mut self, config: &LayoutConfig, delta: f64) -> bool {
        let tiled = self.tiled_slots().len();
        let focused = self.focused_tiled_index();
        match self.layout {
            LayoutMode::MasterStack => {
                let Some(focused) = focused else {
                    return false;
                };
                self.sync_layout_sizes(tiled);
                let master = self.effective_master_count(tiled, &config.master_stack);
                let range = if focused < master {
                    0..master
                } else {
                    master..tiled
                };
                return adjust_weights(
                    &mut self.master_weights[range.clone()],
                    focused - range.start,
                    delta,
                );
            }
            LayoutMode::Dwindle => {
                return self.adjust_dwindle_split(tiled, false, delta, &config.dwindle);
            }
            LayoutMode::Monocle => return false,
            LayoutMode::Scrolling => {}
        }
        let Some(column) = self.focused_column_mut() else {
            return false;
        };
        adjust_row_weights(column, delta)
    }
}

fn column_insertion_index(focused: Option<usize>, column_count: usize) -> usize {
    focused.map_or(column_count, |index| index + 1)
}

fn column_edge(
    pointer_x: f64,
    geometry: Rectangle<i32, Logical>,
    margin: i32,
) -> Option<(ColumnEdge, f64)> {
    let left = (pointer_x - f64::from(geometry.loc.x)).abs();
    let right = (pointer_x - f64::from(geometry.loc.x + geometry.size.w)).abs();
    if left.min(right) > f64::from(margin) {
        None
    } else if left <= right {
        Some((ColumnEdge::Left, left))
    } else {
        Some((ColumnEdge::Right, right))
    }
}

fn row_boundary(
    pointer_y: f64,
    geometry: Rectangle<i32, Logical>,
    row: usize,
    rows: usize,
    margin: i32,
) -> Option<(usize, f64)> {
    let top = (row > 0).then(|| (row - 1, (pointer_y - f64::from(geometry.loc.y)).abs()));
    let bottom = (row + 1 < rows).then(|| {
        (
            row,
            (pointer_y - f64::from(geometry.loc.y + geometry.size.h)).abs(),
        )
    });
    [top, bottom]
        .into_iter()
        .flatten()
        .filter(|(_, distance)| *distance <= f64::from(margin))
        .min_by(|left, right| left.1.total_cmp(&right.1))
}

fn adjusted_column_width(current: f64, delta: f64) -> f64 {
    (current + delta).clamp(0.1, 1.0)
}

fn adjusted_split_ratio(current: f64, delta: f64) -> f64 {
    (current + delta).clamp(0.1, 0.9)
}

fn adjust_split_ratio(ratio: &mut f64, delta: f64) -> bool {
    let adjusted = adjusted_split_ratio(*ratio, delta);
    if adjusted == *ratio {
        return false;
    }
    *ratio = adjusted;
    true
}

/// The split nearest above `focused` that cuts the way `vertical` asks, out of
/// `cuts` - each split's direction from the last layout pass.
fn dwindle_split(focused: usize, tiled: usize, vertical: bool, cuts: &[bool]) -> Option<usize> {
    let last = focused.min(tiled.checked_sub(2)?);
    (0..=last)
        .rev()
        .find(|split| cuts.get(*split).copied() == Some(vertical))
}

fn resized_column(
    initial_width: f64,
    initial_offset: i32,
    edge: ColumnEdge,
    delta: f64,
    output_width: i32,
    gap: i32,
) -> (f64, i32) {
    let direction = if edge == ColumnEdge::Left { -1.0 } else { 1.0 };
    let width = adjusted_column_width(
        initial_width,
        direction * delta / f64::from((output_width - gap).max(1)),
    );
    let offset = if edge == ColumnEdge::Left {
        initial_offset + column_width(output_width, width, gap)
            - column_width(output_width, initial_width, gap)
    } else {
        initial_offset
    };
    (width, offset)
}

fn resized_row_weights(
    initial_heights: &[f64],
    boundary: usize,
    delta: f64,
    requested_minimum: i32,
) -> Option<Vec<f64>> {
    if boundary + 1 >= initial_heights.len() {
        return None;
    }
    let available = initial_heights.iter().sum::<f64>();
    let minimum = f64::from(requested_minimum).min(available / initial_heights.len() as f64);
    let pair_height = initial_heights[boundary] + initial_heights[boundary + 1];
    let upper = (initial_heights[boundary] + delta).clamp(minimum, pair_height - minimum);
    let mut heights = initial_heights.to_vec();
    heights[boundary] = upper;
    heights[boundary + 1] = pair_height - upper;
    Some(
        heights
            .into_iter()
            .map(|height| (height - minimum).max(0.0))
            .collect(),
    )
}

fn adjust_row_weights<T>(column: &mut Column<T>, delta: f64) -> bool {
    if column.windows.len() != column.row_weights.len() {
        return false;
    }
    adjust_weights(&mut column.row_weights, column.focused, delta)
}

fn adjust_weights(weights: &mut [f64], focused: usize, delta: f64) -> bool {
    if weights.len() < 2 || focused >= weights.len() {
        return false;
    }
    let neighbor = if focused + 1 < weights.len() {
        focused + 1
    } else {
        focused - 1
    };
    let amount = weights.iter().sum::<f64>() * delta.abs();
    let (from, to) = if delta < 0.0 {
        (focused, neighbor)
    } else {
        (neighbor, focused)
    };
    let moved = weights[from].min(amount);
    if moved <= 0.0 {
        return false;
    }
    weights[from] -= moved;
    weights[to] += moved;
    true
}

struct Scroller {
    output: Output,
    /// `Output::name` takes a mutex and clones a `String` on every call, and
    /// this is the identity key for every monitor lookup - including
    /// `window_render_state`, which runs per render element per frame. The name
    /// is invariant for a monitor's lifetime, because `replace_output` is only
    /// reached after finding the monitor by this same name.
    name: String,
    space: Space<Window>,
    workspaces: [Workspace; WORKSPACE_COUNT],
    active_workspace: usize,
}

impl Scroller {
    fn new(output: Output, position: Point<i32, Logical>) -> Self {
        let mut space = Space::default();
        space.map_output(&output, position);
        Self {
            name: output.name(),
            output,
            space,
            workspaces: Default::default(),
            active_workspace: 0,
        }
    }

    fn replace_output(&mut self, output: Output, position: Point<i32, Logical>) {
        debug_assert_eq!(self.name, output.name());
        self.space.unmap_output(&self.output);
        self.space.map_output(&output, position);
        self.output = output;
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

    /// Where windows may go: this monitor's geometry minus whatever the layer
    /// surfaces on it reserved. `None` while the output has no geometry yet.
    fn work_area(&self) -> Option<Rectangle<i32, Logical>> {
        Some(work_area(
            self.space.output_geometry(&self.output)?,
            layer_map_for_output(&self.output).non_exclusive_zone(),
        ))
    }

    fn active(&self) -> &Workspace {
        &self.workspaces[self.active_workspace]
    }

    /// The display's advertised modes, newest-API-first order preserved, with
    /// the active one flagged. Refresh is millihertz in smithay and Hz in the
    /// config, so it is converted here rather than at every reader.
    fn modes(&self) -> Vec<crate::ipc::ModeState> {
        let current = self.output.current_mode();
        self.output
            .modes()
            .into_iter()
            .map(|mode| crate::ipc::ModeState {
                width: mode.size.w,
                height: mode.size.h,
                refresh: f64::from(mode.refresh) / 1000.0,
                current: Some(mode) == current,
            })
            .collect()
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

impl OverviewSelection {
    /// Seeds the pan from the workspace's real offset, so opening the overview
    /// has no horizontal jump. Every workspace and monitor change builds a fresh
    /// selection, which is what resets the pan back to reality.
    fn new(monitor: &Scroller, workspace: usize) -> Self {
        Self {
            output: monitor.name.clone(),
            workspace,
            window: monitor
                .workspaces
                .get(workspace)
                .and_then(Workspace::overview_window),
            pan: monitor
                .workspaces
                .get(workspace)
                .map_or(0, |workspace| workspace.offset),
        }
    }
}

impl Scrollers {
    /// Everything a panel draws: the monitors with their workspace strips, and
    /// every window with the workspace it sits on.
    ///
    /// Built here rather than in `ipc` because the workspace fields are private
    /// to this module, and one method beats the five accessors it would take to
    /// assemble this from outside.
    pub fn ipc_state(&self) -> (Vec<crate::ipc::OutputState>, Vec<crate::ipc::WindowState>) {
        let mut outputs = Vec::with_capacity(self.monitors.len());
        let mut windows = Vec::new();
        for monitor in &self.monitors {
            let focused_output = self.focused_output.as_deref() == Some(monitor.name.as_str());
            let active = &monitor.workspaces[monitor.active_workspace];
            outputs.push(crate::ipc::OutputState {
                name: monitor.name.clone(),
                focused: focused_output,
                active: monitor.active_workspace,
                occupied: monitor
                    .workspaces
                    .iter()
                    .map(|workspace| !workspace.windows.is_empty())
                    .collect(),
                layout: active.layout.name().into(),
                modes: monitor.modes(),
            });
            for (index, workspace) in monitor.workspaces.iter().enumerate() {
                for (position, window) in workspace.windows.iter().enumerate() {
                    let (title, app_id) = crate::screencopy::window_metadata(window);
                    windows.push(crate::ipc::WindowState {
                        id: window.id(),
                        // Filled in by the IPC flush, which owns the toplevel list.
                        identifier: String::new(),
                        title,
                        app_id,
                        output: monitor.name.clone(),
                        workspace: index,
                        // Only the focused monitor's active workspace can hold
                        // the window that actually has focus.
                        focused: focused_output
                            && index == monitor.active_workspace
                            && workspace.focused == Some(position),
                        minimized: false,
                    });
                }
            }
        }
        // Listed under the workspace they left, so a taskbar keeps them in place.
        for minimized in &self.minimized {
            let (title, app_id) = crate::screencopy::window_metadata(&minimized.window);
            windows.push(crate::ipc::WindowState {
                id: minimized.window.id(),
                identifier: String::new(),
                title,
                app_id,
                output: minimized.output.clone(),
                workspace: minimized.workspace,
                focused: false,
                minimized: true,
            });
        }
        (outputs, windows)
    }
}

#[derive(Default)]
pub struct Scrollers {
    monitors: Vec<Scroller>,
    focused_output: Option<String>,
    overview: Option<OverviewSelection>,
    /// How far the overview is open: 0.0 is the plain desktop, 1.0 fully zoomed
    /// out. It outlives `overview` so the closing animation still has something
    /// to draw after the selection is gone.
    overview_zoom: Tween,
    /// Which workspace the strip is centred on, as a position in the visible
    /// list, so switching workspace slides rather than jumps.
    overview_scroll: Tween,
    /// The selected workspace's preview offset, so moving along the columns
    /// slides the card rather than jumping it.
    overview_pan: Tween,
    scratchpad_overview: Option<ScratchpadOverviewSelection>,
    parked: [Workspace; WORKSPACE_COUNT],
    animations: HashMap<usize, WindowAnimation>,
    manual_floating: HashMap<usize, ManualFloating>,
    drag_preview: Option<usize>,
    /// The last press on a titlebar - window and time in ms - so a second
    /// one on the same window soon after counts as a double-click.
    last_title_press: Option<(usize, u32)>,
    /// The titlebar button under the pointer, by window id, so it can light up.
    hovered_bar_button: Option<(usize, BarPart)>,
    /// Minimized windows, oldest first. They stay out of every workspace until
    /// restored.
    minimized: Vec<minimize::MinimizedWindow>,
    scratchpads: HashMap<String, ScratchpadRuntime>,
    scratchpad_geometry: HashMap<String, HashMap<String, SavedScratchpadGeometry>>,
    scratchpad_state_path: Option<PathBuf>,
    scratchpad_order: Vec<String>,
    focused_scratchpad: Option<String>,
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

/// One number easing toward another. Like `WindowAnimation`, progress comes
/// from absolute elapsed time, so reading it more than once in a frame is free.
#[derive(Default)]
struct Tween {
    start: Duration,
    duration: Duration,
    from: f64,
    to: f64,
}

impl Tween {
    fn value(&self, now: Duration) -> f64 {
        if self.duration.is_zero() {
            return self.to;
        }
        let progress = (now.saturating_sub(self.start).as_secs_f64() / self.duration.as_secs_f64())
            .clamp(0.0, 1.0);
        self.from + (self.to - self.from) * ease_out_cubic(progress)
    }

    fn finished(&self, now: Duration) -> bool {
        self.duration.is_zero() || now.saturating_sub(self.start) >= self.duration
    }

    /// Re-aims at `to` from wherever the value stands right now, so closing the
    /// overview mid-open resumes from what is on screen instead of snapping to
    /// the far end and easing back.
    fn retarget(&mut self, to: f64, now: Duration, duration: Option<Duration>) {
        if self.to == to && !self.finished(now) {
            return;
        }
        *self = Self {
            from: self.value(now),
            to,
            start: now,
            duration: duration.unwrap_or_default(),
        };
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

fn scratchpad_state_path() -> Option<PathBuf> {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .map(|root| root.join("paw2/scratchpads.json"))
}

fn configured_scratchpad_rectangle(
    config: &ScratchpadConfig,
    area: Rectangle<i32, Logical>,
) -> Rectangle<i32, Logical> {
    scratchpad_rectangle_from_options(
        config.size,
        config.position,
        config.edge,
        config.margin,
        area,
    )
}

fn scratchpad_rectangle_from_options(
    percent: [f64; 2],
    position: Option<[f64; 2]>,
    edge: ScratchpadEdge,
    margin: i32,
    area: Rectangle<i32, Logical>,
) -> Rectangle<i32, Logical> {
    let size: Size<i32, Logical> = (
        ((f64::from(area.size.w) * percent[0] / 100.0)
            .round()
            .max(f64::from(MIN_ROW_HEIGHT)) as i32)
            .min(area.size.w.max(1)),
        ((f64::from(area.size.h) * percent[1] / 100.0)
            .round()
            .max(f64::from(MIN_ROW_HEIGHT)) as i32)
            .min(area.size.h.max(1)),
    )
        .into();
    let location = if let Some([x, y]) = position {
        (
            area.loc.x + (f64::from(area.size.w) * x / 100.0).round() as i32,
            area.loc.y + (f64::from(area.size.h) * y / 100.0).round() as i32,
        )
            .into()
    } else {
        match edge {
            ScratchpadEdge::Top => (area.loc.x + (area.size.w - size.w) / 2, area.loc.y + margin),
            ScratchpadEdge::Bottom => (
                area.loc.x + (area.size.w - size.w) / 2,
                area.loc.y + area.size.h - size.h - margin,
            ),
            ScratchpadEdge::Left => (area.loc.x + margin, area.loc.y + (area.size.h - size.h) / 2),
            ScratchpadEdge::Right => (
                area.loc.x + area.size.w - size.w - margin,
                area.loc.y + (area.size.h - size.h) / 2,
            ),
        }
        .into()
    };
    Rectangle::new(constrain_window(area, size, location), size)
}

fn numbered_scratchpad_name(index: usize) -> String {
    format!("{NUMBERED_SCRATCHPAD_PREFIX}{index}")
}

fn numbered_scratchpad_index(name: &str) -> Option<usize> {
    name.strip_prefix(NUMBERED_SCRATCHPAD_PREFIX)?
        .parse::<usize>()
        .ok()
        .filter(|index| *index < NUMBERED_SCRATCHPAD_COUNT)
}

fn hidden_scratchpad_location(
    edge: ScratchpadEdge,
    area: Rectangle<i32, Logical>,
    rectangle: Rectangle<i32, Logical>,
) -> Point<i32, Logical> {
    match edge {
        ScratchpadEdge::Top => (rectangle.loc.x, area.loc.y - rectangle.size.h),
        ScratchpadEdge::Bottom => (rectangle.loc.x, area.loc.y + area.size.h),
        ScratchpadEdge::Left => (area.loc.x - rectangle.size.w, rectangle.loc.y),
        ScratchpadEdge::Right => (area.loc.x + area.size.w, rectangle.loc.y),
    }
    .into()
}

impl Scrollers {
    /// Lookup by output identity rather than by name. `Output: PartialEq` is an
    /// `Arc::ptr_eq`, so this costs nothing next to `Output::name`, which locks
    /// a mutex and clones a `String` on every call.
    fn monitor_for(&self, output: &Output) -> Option<&Scroller> {
        self.monitors
            .iter()
            .find(|monitor| monitor.output == *output)
    }

    /// The index of the monitor called `output`, for the callers that need to
    /// let the borrow go and come back to it.
    fn monitor_index(&self, output: &str) -> Option<usize> {
        self.monitors
            .iter()
            .position(|monitor| monitor.name == output)
    }

    /// The index of the monitor whose active workspace holds `window`.
    fn monitor_index_holding(&self, window: &Window) -> Option<usize> {
        self.monitors
            .iter()
            .position(|monitor| monitor.active().windows.contains(window))
    }

    fn monitor(&self, output: &str) -> Option<&Scroller> {
        self.monitors.iter().find(|monitor| monitor.name == output)
    }

    fn monitor_mut(&mut self, output: &str) -> Option<&mut Scroller> {
        self.monitors
            .iter_mut()
            .find(|monitor| monitor.name == output)
    }

    fn set_focused_output(&mut self, output: &str) -> bool {
        if self.monitors.iter().any(|monitor| monitor.name == output) {
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

    /// Every workspace holding windows, mapped or parked.
    fn workspaces(&self) -> impl Iterator<Item = &Workspace> {
        self.monitors
            .iter()
            .flat_map(|monitor| &monitor.workspaces)
            .chain(&self.parked)
    }

    fn should_float(&self, window: &Window, automatic: bool) -> bool {
        self.manual_floating
            .get(&window.id())
            .map_or(automatic, |override_| override_.floating)
    }

    fn is_floating(&self, window: &Window) -> bool {
        self.workspaces()
            .find(|workspace| workspace.windows.contains(window))
            .is_some_and(|workspace| workspace.is_floating(window))
    }

    /// Moves the window into or out of the column layout. Reports whether that
    /// changed anything, so callers only relayout when it did.
    fn set_floating(&mut self, window: &Window, floating: bool) -> bool {
        let Some(workspace) = self
            .monitors
            .iter_mut()
            .flat_map(|monitor| &mut monitor.workspaces)
            .chain(&mut self.parked)
            .find(|workspace| workspace.windows.contains(window))
        else {
            return false;
        };
        if workspace.is_floating(window) == floating {
            return false;
        }
        if floating {
            workspace.remove_from_column(window);
        } else {
            workspace.columns.push(Column::new(window.clone()));
        }
        true
    }
}

impl Paw2 {
    pub fn apply_startup_monitor(&mut self) {
        let Some(name) = self.config.startup_monitor().map(str::to_owned) else {
            return;
        };
        let Some((_, geometry)) = self
            .output_geometries()
            .into_iter()
            .find(|(output, _)| output == &name)
        else {
            tracing::warn!(
                monitor = name,
                "startup monitor is unavailable; keeping fallback focus"
            );
            return;
        };
        self.scroller.set_focused_output(&name);
        self.seat
            .get_pointer()
            .unwrap()
            .set_location(geometry.loc.to_f64() + geometry.size.to_f64() / 2.0);
        self.sync_focus();
        tracing::info!(monitor = name, "focused startup monitor");
    }

    pub fn output_added(&mut self, output: &Output, position: Point<i32, Logical>) {
        let name = output.name();
        if let Some(monitor) = self.scroller.monitor_mut(&name) {
            monitor.replace_output(output.clone(), position);
            self.relayout(true);
            return;
        }
        let mut monitor = Scroller::new(output.clone(), position);
        for (index, parked) in self.scroller.parked.iter_mut().enumerate() {
            if monitor.workspaces[index].windows.is_empty() {
                monitor.workspaces[index].adopt_layout_state(parked);
            }
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
        let scratchpads = self
            .scroller
            .scratchpads
            .iter()
            .filter(|(_, runtime)| runtime.visible && runtime.output.as_deref() == Some(name))
            .map(|(scratchpad, _)| scratchpad.clone())
            .collect::<Vec<_>>();
        let Some(index) = self.scroller.monitor_index(name) else {
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
                if target.workspaces[index].windows.is_empty() {
                    target.workspaces[index].adopt_layout_state(workspace);
                }
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
                if self.scroller.parked[index].windows.is_empty() {
                    self.scroller.parked[index].adopt_layout_state(workspace);
                }
                for mut group in workspace.drain_groups() {
                    if let Some(window) = group.fullscreen.take() {
                        restore_tiled.push(window);
                    }
                    self.scroller.parked[index].add_group(group);
                }
            }
            if removed_was_focused {
                self.scroller.focused_output =
                    self.scroller.monitors.first().map(|item| item.name.clone());
            }
        }
        for window in restore_tiled {
            self.set_window_fullscreen(&window, None);
        }
        for (window, output) in keep_fullscreen {
            self.set_window_fullscreen(&window, Some(&output));
        }
        for scratchpad in scratchpads {
            self.show_scratchpad(&scratchpad);
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
        let Some(name) = self
            .monitor_at(position)
            .map(|monitor| monitor.name.clone())
        else {
            return;
        };
        self.focus_output_by_name(&name);
    }

    pub fn focus_output_by_name(&mut self, name: &str) {
        if self.scroller.set_focused_output(name) {
            self.sync_focus();
            // The shell's panels place themselves on the focused output, and
            // cannot follow a change nothing tells them about.
            self.ipc.touch();
        }
    }

    /// Focus a window by the id `ext-foreign-toplevel-list-v1` and the control
    /// socket both use. Reuses the toplevel table rather than walking every
    /// workspace, since that table exists precisely to map ids to windows.
    pub fn focus_window_by_id(&mut self, id: usize) {
        if self.restore_window(id) {
            return;
        }
        let Some(window) = self.toplevel_window(id) else {
            return;
        };
        self.focus_window(&window);
    }

    /// Ask a window to close, as `close_window` does for the focused one.
    pub fn close_window_by_id(&mut self, id: usize) {
        let Some(window) = self.toplevel_window(id) else {
            return;
        };
        if let Some(surface) = window.toplevel() {
            surface.send_close();
        } else if let Some(surface) = window.x11_surface() {
            let _ = surface.close();
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
            .map(|monitor| monitor.name.clone())
            .or_else(|| self.scroller.focused_output.clone());
        if let Some(monitor) = output.and_then(|name| self.scroller.monitor_mut(&name)) {
            monitor.space.map_element(window, location, true);
        }
    }

    fn add_managed_window(&mut self, window: Window, fullscreen: bool, floating: bool) {
        self.capture_window_added(&window);
        if self.claim_scratchpad(&window) {
            self.ipc.touch();
            return;
        }
        let pointer = self.pointer().current_location();
        self.focus_output_at(pointer);
        if let Some(output) = self.scroller.focused_output.clone()
            && let Some(monitor) = self.scroller.monitor_mut(&output)
        {
            monitor.active_mut().add(window.clone(), floating);
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

    /// Re-decide whether `window` floats, now that a hint behind that decision
    /// changed. `automatic` is what the surface's own hints ask for; a manual
    /// override still wins inside `should_float`.
    fn refresh_floating(&mut self, window: &Window, automatic: bool) {
        let floating = self.scroller.should_float(window, automatic);
        if self.scroller.set_floating(window, floating) {
            self.relayout(true);
            self.sync_focus();
        }
    }

    pub fn refresh_wayland_floating(&mut self, surface: &ToplevelSurface) {
        if let Some(window) = self.window_for_surface(surface.wl_surface()).cloned() {
            self.refresh_floating(&window, wayland_should_float(surface));
        }
    }

    pub fn refresh_x11_floating(&mut self, surface: &smithay::xwayland::X11Surface) {
        if let Some(window) = self.window_for_x11_surface(surface).cloned() {
            self.refresh_floating(&window, x11_should_float(surface));
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
        if let Some(window) = self.forget_minimized(&predicate) {
            self.forget_window(&window);
            return true;
        }
        let scratchpad = self
            .scroller
            .scratchpads
            .iter_mut()
            .find_map(|(name, runtime)| {
                runtime.window.as_ref().filter(|window| predicate(window))?;
                runtime.visible = false;
                runtime.hiding = false;
                Some((name.clone(), runtime.window.take().unwrap()))
            });
        if let Some((name, window)) = scratchpad {
            for monitor in &mut self.scroller.monitors {
                monitor.space.unmap_elem(&window);
            }
            self.scroller.scratchpad_order.retain(|item| item != &name);
            if self.scroller.focused_scratchpad.as_deref() == Some(&name) {
                self.scroller.focused_scratchpad = None;
            }
            self.forget_window(&window);
            return true;
        }
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
        self.capture_window_removed(window);
        self.scroller.animations.remove(&window.id());
        self.scroller.manual_floating.remove(&window.id());
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
        if let Some(name) = self.scratchpad_name_for_window(&window)
            && let Some(output) = self
                .scroller
                .scratchpads
                .get(&name)
                .and_then(|runtime| runtime.output.as_deref())
            && let Some(rectangle) = self.scratchpad_rectangle(&name, output)
        {
            configure_window(&window, rectangle, false);
            return;
        }
        if !self.scroller.is_floating(&window) {
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
            let Some(area) = monitor.work_area() else {
                return;
            };
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
        self.focus_window_with_behavior(window, true);
    }

    pub fn focus_window_on_hover(&mut self, window: &Window) {
        self.focus_window_with_behavior(window, false);
    }

    fn focus_window_with_behavior(&mut self, window: &Window, reveal_and_raise: bool) {
        if let Some(name) = self.scratchpad_name_for_window(window) {
            let Some(output) = self
                .scroller
                .scratchpads
                .get(&name)
                .filter(|runtime| runtime.visible)
                .and_then(|runtime| runtime.output.clone())
            else {
                return;
            };
            self.scroller.focused_output = Some(output.clone());
            self.scroller.focused_scratchpad = Some(name.clone());
            if reveal_and_raise {
                if let Some(monitor) = self.scroller.monitor_mut(&output) {
                    monitor.space.raise_element(window, false);
                }
                self.scroller.scratchpad_order.retain(|item| item != &name);
                self.scroller.scratchpad_order.push(name);
            }
            self.sync_focus();
            return;
        }
        self.scroller.focused_scratchpad = None;
        let Some(monitor_index) = self.scroller.monitor_index_holding(window) else {
            return;
        };
        let monitor = &self.scroller.monitors[monitor_index];
        let already_focused = self.scroller.focused_output.as_deref()
            == Some(monitor.name.as_str())
            && monitor
                .active()
                .focused
                .and_then(|index| monitor.active().windows.get(index))
                == Some(window);
        if already_focused && !reveal_and_raise {
            return;
        }
        self.scroller.monitors[monitor_index]
            .active_mut()
            .focus_window(window);
        self.scroller.focused_output = Some(self.scroller.monitors[monitor_index].name.clone());
        self.relayout(reveal_and_raise);
        if reveal_and_raise && self.scroller.is_floating(window) {
            self.scroller.monitors[monitor_index]
                .space
                .raise_element(window, false);
        }
        self.sync_focus();
    }

    pub fn set_fullscreen(&mut self, surface: &ToplevelSurface) {
        let Some(window) = self.window_for_surface(surface.wl_surface()).cloned() else {
            surface.send_configure();
            return;
        };
        self.fullscreen_managed_window(window);
    }

    pub fn set_x11_fullscreen(&mut self, surface: &smithay::xwayland::X11Surface) {
        if let Some(window) = self.window_for_x11_surface(surface).cloned() {
            self.fullscreen_managed_window(window);
        }
    }

    fn fullscreen_managed_window(&mut self, window: Window) {
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

        // The client's requested output is only a hint. Keep fullscreen windows
        // on the monitor where Paw2 originally placed them.
        let target_monitor = source_monitor;
        let target_workspace = self.scroller.monitors[target_monitor].active_workspace;

        if (source_monitor, source_workspace) != (target_monitor, target_workspace) {
            // Read before the move: leaving the column is what floating means.
            let floating = self.scroller.monitors[source_monitor].workspaces[source_workspace]
                .is_floating(&window);
            let source = &mut self.scroller.monitors[source_monitor];
            let workspace = &mut source.workspaces[source_workspace];
            let window = workspace.windows.remove(source_window);
            workspace.remove_from_column(&window);
            workspace.fullscreen = None;
            workspace.focused =
                focus_after_removal(workspace.focused, source_window, workspace.windows.len());
            source.space.unmap_elem(&window);

            let target = &mut self.scroller.monitors[target_monitor];
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

    /// Both branches relayout and refocus themselves, and the redraw arrives with
    /// the client's configure-commit, so `run_action` has nothing left to do.
    fn toggle_fullscreen(&mut self) {
        if self.scroller.focused_scratchpad.is_some() {
            return;
        }
        let Some(window) = self.scroller.focused_mut().and_then(|scroller| {
            let workspace = scroller.active();
            workspace
                .focused
                .and_then(|index| workspace.windows.get(index).cloned())
        }) else {
            return;
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
            self.fullscreen_managed_window(window);
        }
    }

    fn toggle_floating(&mut self) -> bool {
        if self.scroller.focused_scratchpad.is_some() {
            return false;
        }
        let pointer_x = self.pointer().current_location().x;
        let Some(output) = self.scroller.focused_output.clone() else {
            return false;
        };
        let Some(monitor_index) = self.scroller.monitor_index(&output) else {
            return false;
        };
        let Some(window) = self.scroller.monitors[monitor_index]
            .active()
            .focused
            .and_then(|index| {
                self.scroller.monitors[monitor_index]
                    .active()
                    .windows
                    .get(index)
            })
            .cloned()
        else {
            return false;
        };
        if self.scroller.monitors[monitor_index]
            .active()
            .fullscreen
            .is_some()
        {
            return false;
        }

        let floating = self.scroller.monitors[monitor_index]
            .active()
            .is_floating(&window);
        let remembered_width = self
            .scroller
            .manual_floating
            .get(&window.id())
            .and_then(|override_| override_.width);
        let monitor = &mut self.scroller.monitors[monitor_index];
        let Some(area) = monitor.work_area() else {
            return false;
        };
        let (width, size, location) = if floating {
            let index = column_insertion_at(monitor.active(), &monitor.space, pointer_x);
            monitor
                .active_mut()
                .tile_at(window.clone(), index, remembered_width);
            (remembered_width, None, None)
        } else {
            let width = monitor.active_mut().remove_from_column(&window).flatten();
            let size = (!window_is_fixed_size(&window)).then_some(default_floating_size(area.size));
            let location =
                centered_rectangle(area, size.unwrap_or_else(|| floating_size(&window))).loc;
            (width, size, Some(location))
        };
        self.scroller.manual_floating.insert(
            window.id(),
            ManualFloating {
                floating: !floating,
                width,
                size,
                location,
            },
        );
        true
    }

    pub fn run_action(&mut self, action: Action) {
        let changed = match action {
            Action::FocusLeft => self.directional_layout_action(true, -1, false),
            Action::FocusRight => self.directional_layout_action(true, 1, false),
            Action::FocusUp => self.directional_layout_action(false, -1, false),
            Action::FocusDown => self.directional_layout_action(false, 1, false),
            Action::SwapLeft => self.directional_layout_action(true, -1, true),
            Action::SwapRight => self.directional_layout_action(true, 1, true),
            Action::SwapUp => self.directional_layout_action(false, -1, true),
            Action::SwapDown => self.directional_layout_action(false, 1, true),
            Action::ConsumeOrExpelLeft => self.configured_layout_action(|workspace, config| {
                if workspace.layout == LayoutMode::MasterStack {
                    workspace.adjust_master_count(-1, &config.master_stack)
                } else {
                    workspace.layout == LayoutMode::Scrolling && workspace.consume_or_expel(-1)
                }
            }),
            Action::ConsumeOrExpelRight => self.configured_layout_action(|workspace, config| {
                if workspace.layout == LayoutMode::MasterStack {
                    workspace.adjust_master_count(1, &config.master_stack)
                } else {
                    workspace.layout == LayoutMode::Scrolling && workspace.consume_or_expel(1)
                }
            }),
            Action::CycleColumnWidth => {
                self.configured_layout_action(Workspace::cycle_column_width)
            }
            Action::CycleLayout => {
                let enabled = &self.config.layout.enabled_layouts;
                self.scroller
                    .active_workspace()
                    .is_some_and(|workspace| workspace.cycle_layout(enabled))
            }
            Action::SwitchLayout(layout) => {
                let enabled = &self.config.layout.enabled_layouts;
                self.scroller
                    .active_workspace()
                    .is_some_and(|workspace| workspace.switch_layout(layout, enabled))
            }
            Action::DecreaseColumnWidth => self.configured_layout_action(|workspace, config| {
                workspace.adjust_column_width(config, -0.05)
            }),
            Action::IncreaseColumnWidth => self.configured_layout_action(|workspace, config| {
                workspace.adjust_column_width(config, 0.05)
            }),
            Action::DecreaseRowHeight => self.configured_layout_action(|workspace, config| {
                workspace.adjust_row_height(config, -0.05)
            }),
            Action::IncreaseRowHeight => self.configured_layout_action(|workspace, config| {
                workspace.adjust_row_height(config, 0.05)
            }),
            Action::TabGroupLeft => self.tab_group_direction(-1),
            Action::TabGroupRight => self.tab_group_direction(1),
            Action::PreviousTab => self.layout_action(|workspace| workspace.cycle_tab(-1)),
            Action::NextTab => self.layout_action(|workspace| workspace.cycle_tab(1)),
            // Mouse-only: input starts the grab directly so this never reaches
            // the keyboard action dispatcher.
            Action::TabGroupDrag => false,
            Action::ToggleFloating => self.toggle_floating(),
            Action::MinimizeWindow => {
                let window = self.scroller.focused_mut().and_then(|scroller| {
                    let workspace = scroller.active();
                    workspace
                        .focused
                        .and_then(|index| workspace.windows.get(index).cloned())
                });
                if let Some(window) = window {
                    self.minimize_window(&window);
                }
                false
            }
            Action::RestoreMinimized => {
                self.restore_last_minimized();
                false
            }
            Action::CloseWindow => {
                let scratchpad = self
                    .scroller
                    .focused_scratchpad
                    .as_ref()
                    .and_then(|name| self.scroller.scratchpads.get(name))
                    .and_then(|runtime| runtime.window.clone());
                let window = scratchpad.or_else(|| {
                    self.scroller.focused_mut().and_then(|scroller| {
                        let workspace = scroller.active();
                        workspace
                            .focused
                            .and_then(|index| workspace.windows.get(index).cloned())
                    })
                });
                if let Some(window) = window {
                    if let Some(surface) = window.toplevel() {
                        surface.send_close();
                    } else if let Some(surface) = window.x11_surface() {
                        let _ = surface.close();
                    }
                }
                false
            }
            Action::ToggleFullscreen => {
                self.toggle_fullscreen();
                false
            }
            Action::ToggleOverview => {
                self.toggle_overview();
                false
            }
            Action::ToggleScratchpad(index) => {
                self.toggle_scratchpad(index);
                false
            }
            Action::ToggleNumberedScratchpad(index) => {
                self.toggle_numbered_scratchpad(index);
                false
            }
            Action::AssignNumberedScratchpad(index) => {
                self.assign_numbered_scratchpad(index);
                false
            }
            Action::ToggleScratchpadOverview => {
                self.toggle_scratchpad_overview();
                false
            }
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
                if let Some(command) = self.config.binding_commands.get(index)
                    && let Err(error) = crate::spawn(
                        command,
                        &self.cursor_defaults.configured(
                            self.config.theme.cursor_theme.as_deref(),
                            self.config.theme.cursor_size,
                        ),
                    )
                {
                    tracing::error!(?error, program = command[0], "failed to launch command");
                }
                false
            }
            Action::Quit => {
                self.begin_shutdown();
                false
            }
        };
        if self.overview_active()
            && matches!(
                action,
                Action::FocusMonitorLeft
                    | Action::FocusMonitorRight
                    | Action::FocusMonitorUp
                    | Action::FocusMonitorDown
                    | Action::MoveToMonitorLeft
                    | Action::MoveToMonitorRight
                    | Action::MoveToMonitorUp
                    | Action::MoveToMonitorDown
                    | Action::SwitchWorkspace(_)
                    | Action::PreviousWorkspace
                    | Action::NextWorkspace
            )
        {
            self.select_active_workspace();
        }
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

    /// [`Self::layout_action`] for actions whose defaults come from `[layout]`.
    fn configured_layout_action(
        &mut self,
        action: impl FnOnce(&mut Workspace, &LayoutConfig) -> bool,
    ) -> bool {
        let config = &self.config.layout;
        self.scroller
            .active_workspace()
            .is_some_and(|workspace| action(workspace, config))
    }

    fn tab_group_direction(&mut self, delta: i32) -> bool {
        let Some(workspace) = self.scroller.active_workspace() else {
            return false;
        };
        let Some(source) = workspace
            .focused
            .and_then(|index| workspace.windows.get(index))
            .cloned()
        else {
            return false;
        };
        if workspace.tab_group_index(&source).is_some() {
            return self.layout_action(|workspace| workspace.group_or_expel_tab(None, delta));
        }
        if !self.directional_layout_action(true, delta, false) {
            return false;
        }
        let target = self
            .scroller
            .active_workspace()
            .and_then(|workspace| workspace.focused)
            .and_then(|index| self.scroller.active_workspace()?.windows.get(index))
            .cloned();
        self.layout_action(|workspace| {
            workspace.focus_window(&source);
            workspace.group_or_expel_tab(target, delta)
        })
    }

    pub(crate) fn reconcile_tab_grouping(&mut self) {
        if self.config.layout.tab_grouping {
            return;
        }
        let mut changed = false;
        for workspace in self
            .scroller
            .monitors
            .iter_mut()
            .flat_map(|monitor| &mut monitor.workspaces)
        {
            changed |= workspace.dissolve_tab_groups();
        }
        if changed {
            self.sync_focus();
        }
    }

    fn directional_layout_action(&mut self, horizontal: bool, delta: i32, swap: bool) -> bool {
        let Some(output) = self.scroller.focused_output.clone() else {
            return false;
        };
        let Some(monitor_index) = self.scroller.monitor_index(&output) else {
            return false;
        };
        let layout = self.scroller.monitors[monitor_index].active().layout;
        if !matches!(layout, LayoutMode::MasterStack | LayoutMode::Dwindle) {
            return self.layout_action(|workspace| {
                if swap {
                    workspace.swap_direction(horizontal, delta)
                } else {
                    workspace.focus_direction(horizontal, delta)
                }
            });
        }
        let monitor = &self.scroller.monitors[monitor_index];
        let workspace = monitor.active();
        if layout == LayoutMode::MasterStack && workspace.fullscreen.is_some() {
            return false;
        }
        let tiled = workspace
            .columns
            .iter()
            .flat_map(|column| column.windows.iter())
            .collect::<Vec<_>>();
        let Some(focused) = workspace.focused_tiled_index() else {
            return false;
        };
        let geometries = tiled
            .iter()
            .map(|window| monitor.space.element_geometry(window))
            .collect::<Option<Vec<_>>>();
        let direction = match (horizontal, delta.signum()) {
            (true, -1) => Direction::Left,
            (true, 1) => Direction::Right,
            (false, -1) => Direction::Up,
            (false, 1) => Direction::Down,
            _ => return false,
        };
        let master_stack = &self.config.layout.master_stack;
        let Some(target) = geometries.and_then(|geometries| match layout {
            LayoutMode::MasterStack => {
                // Asked in the frame where the master area is on the left.
                let frame = MasterFrame::new(
                    monitor.work_area().unwrap_or_default(),
                    master_stack.position,
                );
                let geometries = geometries
                    .into_iter()
                    .map(|geometry| frame.to_frame(geometry))
                    .collect::<Vec<_>>();
                let frame_horizontal = horizontal != master_stack.position.transposed();
                let frame_delta = if frame_horizontal && master_stack.position.mirrored() {
                    -delta
                } else {
                    delta
                };
                master_neighbor(
                    focused,
                    workspace.effective_master_count(tiled.len(), master_stack),
                    frame_horizontal,
                    frame_delta,
                    &geometries,
                )
            }
            LayoutMode::Dwindle => directional_index(focused, direction, &geometries, true),
            _ => None,
        }) else {
            return false;
        };
        let workspace = self.scroller.monitors[monitor_index].active_mut();
        if swap {
            workspace.swap_tiled_at(target)
        } else {
            workspace.focus_tiled_at(target)
        }
    }

    fn change_monitor(&mut self, direction: Direction, move_window: bool) -> bool {
        self.try_change_monitor(direction, move_window).is_some()
    }

    fn change_workspace(&mut self, delta: i32, move_window: bool) -> bool {
        let target = {
            let Some(scroller) = self.scroller.focused_mut() else {
                return false;
            };
            let target = workspace_neighbor(scroller.active_workspace, delta);
            if !move_window {
                return scroller.switch_workspace(target);
            }
            target
        };
        self.move_focused_to_workspace(target)
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
            let window = scroller.active().windows.get(index)?;
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
        if self.overview_active() && workspace < WORKSPACE_COUNT {
            self.scroller.overview = self
                .scroller
                .monitor(output)
                .map(|monitor| OverviewSelection::new(monitor, workspace));
            self.retarget_overview(true);
        }
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
        self.ipc.touch();
        let active = self
            .scroller
            .monitors
            .iter()
            .map(|monitor| (monitor.name.clone(), monitor.active_workspace))
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
            target_scroller.active_mut().add_group(group);
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
            let pointer = self.pointer();
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

    pub(super) fn output_geometries(&self) -> Vec<(String, Rectangle<i32, Logical>)> {
        self.scroller
            .monitors
            .iter()
            .filter_map(|monitor| {
                monitor
                    .space
                    .output_geometry(&monitor.output)
                    .map(|geometry| (monitor.name.clone(), geometry))
            })
            .collect()
    }

    /// Geometry of the output the pointer should treat as current: the focused
    /// one, else the first. Split out so the absolute-motion path, which runs on
    /// every event, does not have to build the whole name/geometry vector.
    pub fn focused_output_geometry(&self) -> Option<Rectangle<i32, Logical>> {
        let focused = self.scroller.focused_output.as_deref();
        // Falls through to the first monitor that *has* geometry, not simply the
        // first monitor: a suspended output is unmapped from its space, and the
        // pointer still has to land somewhere.
        let mut mapped = self
            .scroller
            .monitors
            .iter()
            .filter_map(|monitor| {
                let geometry = monitor.space.output_geometry(&monitor.output)?;
                Some((monitor.name.as_str(), geometry))
            })
            .peekable();
        let first = mapped.peek().map(|(_, geometry)| *geometry);
        mapped
            .find(|(name, _)| Some(*name) == focused)
            .map(|(_, geometry)| geometry)
            .or(first)
    }

    /// Output rectangles without their names, for callers that only need geometry.
    /// `output_geometries` clones a `String` per output, and the pointer path runs
    /// on every motion event.
    pub fn output_rects(&self) -> impl Iterator<Item = Rectangle<i32, Logical>> + '_ {
        self.scroller
            .monitors
            .iter()
            .filter_map(|monitor| monitor.space.output_geometry(&monitor.output))
    }

    pub fn output_geometry(&self, output: &Output) -> Option<Rectangle<i32, Logical>> {
        let monitor = self.scroller.monitor_for(output)?;
        monitor.space.output_geometry(&monitor.output)
    }

    pub fn reposition_output(&mut self, output: &Output, position: Point<i32, Logical>) {
        let Some(monitor) = self
            .scroller
            .monitors
            .iter_mut()
            .find(|monitor| monitor.output == *output)
        else {
            return;
        };
        monitor.space.map_output(&monitor.output, position);
        monitor
            .output
            .change_current_state(None, None, None, Some(position));
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
            .monitor_for(output)
            .map(|monitor| &monitor.space)
    }

    /// The workspace strip this output draws: one card per visible workspace,
    /// each carrying its windows at their real, unscaled positions plus the zoom
    /// the renderer should apply to them.
    pub(crate) fn overview_workspaces(&self, output: &Output) -> Vec<OverviewWorkspace> {
        let Some(monitor) = self.scroller.monitor_for(output) else {
            return Vec::new();
        };
        let Some(output_geometry) = monitor.space.output_geometry(output) else {
            return Vec::new();
        };
        let visible = visible_workspaces(&monitor.workspaces, monitor.active_workspace);
        let selection = self
            .scroller
            .overview
            .as_ref()
            .filter(|selection| selection.output == monitor.name);
        // Only one strip scrolls: the one holding the selection, or the focused
        // one while the overview closes. Every other monitor sits on its own
        // active workspace, which is not moving.
        let scrolling = match &self.scroller.overview {
            Some(selection) => selection.output == monitor.name,
            None => self.scroller.focused_output.as_deref() == Some(monitor.name.as_str()),
        };
        let center = if scrolling {
            self.scroller
                .overview_scroll
                .value(self.start_time.elapsed())
        } else {
            visible
                .iter()
                .position(|workspace| *workspace == monitor.active_workspace)
                .unwrap_or_default() as f64
        };
        let progress = self.overview_progress();
        let zoom = overview_zoom(progress, self.config.overview.zoom);
        let pan = self
            .scroller
            .overview_pan
            .value(self.start_time.elapsed())
            .round() as i32;
        let area = work_area(
            output_geometry,
            layer_map_for_output(output).non_exclusive_zone(),
        );
        overview_cards(
            output_geometry.size,
            &visible,
            center,
            progress,
            self.config.overview.zoom,
        )
        .into_iter()
        .map(|(index, card)| {
            let workspace = &monitor.workspaces[index];
            let selected = selection.filter(|selection| selection.workspace == index);
            let selected_window = selected.and_then(|selection| selection.window);
            // Only the workspace being navigated pans; the rest are not moving,
            // so they keep showing their real scroll position.
            let offset = match selected {
                Some(_) => pan,
                None => workspace.offset,
            };
            let windows = overview_window_placements(
                workspace,
                area,
                output_geometry,
                offset,
                &self.config,
                &self.scroller.manual_floating,
            )
            .into_iter()
            .filter_map(|(window, mut geometry)| {
                // Placements are global; the renderer works output-local.
                geometry.loc -= output_geometry.loc;
                let selected = selected_window == Some(window.id());
                // The selection is never culled. Panning should always have
                // revealed it, and a selection you cannot see is the exact
                // failure this pan exists to prevent.
                (selected
                    || overview_screen_rect(geometry, card, zoom)
                        .overlaps(Rectangle::from_size(output_geometry.size)))
                .then_some(OverviewWindow {
                    selected,
                    window,
                    geometry,
                })
            })
            .collect();
            OverviewWorkspace {
                workspace: index,
                card,
                zoom,
                windows,
            }
        })
        .collect()
    }

    fn overview_inactive_windows(&self, output: &Output) -> Vec<Window> {
        if !self.overview_visible() {
            return Vec::new();
        }
        let Some(monitor) = self.scroller.monitor_for(output) else {
            return Vec::new();
        };
        monitor
            .workspaces
            .iter()
            .enumerate()
            .filter(|(index, workspace)| {
                *index != monitor.active_workspace && !workspace.windows.is_empty()
            })
            .flat_map(|(_, workspace)| workspace.windows.iter().cloned())
            .collect()
    }

    pub(crate) fn scratchpad_strip(&self, output: &Output) -> Option<ScratchpadStrip> {
        let selection = self.scroller.scratchpad_overview.as_ref()?;
        if self.scroller.focused_output.as_deref() != Some(output.name().as_str()) {
            return None;
        }
        // The strip lives in the space bars and docks leave behind, held off
        // that edge by the same gap a window keeps.
        let area = inset_rectangle(
            layer_map_for_output(output).non_exclusive_zone(),
            self.config.appearance.gaps,
        );
        let (panel, cards) = scratchpad_strip_layout(
            area,
            selection.names.len(),
            self.config.overview.scratchpad_position,
            selection.selected,
        );
        let previews = cards
            .into_iter()
            .filter_map(|(index, card)| {
                let name = selection.names.get(index)?;
                let window = self.scroller.scratchpads.get(name)?.window.clone()?;
                Some(ScratchpadPreview {
                    window,
                    card,
                    selected: index == selection.selected,
                    index,
                })
            })
            .collect();
        Some(ScratchpadStrip { panel, previews })
    }

    /// Whether `output`'s focused window is its workspace's only tiled window
    /// under `appearance.smart_gaps`, and so wears no focus ring.
    pub fn focused_window_is_lone(&self, output: &Output) -> bool {
        if !self.config.appearance.smart_gaps {
            return false;
        }
        let Some(monitor) = self.scroller.monitor_for(output) else {
            return false;
        };
        let workspace = monitor.active();
        // A visible scratchpad holds the focus over the workspace beneath it.
        let scratchpad_focused = self
            .scroller
            .focused_scratchpad
            .as_ref()
            .and_then(|name| self.scroller.scratchpads.get(name))
            .is_some_and(|runtime| runtime.visible);
        !scratchpad_focused
            && workspace.tiled_count() == 1
            && workspace
                .focused
                .and_then(|index| workspace.windows.get(index))
                .is_some_and(|window| !workspace.is_floating(window))
    }

    pub fn focused_window_render_state(
        &self,
        output: &Output,
    ) -> Option<(Rectangle<i32, Logical>, Id)> {
        let monitor = self.scroller.monitor_for(output)?;
        if self.scroller.focused_output.as_deref() != Some(monitor.name.as_str()) {
            return None;
        }
        let window = if let Some(window) = self
            .scroller
            .focused_scratchpad
            .as_ref()
            .and_then(|name| self.scroller.scratchpads.get(name))
            .filter(|runtime| runtime.visible && runtime.output.as_deref() == Some(&monitor.name))
            .and_then(|runtime| runtime.window.as_ref())
        {
            window
        } else {
            let workspace = monitor.active();
            workspace.windows.get(workspace.focused?)?
        };
        let client = monitor.space.element_geometry(window)?;
        let frame = if has_bar(monitor.active(), window, &self.config) {
            bar_outer_rectangle(client, bar_height(&self.config))
        } else {
            client
        };
        Some((frame, Id::from_wayland_resource(&window_surface(window)?)))
    }

    /// Every bar on `output`'s active workspace: one per tab group, and one per
    /// other window with a titlebar.
    pub(crate) fn title_bars(&self, output: &Output) -> Vec<TitleBar> {
        if self.overview_visible() || self.scratchpad_overview_visible_on(output) {
            return Vec::new();
        }
        let Some(monitor) = self.scroller.monitor_for(output) else {
            return Vec::new();
        };
        let Some(output_location) = monitor
            .space
            .output_geometry(output)
            .map(|geometry| geometry.loc)
        else {
            return Vec::new();
        };
        let workspace = monitor.active();
        let height = bar_height(&self.config);
        let buttons = self.config.appearance.titlebars;
        let focused = self.focused_window_render_state(output).map(|(_, id)| id);
        let hovered = self.scroller.hovered_bar_button;
        let title = |window: &Window| {
            let (title, app_id) = crate::screencopy::window_metadata(window);
            if title.is_empty() { app_id } else { title }
        };
        let bar = |key: BarKey, windows: Vec<Window>, active: usize| {
            let shown = windows.get(active)?;
            if !has_bar(workspace, shown, &self.config) {
                return None;
            }
            let root = Id::from_wayland_resource(&window_surface(shown)?);
            let mut geometry = bar_outer_rectangle(monitor.space.element_geometry(shown)?, height);
            geometry.size.h = height;
            geometry.loc -= output_location;
            let hovered = hovered
                .filter(|(id, _)| buttons && *id == shown.id())
                .map(|(_, part)| part);
            Some(TitleBar {
                key,
                focused: focused.as_ref() == Some(&root),
                root,
                geometry,
                titles: windows.iter().map(title).collect(),
                windows,
                active,
                buttons,
                hovered,
            })
        };

        let grouped = workspace.tab_groups.iter().filter_map(|group| {
            bar(
                BarKey::TabGroup(group.id),
                group.windows.clone(),
                group.active,
            )
        });
        let single = workspace
            .windows
            .iter()
            .filter(|window| workspace.tab_group_index(window).is_none())
            .filter_map(|window| bar(BarKey::Window(window.id()), vec![window.clone()], 0));
        grouped.chain(single).collect()
    }

    /// The window and part of the bar under `position`, if a bar is there and
    /// nothing covers it.
    pub(crate) fn title_bar_at(&self, position: Point<f64, Logical>) -> Option<(Window, BarPart)> {
        if self.session_locked() {
            return None;
        }
        let monitor = self.monitor_at(position)?;
        // A fullscreen output draws no bars. Checking first also keeps every
        // pointer motion over a fullscreen game from building them all.
        if self.renders_fullscreen(&monitor.output) {
            return None;
        }
        let output_location = monitor.space.output_geometry(&monitor.output)?.loc;
        let local = position - output_location.to_f64();
        let layers = layer_map_for_output(&monitor.output);
        if layers.layer_under(Layer::Overlay, local).is_some()
            || layers.layer_under(Layer::Top, local).is_some()
        {
            return None;
        }
        drop(layers);
        self.uncovered_title_bar_at(position)
    }

    /// Bar hit testing used while `surface_under` already holds Smithay's
    /// per-output layer-map lock. This must not acquire that lock itself.
    fn uncovered_title_bar_at(&self, position: Point<f64, Logical>) -> Option<(Window, BarPart)> {
        if !self.config.layout.tab_grouping && !self.config.appearance.titlebars {
            return None;
        }
        let monitor = self.monitor_at(position)?;
        let output_location = monitor.space.output_geometry(&monitor.output)?.loc;
        let covering = monitor
            .space
            .element_under(position)
            .map(|(window, _)| window);
        for bar in self.title_bars(&monitor.output) {
            let mut geometry = bar.geometry;
            geometry.loc += output_location;
            if !geometry.to_f64().contains(position) {
                continue;
            }
            if covering.is_some_and(|window| !bar.windows.contains(window)) {
                return None;
            }
            let relative = (position.x - f64::from(geometry.loc.x)).max(0.0);
            let titles_width = geometry.size.w - bar_buttons_width(bar.buttons, geometry.size.h);
            if relative >= f64::from(titles_width) {
                let active = bar.windows.get(bar.active)?.clone();
                let part = bar_button_at(relative - f64::from(titles_width), geometry.size.h);
                return Some((active, part));
            }
            let index = tab_index_at(relative, titles_width, bar.windows.len())?;
            return Some((bar.windows.get(index)?.clone(), BarPart::Title));
        }
        None
    }

    /// Light up the titlebar button under `position`. Nothing lights up while
    /// a drag holds the pointer.
    pub(crate) fn hover_bar_button(&mut self, position: Point<f64, Logical>, grabbed: bool) {
        self.scroller.hovered_bar_button = if grabbed {
            None
        } else {
            self.title_bar_at(position)
                .filter(|(_, part)| *part != BarPart::Title)
                .map(|(window, part)| (window.id(), part))
        };
    }

    /// The window whose title is under `position`: a tab of a group, or a
    /// window's own titlebar.
    pub(crate) fn tab_at(&self, position: Point<f64, Logical>) -> Option<Window> {
        self.title_bar_at(position)
            .filter(|(_, part)| *part == BarPart::Title)
            .map(|(window, _)| window)
    }

    /// The window a bar under `position` belongs to - for a tab group, the tab
    /// on show.
    fn bar_window_at(&self, position: Point<f64, Logical>) -> Option<Window> {
        let (member, _) = self.title_bar_at(position)?;
        let monitor = self.monitor_at(position)?;
        let Some(group) = monitor.active().tab_group_index(&member) else {
            return Some(member);
        };
        let tab_group = &monitor.active().tab_groups[group];
        tab_group.windows.get(tab_group.active).cloned()
    }

    pub fn fullscreen_window(&self, output: &Output) -> Option<&Window> {
        self.scroller
            .monitor_for(output)?
            .active()
            .fullscreen
            .as_ref()
    }

    pub(crate) fn renders_fullscreen(&self, output: &Output) -> bool {
        !self.overview_visible()
            && !self.scratchpad_overview_visible_on(output)
            && !self.scroller.scratchpads.values().any(|runtime| {
                runtime.visible && runtime.output.as_deref() == Some(output.name().as_str())
            })
            && self.fullscreen_window(output).is_some()
    }

    pub fn x11_override_windows(&self, output: &Output) -> Vec<(Window, Point<i32, Logical>)> {
        let Some(monitor) = self.scroller.monitor_for(output) else {
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

    /// Output-relative geometry and effective opacity for every window that is
    /// rendering on `output`, keyed by the render element id of its root
    /// surface.
    ///
    /// Built once per frame rather than answered per element. It used to be the
    /// latter, which meant walking every window on every workspace of every
    /// monitor for each of the frame's elements - and doing the whole walk
    /// twice over for the elements that are not windows at all, like layer
    /// surfaces and the cursor.
    ///
    /// Subsurfaces appear too, marked `subsurface`, so they are clipped to the
    /// window like the root is. Chromium draws its frame in subsurfaces that
    /// reach past its window geometry; left unclipped, they cover the focus
    /// ring.
    // `Id` hashes by the render element's identity - a `wl_resource` id or an
    // allocation - which never changes. The interior mutability clippy sees is
    // in the object it points at, not in what the hash is taken from.
    #[allow(clippy::mutable_key_type)]
    pub(crate) fn window_render_states(&self, output: &Output) -> HashMap<Id, WindowRenderState> {
        let Some(monitor) = self.scroller.monitor_for(output) else {
            return HashMap::new();
        };
        let Some(output_location) = monitor.space.output_geometry(output).map(|value| value.loc)
        else {
            return HashMap::new();
        };
        let now = self.start_time.elapsed();
        let opacity = self.config.appearance.window_opacity();
        let workspace = monitor.active();

        // Three kinds of window can be on screen here: the active workspace's
        // own, a scratchpad shown on this output, and a window being dragged.
        let scratchpads = self
            .scroller
            .scratchpads
            .values()
            .filter(|runtime| {
                runtime.visible && runtime.output.as_deref() == Some(monitor.name.as_str())
            })
            .filter_map(|runtime| runtime.window.as_ref());
        let dragged = self
            .scroller
            .drag_preview
            .and_then(|id| self.find_window(|window| window.id() == id));

        workspace
            .windows
            .iter()
            .map(|window| (window, false))
            .chain(scratchpads.map(|window| (window, true)))
            .chain(dragged.map(|window| (window, false)))
            .filter_map(|(window, scratchpad)| {
                let root = window_surface(window)?;
                let element = Id::from_wayland_resource(&root);
                let mut client = monitor.space.element_geometry(window)?;
                client.loc -= output_location;
                let frame = if has_bar(workspace, window, &self.config) {
                    bar_outer_rectangle(client, bar_height(&self.config))
                } else {
                    client
                };
                let fade = self
                    .scroller
                    .animations
                    .get(&window.id())
                    .map_or(1.0, |animation| animation.opacity(now));
                let state = WindowRenderState {
                    client,
                    frame,
                    opacity: opacity * fade,
                    scratchpad: scratchpad || self.is_scratchpad_window(window),
                    subsurface: false,
                };
                let mut surfaces = vec![(element, state)];
                let subsurface = WindowRenderState {
                    subsurface: true,
                    ..state
                };
                with_surface_tree_downward(
                    &root,
                    (),
                    |_, _, _| TraversalAction::DoChildren(()),
                    |surface, _, _| {
                        if *surface != root {
                            surfaces.push((Id::from_wayland_resource(surface), subsurface));
                        }
                    },
                    |_, _, _| true,
                );
                Some(surfaces)
            })
            .flatten()
            .collect()
    }

    fn unblock_surfaces(
        &mut self,
        output: &Output,
        mut ready: impl FnMut(&WlSurface, &smithay::wayland::compositor::SurfaceData) -> bool,
    ) {
        let mut clients = Vec::<Client>::new();
        let mut collect =
            |surface: &WlSurface, states: &smithay::wayland::compositor::SurfaceData| {
                if !ready(surface, states) {
                    return;
                }
                if let Some(client) = surface.client()
                    && !clients.iter().any(|known| known.id() == client.id())
                {
                    clients.push(client);
                }
            };
        if let Some(space) = self.space_for_output(output) {
            for window in space.elements() {
                window.with_surfaces(&mut collect);
            }
        }
        for window in self.overview_inactive_windows(output) {
            window.with_surfaces(&mut collect);
        }
        let layers = layer_map_for_output(output);
        for layer in layers.layers() {
            layer.with_surfaces(&mut collect);
        }
        drop(layers);

        let display_handle = self.display_handle.clone();
        for client in clients {
            self.client_compositor_state(&client)
                .blocker_cleared(self, &display_handle);
        }
    }

    pub fn pre_repaint(&mut self, output: &Output, frame_target: Time<Monotonic>) {
        self.unblock_surfaces(output, |_, states| {
            let Some(commit_timer) = states.data_map.get::<CommitTimerBarrierStateUserData>()
            else {
                return false;
            };
            commit_timer.lock().unwrap().signal_until(frame_target);
            true
        });
    }

    pub fn send_frames(&mut self, output: &Output) {
        let time = self.start_time.elapsed();
        // A surface hears from the output it was last presented on, every
        // frame. One that was not presented anywhere - a column scrolled off
        // screen - still hears now and then, so it keeps running, but it can no
        // longer spin its client at whatever rate this output is redrawn.
        let throttle = Some(HIDDEN_SURFACE_FRAME_INTERVAL);
        if let Some(space) = self.space_for_output(output) {
            for window in space.elements() {
                window.send_frame(output, time, throttle, surface_primary_scanout_output);
            }
        }
        for window in self.overview_inactive_windows(output) {
            window.send_frame(output, time, throttle, surface_primary_scanout_output);
        }
        let layers = layer_map_for_output(output);
        for layer in layers.layers() {
            layer.send_frame(output, time, throttle, surface_primary_scanout_output);
        }
        drop(layers);
        self.send_lock_frames(output, time);

        self.unblock_surfaces(output, |surface, states| {
            if surface_primary_scanout_output(surface, states)
                .is_some_and(|primary| primary != *output)
            {
                return false;
            }
            let barrier = states
                .cached_state
                .get::<FifoBarrierCachedState>()
                .current()
                .barrier
                .take();
            let Some(barrier) = barrier else {
                return false;
            };
            barrier.signal();
            true
        });
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
                    refresh_fractional_scale(surface, data);
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
        for window in self.overview_inactive_windows(output) {
            window.with_surfaces(|surface, data| {
                update_surface_primary_scanout_output(
                    surface,
                    output,
                    data,
                    None,
                    states,
                    default_primary_scanout_output_compare,
                );
                refresh_fractional_scale(surface, data);
            });
            window.take_presentation_feedback(
                &mut feedback,
                surface_primary_scanout_output,
                |surface, _| surface_presentation_feedback_flags_from_states(surface, None, states),
            );
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
                refresh_fractional_scale(surface, data);
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
        // Pruned here rather than while collecting elements: the fullscreen path
        // returns before that, and a game can stay fullscreen for hours.
        self.blur_surfaces.retain(Resource::is_alive);
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

    pub(crate) fn capture_output_for_window(&self, window: &Window) -> Option<Output> {
        if let Some(output) = self
            .scratchpad_name_for_window(window)
            .and_then(|name| self.scroller.scratchpads.get(&name))
            .and_then(|runtime| runtime.output.as_deref())
            .and_then(|output| self.scroller.monitor(output))
            .map(|monitor| monitor.output.clone())
        {
            return Some(output);
        }
        self.scroller.monitors.iter().find_map(|monitor| {
            monitor
                .workspaces
                .iter()
                .any(|workspace| workspace.windows.contains(window))
                .then(|| monitor.output.clone())
        })
    }

    pub(crate) fn capture_window_location(&self, window: &Window) -> Option<Point<i32, Logical>> {
        self.scroller
            .monitors
            .iter()
            .find_map(|monitor| monitor.space.element_location(window))
    }

    /// Mapped windows first, then every workspace: a window on an inactive
    /// workspace belongs to no space.
    fn find_window(&self, predicate: impl Fn(&Window) -> bool) -> Option<&Window> {
        self.scroller
            .scratchpads
            .values()
            .filter_map(|runtime| runtime.window.as_ref())
            .find(|window| predicate(window))
            .or_else(|| {
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
            })
            .or_else(|| {
                self.scroller
                    .minimized
                    .iter()
                    .map(|minimized| &minimized.window)
                    .find(|window| predicate(window))
            })
    }

    pub fn window_location(&self, surface: &WlSurface) -> Option<Point<i32, Logical>> {
        if let Some(location) = self.scroller.scratchpads.values().find_map(|runtime| {
            let window = runtime.window.as_ref()?;
            (window_surface(window).as_ref() == Some(surface))
                .then(|| {
                    runtime
                        .output
                        .as_deref()
                        .and_then(|output| self.scroller.monitor(output))
                        .and_then(|monitor| monitor.space.element_location(window))
                })
                .flatten()
        }) {
            return Some(location);
        }
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
        if self.session_locked() {
            return None;
        }
        let monitor = self.monitor_at(position)?;
        if let Some(window) = self.bar_window_at(position) {
            return Some(window);
        }
        if let Some((window, _)) = monitor.space.element_under(position)
            && self.is_scratchpad_window(window)
        {
            return Some(window.clone());
        }
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
        if self.session_locked() {
            return None;
        }
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
        if self.session_locked() {
            return self.lock_surface_under(position);
        }
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
        let top = layers
            .layer_under(Layer::Top, local)
            .and_then(layer_surface);
        let scratchpad = monitor
            .space
            .element_under(position)
            .filter(|(window, _)| self.is_scratchpad_window(window))
            .and_then(|(window, location)| {
                window
                    .surface_under(position - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(surface, offset)| (surface, (offset + location).to_f64()))
            });
        if let Some(window) = monitor.active().fullscreen.as_ref() {
            return overlay.or(scratchpad).or_else(|| {
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
        if overlay.is_none() && top.is_none() && self.uncovered_title_bar_at(position).is_some() {
            return None;
        }
        overlay
            .or(top)
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
        self.relayout_with_animation(reveal_focus, true);
    }

    fn relayout_with_animation(&mut self, reveal_focus: bool, animate: bool) {
        let enabled_layouts = &self.config.layout.enabled_layouts;
        let default_layout = self.config.layout.default;
        for workspace in self
            .scroller
            .monitors
            .iter_mut()
            .flat_map(|monitor| &mut monitor.workspaces)
            .chain(&mut self.scroller.parked)
        {
            if !workspace.layout_chosen && workspace.layout != default_layout {
                workspace.layout = default_layout;
                workspace.offset = 0;
            }
            if !enabled_layouts.contains(&workspace.layout) {
                workspace.layout = enabled_layouts[0];
                workspace.offset = 0;
            }
        }
        // Keyed by index rather than name: the geometry has to be collected up
        // front to release the borrow, and re-finding each monitor by name
        // afterwards costs a lock and an allocation per monitor per pass.
        let work_areas = self
            .scroller
            .monitors
            .iter()
            .enumerate()
            .filter_map(|(index, monitor)| {
                let output = monitor.space.output_geometry(&monitor.output)?;
                let zone = layer_map_for_output(&monitor.output).non_exclusive_zone();
                Some((index, output, work_area(output, zone)))
            })
            .collect::<Vec<_>>();
        // Resolved before the mutable borrows below, which run to the end of
        // the loop.
        let duration = animate.then(|| self.animation_duration()).flatten();
        let now = self.start_time.elapsed();
        let manual_floating = &self.scroller.manual_floating;
        let animations = &mut self.scroller.animations;
        let focused_output = self.scroller.focused_output.clone();
        let monitors = &mut self.scroller.monitors;
        for (index, output_geometry, geometry) in work_areas {
            let Some(monitor) = monitors.get_mut(index) else {
                continue;
            };
            let reveal_output =
                should_reveal_output(reveal_focus, focused_output.as_deref(), &monitor.name);
            let Scroller {
                space,
                workspaces,
                active_workspace,
                ..
            } = monitor;
            let workspace = &mut workspaces[*active_workspace];
            // Windows with a bar above them, which comes out of their tile.
            let barred = workspace
                .windows
                .iter()
                .filter(|window| has_bar(workspace, window, &self.config))
                .map(Window::id)
                .collect::<Vec<_>>();
            let bar = bar_height(&self.config);
            for window in workspace.tab_groups.iter().flat_map(|group| {
                group
                    .windows
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| *index != group.active)
                    .map(|(_, window)| window)
            }) {
                space.unmap_elem(window);
            }
            let floating_locations = workspace
                .windows
                .iter()
                .filter(|window| workspace.is_floating(window))
                .filter_map(|window| {
                    space
                        .element_location(window)
                        .map(|location| (window.id(), location))
                })
                .collect::<HashMap<_, _>>();
            let tiled = workspace
                .columns
                .iter()
                .flat_map(|column| column.windows.iter().cloned())
                .collect::<Vec<_>>();
            workspace.sync_layout_sizes(tiled.len());
            let placements = if workspace.layout == LayoutMode::Scrolling {
                let (gap, widths) = column_widths(
                    &workspace.columns,
                    geometry.size,
                    self.config.layout.scrolling.column_width,
                    workspace_gap(workspace, &self.config),
                    lone_full_width(&self.config),
                );
                workspace.offset = viewport_offset(
                    workspace.offset,
                    workspace.focused_column(),
                    &widths,
                    geometry.size.w,
                    gap,
                    self.config.layout.scrolling.center_focused_column && animate && reveal_output,
                    reveal_output,
                );
                place_columns(&workspace.columns, geometry, &widths, gap, workspace.offset)
            } else {
                workspace.offset = 0;
                if workspace.layout == LayoutMode::Dwindle {
                    let gap = effective_gap(
                        workspace_gap(workspace, &self.config),
                        geometry.size.w,
                        geometry.size,
                    );
                    workspace.dwindle_vertical = dwindle_cuts(
                        inset_rectangle(geometry, gap),
                        tiled.len(),
                        gap,
                        &workspace.dwindle_ratios,
                        &self.config.layout.dwindle,
                    )
                    .iter()
                    .map(|cut| cut.vertical)
                    .collect();
                }
                layout_rectangles(
                    workspace,
                    geometry,
                    tiled.len(),
                    workspace_gap(workspace, &self.config),
                    &self.config.layout,
                )
                .into_iter()
                .zip(tiled.iter().cloned())
                .map(|(rectangle, window)| (window, rectangle))
                .collect()
            };
            let floating = workspace
                .windows
                .iter()
                .filter(|window| workspace.is_floating(window))
                .cloned()
                .collect::<Vec<_>>();
            {
                // Sizes the window, then glides it from wherever it currently sits.
                let mut place =
                    |window: &Window, rectangle: Rectangle<i32, Logical>, client_sized: bool| {
                        configure_window(window, rectangle, client_sized);
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
                    };

                for (window, rectangle) in &placements {
                    let rectangle = if workspace.fullscreen.as_ref() == Some(window) {
                        output_geometry
                    } else if barred.contains(&window.id()) {
                        bar_client_rectangle(*rectangle, bar)
                    } else {
                        *rectangle
                    };
                    place(window, rectangle, false);
                }
                for window in &floating {
                    let fullscreen = workspace.fullscreen.as_ref() == Some(window);
                    let override_ = manual_floating.get(&window.id());
                    let rectangle = if fullscreen {
                        output_geometry
                    } else {
                        // A floating window keeps its own size; its bar comes out
                        // of the room above it instead.
                        floating_rectangle(
                            window,
                            if barred.contains(&window.id()) {
                                bar_client_rectangle(geometry, bar)
                            } else {
                                geometry
                            },
                            override_,
                            floating_locations.get(&window.id()).copied(),
                        )
                    };
                    place(
                        window,
                        rectangle,
                        !fullscreen && override_.is_none_or(|override_| override_.size.is_none()),
                    );
                }
            }
            if workspace.layout == LayoutMode::Monocle
                && let Some(window) = workspace
                    .focused
                    .and_then(|index| workspace.windows.get(index))
                    .filter(|window| !workspace.is_floating(window))
            {
                space.raise_element(window, false);
            }
            for window in floating {
                space.raise_element(&window, false);
            }
        }
        let scratchpad_placements = self
            .scroller
            .scratchpads
            .iter()
            .filter_map(|(name, runtime)| {
                let window = runtime.window.clone()?;
                let output = runtime.output.clone()?;
                let rectangle = runtime
                    .visible
                    .then(|| self.scratchpad_rectangle(name, &output))??;
                Some((output, window, rectangle))
            })
            .collect::<Vec<_>>();
        for (output, window, rectangle) in scratchpad_placements {
            configure_window(&window, rectangle, false);
            if !self.scroller.animations.contains_key(&window.id())
                && let Some(monitor) = self.scroller.monitor_mut(&output)
            {
                monitor.space.map_element(window, rectangle.loc, true);
            }
        }
        let ordered = self
            .scroller
            .scratchpad_order
            .iter()
            .filter_map(|name| {
                let runtime = self.scroller.scratchpads.get(name)?;
                Some((
                    runtime.output.clone()?,
                    runtime.window.clone()?,
                    runtime.visible,
                ))
            })
            .collect::<Vec<_>>();
        for (output, window, visible) in ordered {
            if visible && let Some(monitor) = self.scroller.monitor_mut(&output) {
                monitor.space.raise_element(&window, false);
            }
        }
    }

    /// Re-maps every animating window at its position for the current instant.
    /// Progress comes from absolute time, so calling this repeatedly within one
    /// frame - once per output, and again per screencopy capture - is a no-op
    /// beyond the first.
    /// How long a motion should take, or `None` when animation is off - which
    /// `start_motion` reads as "go straight there".
    fn animation_duration(&self) -> Option<Duration> {
        self.config
            .animations
            .active()
            .then_some(self.config.animations.duration)
    }

    pub fn advance_animations(&mut self) {
        if self.scroller.animations.is_empty() {
            return;
        }
        let now = self.start_time.elapsed();
        let mut animations = std::mem::take(&mut self.scroller.animations);
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
        let scratchpads = self
            .scroller
            .scratchpads
            .iter()
            .filter_map(|(name, runtime)| {
                let window = runtime.window.clone()?;
                let output = runtime.output.clone()?;
                let animation = animations.get(&window.id())?;
                Some((
                    name.clone(),
                    window,
                    output,
                    animation.position(now),
                    runtime.hiding,
                ))
            })
            .collect::<Vec<_>>();
        for (_, window, output, position, _) in &scratchpads {
            if let Some(monitor) = self.scroller.monitor_mut(output) {
                monitor.space.map_element(window.clone(), *position, true);
                monitor.space.raise_element(window, false);
            }
        }
        let hidden = scratchpads
            .into_iter()
            .filter_map(|(name, window, output, _, hiding)| {
                (hiding
                    && animations
                        .get(&window.id())
                        .is_some_and(|value| value.finished(now)))
                .then_some((name, window, output))
            })
            .collect::<Vec<_>>();
        let visibility_changed = !hidden.is_empty();
        animations.retain(|_, animation| !animation.finished(now));
        for (name, window, output) in hidden {
            if let Some(monitor) = self.scroller.monitor_mut(&output) {
                monitor.space.unmap_elem(&window);
            }
            if let Some(runtime) = self.scroller.scratchpads.get_mut(&name) {
                runtime.visible = false;
                runtime.hiding = false;
            }
        }
        self.scroller.animations = animations;
        if visibility_changed {
            self.ipc.touch();
        }
    }

    /// Whether anything on screen is still moving, so the backend keeps queueing
    /// repaints. The overview tweens count: without them the transition would
    /// stall on the first frame that produced no damage.
    pub fn animations_active(&self) -> bool {
        let now = self.start_time.elapsed();
        !self.scroller.animations.is_empty()
            || !self.scroller.overview_zoom.finished(now)
            || !self.scroller.overview_scroll.finished(now)
            || !self.scroller.overview_pan.finished(now)
    }

    pub(crate) fn sync_focus(&mut self) {
        if self.session_locked() {
            // Nothing but the lock surface may hold the keyboard, not even
            // an exclusive layer surface. Popup grabs end with their focus.
            let focus = self.lock_keyboard_focus().map(KeyboardFocus::Surface);
            self.keyboard()
                .set_focus(self, focus, SERIAL_COUNTER.next_serial());
            return;
        }
        let active_output = self.scroller.focused_output.clone();
        let exclusive = self.exclusive_keyboard_surface();
        if exclusive.is_some() && self.scroller.overview.take().is_some() {
            self.retarget_overview(true);
        }
        if exclusive.is_some() {
            self.scroller.scratchpad_overview = None;
        }
        let overview = self.overview_active() || self.scroller.scratchpad_overview.is_some();
        let focused_surface = exclusive.map(KeyboardFocus::Surface).or_else(|| {
            if overview {
                return None;
            }
            if let Some(name) = &self.scroller.focused_scratchpad
                && let Some(focus) = self
                    .scroller
                    .scratchpads
                    .get(name)
                    .filter(|runtime| {
                        runtime.visible && runtime.output.as_deref() == active_output.as_deref()
                    })
                    .and_then(|runtime| runtime.window.as_ref())
                    .and_then(window_keyboard_focus)
            {
                return Some(focus);
            }
            self.scroller
                .monitors
                .iter()
                .find(|monitor| Some(monitor.name.as_str()) == active_output.as_deref())
                .and_then(|monitor| {
                    let workspace = monitor.active();
                    workspace
                        .fullscreen
                        .as_ref()
                        .and_then(window_keyboard_focus)
                        .or_else(|| {
                            workspace
                                .focused
                                .and_then(|index| workspace.windows.get(index))
                                .and_then(window_keyboard_focus)
                        })
                })
        });

        for (name, runtime) in &self.scroller.scratchpads {
            if let Some(window) = &runtime.window {
                window.set_activated(
                    !overview
                        && runtime.visible
                        && runtime.output.as_deref() == active_output.as_deref()
                        && self.scroller.focused_scratchpad.as_deref() == Some(name),
                );
                if let Some(surface) = window.toplevel()
                    && surface.is_initial_configure_sent()
                {
                    surface.send_pending_configure();
                }
            }
        }

        for monitor in &self.scroller.monitors {
            for (workspace_index, workspace) in monitor.workspaces.iter().enumerate() {
                for (index, window) in workspace.windows.iter().enumerate() {
                    window.set_activated(
                        !overview
                            && Some(monitor.name.as_str()) == active_output.as_deref()
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
        let keyboard = self.keyboard();
        // Reading the outgoing focus clones it, and `sync_focus` runs on layer
        // surface commits, so only pay for it when there is an inhibitor to
        // retarget. Smithay exposes no way to enumerate inhibitors, which is why
        // the previous focus has to be captured before it is replaced.
        let inhibiting = self.seat.keyboard_shortcuts_inhibited();
        let previous = inhibiting.then(|| keyboard.current_focus()).flatten();
        let current = inhibiting.then(|| focused_surface.clone()).flatten();
        keyboard.set_focus(self, focused_surface, SERIAL_COUNTER.next_serial());
        if inhibiting {
            self.retarget_shortcuts_inhibitors(previous.as_ref(), current.as_ref());
        }
    }

    /// A shortcuts inhibitor only holds while its surface has keyboard focus, so
    /// a background window cannot swallow every binding and trap the user.
    fn retarget_shortcuts_inhibitors(
        &self,
        previous: Option<&KeyboardFocus>,
        current: Option<&KeyboardFocus>,
    ) {
        if previous == current {
            return;
        }
        if let Some(inhibitor) = previous
            .and_then(|focus| focus.wl_surface())
            .and_then(|surface| self.seat.keyboard_shortcuts_inhibitor_for_surface(&surface))
        {
            inhibitor.inactivate();
        }
        if let Some(inhibitor) = current
            .and_then(|focus| focus.wl_surface())
            .and_then(|surface| self.seat.keyboard_shortcuts_inhibitor_for_surface(&surface))
        {
            inhibitor.activate();
        }
    }

    pub(crate) fn has_exclusive_keyboard_layer(&self) -> bool {
        self.exclusive_keyboard_surface().is_some()
    }

    fn exclusive_keyboard_surface(&self) -> Option<WlSurface> {
        let focused_output = self.scroller.focused_output.as_deref();
        self.scroller
            .monitors
            .iter()
            .filter(|monitor| Some(monitor.name.as_str()) == focused_output)
            .chain(
                self.scroller
                    .monitors
                    .iter()
                    .filter(|monitor| Some(monitor.name.as_str()) != focused_output),
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

/// The keyboard focus target for a window. Prefers the `X11Surface`, because
/// that is the arm that moves the X server's input focus; see
/// [`crate::handlers::KeyboardFocus`].
fn window_keyboard_focus(window: &Window) -> Option<KeyboardFocus> {
    if let Some(surface) = window.x11_surface() {
        return KeyboardFocus::x11(surface);
    }
    window
        .toplevel()
        .map(|surface| KeyboardFocus::Surface(surface.wl_surface().clone()))
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

fn window_is_fixed_size(window: &Window) -> bool {
    if let Some(surface) = window.toplevel() {
        return with_states(surface.wl_surface(), |states| {
            let mut cached = states.cached_state.get::<SurfaceCachedState>();
            let cached = cached.current();
            fixed_size(cached.min_size, cached.max_size)
        });
    }
    window.x11_surface().is_some_and(|surface| {
        surface
            .min_size()
            .zip(surface.max_size())
            .is_some_and(|(minimum, maximum)| fixed_size(minimum, maximum))
    })
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
    let current = geometries.iter().position(|(name, _)| name == current)?;
    let rectangles = geometries
        .iter()
        .map(|(_, geometry)| *geometry)
        .collect::<Vec<_>>();
    directional_index(current, direction, &rectangles, false)
        .map(|index| geometries[index].0.clone())
}

fn directional_index(
    current: usize,
    direction: Direction,
    geometries: &[Rectangle<i32, Logical>],
    prefer_aligned: bool,
) -> Option<usize> {
    let source_geometry = *geometries.get(current)?;
    let source = center(source_geometry);
    geometries
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != current)
        .filter_map(|(index, geometry)| {
            let target = center(*geometry);
            let (primary, secondary) = match direction {
                Direction::Left => (source.x - target.x, (source.y - target.y).abs()),
                Direction::Right => (target.x - source.x, (source.y - target.y).abs()),
                Direction::Up => (source.y - target.y, (source.x - target.x).abs()),
                Direction::Down => (target.y - source.y, (source.x - target.x).abs()),
            };
            let aligned = match direction {
                Direction::Left | Direction::Right => {
                    geometry.loc.y < source_geometry.loc.y + source_geometry.size.h
                        && source_geometry.loc.y < geometry.loc.y + geometry.size.h
                }
                Direction::Up | Direction::Down => {
                    geometry.loc.x < source_geometry.loc.x + source_geometry.size.w
                        && source_geometry.loc.x < geometry.loc.x + geometry.size.w
                }
            };
            (primary > 0).then_some((
                usize::from(prefer_aligned && !aligned),
                primary as i64 * primary as i64 + secondary as i64 * secondary as i64,
                index,
            ))
        })
        .min_by_key(|(alignment, score, _)| (*alignment, *score))
        .map(|(_, _, index)| index)
}

fn center(rectangle: Rectangle<i32, Logical>) -> Point<i32, Logical> {
    rectangle.loc + rectangle.size / 2
}

fn drag_preview_size(size: Size<i32, Logical>) -> Size<i32, Logical> {
    ((size.w * 3 / 4).max(1), (size.h * 3 / 4).max(1)).into()
}

fn default_floating_size(size: Size<i32, Logical>) -> Size<i32, Logical> {
    ((size.w / 2).max(1), (size.h / 2).max(1)).into()
}

fn drag_grab_offset(
    pointer: Point<f64, Logical>,
    geometry: Rectangle<i32, Logical>,
) -> Point<f64, Logical> {
    let offset = pointer - geometry.loc.to_f64();
    (offset.x * 0.75, offset.y * 0.75).into()
}

fn floating_resize_edges(
    pointer: Point<f64, Logical>,
    geometry: Rectangle<i32, Logical>,
    margin: i32,
) -> (Option<FloatingEdge>, Option<FloatingEdge>) {
    let edge = |position: f64, start: i32, length: i32| {
        let start_distance = (position - f64::from(start)).abs();
        let end_distance = (position - f64::from(start + length)).abs();
        match (
            start_distance <= f64::from(margin),
            end_distance <= f64::from(margin),
        ) {
            (true, true) if start_distance <= end_distance => Some(FloatingEdge::Start),
            (true, false) => Some(FloatingEdge::Start),
            (_, true) => Some(FloatingEdge::End),
            _ => None,
        }
    };
    (
        edge(pointer.x, geometry.loc.x, geometry.size.w),
        edge(pointer.y, geometry.loc.y, geometry.size.h),
    )
}

fn resized_floating_rectangle(
    initial: Rectangle<i32, Logical>,
    x: Option<FloatingEdge>,
    y: Option<FloatingEdge>,
    delta: Point<f64, Logical>,
    area: Rectangle<i32, Logical>,
    requested_minimum: i32,
) -> Rectangle<i32, Logical> {
    let resize_axis = |start: i32,
                       length: i32,
                       area_start: i32,
                       area_length: i32,
                       edge: Option<FloatingEdge>,
                       delta: f64| {
        let maximum = area_length.max(1);
        let minimum = requested_minimum.clamp(1, maximum);
        let length = length.clamp(minimum, maximum);
        let start = start.clamp(area_start, area_start + maximum - length);
        let end = start + length;
        match edge {
            Some(FloatingEdge::Start) => {
                let resized = (start + delta.round() as i32).clamp(area_start, end - minimum);
                (resized, end - resized)
            }
            Some(FloatingEdge::End) => {
                let resized =
                    (end + delta.round() as i32).clamp(start + minimum, area_start + maximum);
                (start, resized - start)
            }
            None => (start, length),
        }
    };
    let (x, width) = resize_axis(
        initial.loc.x,
        initial.size.w,
        area.loc.x,
        area.size.w,
        x,
        delta.x,
    );
    let (y, height) = resize_axis(
        initial.loc.y,
        initial.size.h,
        area.loc.y,
        area.size.h,
        y,
        delta.y,
    );
    Rectangle::new((x, y).into(), (width, height).into())
}

fn tiled_drop_target(
    workspace: &Workspace,
    space: &Space<Window>,
    dragged: &Window,
    pointer: Point<f64, Logical>,
) -> Option<Window> {
    let candidates = workspace
        .columns
        .iter()
        .flat_map(|column| &column.windows)
        .filter(|window| *window != dragged)
        .filter_map(|window| {
            space
                .element_geometry(window)
                .map(|geometry| (window, geometry))
        })
        .collect::<Vec<_>>();
    candidates
        .iter()
        .find(|(_, geometry)| geometry.to_f64().contains(pointer))
        .or_else(|| {
            candidates.iter().min_by_key(|(_, geometry)| {
                let target = center(*geometry).to_f64();
                let delta = target - pointer;
                (delta.x * delta.x + delta.y * delta.y) as i64
            })
        })
        .map(|(window, _)| (*window).clone())
}

fn master_neighbor(
    focused: usize,
    master_count: usize,
    horizontal: bool,
    delta: i32,
    geometries: &[Rectangle<i32, Logical>],
) -> Option<usize> {
    let source = center(*geometries.get(focused)?);
    let master_count = master_count.min(geometries.len());
    let in_master = focused < master_count;
    let range = if horizontal {
        match (in_master, delta.signum()) {
            (true, 1) => master_count..geometries.len(),
            (false, -1) => 0..master_count,
            _ => return None,
        }
    } else if in_master {
        0..master_count
    } else {
        master_count..geometries.len()
    };
    range
        .filter(|index| *index != focused)
        .filter_map(|index| {
            let target = center(geometries[index]);
            if !horizontal && (target.y - source.y).signum() != delta.signum() {
                return None;
            }
            Some((index, (target.y - source.y).abs()))
        })
        .min_by_key(|(_, distance)| *distance)
        .map(|(index, _)| index)
}

fn centered_rectangle(
    area: Rectangle<i32, Logical>,
    size: Size<i32, Logical>,
) -> Rectangle<i32, Logical> {
    Rectangle::new(area.loc + (area.size - size) / 2, size)
}

fn column_insertion_at(workspace: &Workspace, space: &Space<Window>, x: f64) -> usize {
    let centers = workspace
        .columns
        .iter()
        .map(|column| {
            space
                .element_geometry(&column.windows[0])
                .map(|geometry| geometry.loc.x + geometry.size.w / 2)
                .unwrap_or(i32::MAX)
        })
        .collect::<Vec<_>>();
    insertion_index(&centers, x)
}

fn insertion_index(centers: &[i32], x: f64) -> usize {
    centers
        .iter()
        .position(|center| x < f64::from(*center))
        .unwrap_or(centers.len())
}

fn constrain_window(
    area: Rectangle<i32, Logical>,
    size: Size<i32, Logical>,
    location: Point<i32, Logical>,
) -> Point<i32, Logical> {
    let max: Point<i32, Logical> = (
        area.loc.x + (area.size.w - size.w).max(0),
        area.loc.y + (area.size.h - size.h).max(0),
    )
        .into();
    (
        location.x.clamp(area.loc.x, max.x),
        location.y.clamp(area.loc.y, max.y),
    )
        .into()
}

fn visible_workspaces(workspaces: &[Workspace; WORKSPACE_COUNT], active: usize) -> Vec<usize> {
    visible_workspace_indices(
        workspaces
            .iter()
            .map(|workspace| !workspace.windows.is_empty()),
        active,
    )
}

fn visible_workspace_indices(occupied: impl Iterator<Item = bool>, active: usize) -> Vec<usize> {
    occupied
        .enumerate()
        .filter_map(|(index, occupied)| (index == active || occupied).then_some(index))
        .collect()
}

fn wrapped_selection(workspaces: &[usize], current: usize, delta: i32) -> Option<usize> {
    let current = workspaces
        .iter()
        .position(|workspace| *workspace == current)?;
    let target = wrapping_index(current, workspaces.len(), delta);
    workspaces.get(target).copied()
}

/// The workspace strip: one card per visible workspace, stacked down the output
/// and centred on `center`.
///
/// `progress` runs 0.0 (closed) to 1.0 (open) and drives both the shrink and
/// the gap, so at 0.0 the centred card is exactly the output rectangle and the
/// overview and the plain desktop line up pixel for pixel. That is what lets
/// the transition animate without a seam.
fn overview_cards(
    output: Size<i32, Logical>,
    workspaces: &[usize],
    center: f64,
    progress: f64,
    zoom: f64,
) -> Vec<(usize, Rectangle<i32, Logical>)> {
    let scale = overview_zoom(progress, zoom);
    let size: Size<i32, Logical> = (
        (f64::from(output.w) * scale).round().max(1.0) as i32,
        (f64::from(output.h) * scale).round().max(1.0) as i32,
    )
        .into();
    let gap = (f64::from(OVERVIEW_GAP) * progress).round() as i32;
    let step = size.h + gap;
    let x = (output.w - size.w) / 2;
    let start_y = (output.h - size.h) / 2 - (center * f64::from(step)).round() as i32;
    workspaces
        .iter()
        .enumerate()
        .map(|(position, workspace)| {
            (
                *workspace,
                Rectangle::new((x, start_y + position as i32 * step).into(), size),
            )
        })
        .collect()
}

/// A card in the strip, paired with the scratchpad it shows.
type ScratchpadCard = (usize, Rectangle<i32, Logical>);

/// The scratchpad strip: a panel inside `area` holding one line of cards.
///
/// The panel hugs exactly the cards it can show, so it never draws a card that
/// would be cut off and never leaves a bordered box half empty. `area` is the
/// output's usable space already inset by the window gap, so the panel clears
/// bars and docks and stands off the screen edge like a window does.
fn scratchpad_strip_layout(
    area: Rectangle<i32, Logical>,
    count: usize,
    position: ScratchpadOverviewPosition,
    selected: usize,
) -> (Rectangle<i32, Logical>, Vec<ScratchpadCard>) {
    let gap = OVERVIEW_GAP;
    let horizontal = position.horizontal();
    // Cards keep the usable area's aspect ratio, so a preview reads as a
    // shrunken screen rather than a stretched one.
    let card: Size<i32, Logical> = if horizontal {
        let height = (area.size.h / 3 - gap * 2).max(1);
        ((height * area.size.w / area.size.h).max(1), height).into()
    } else {
        let width = (area.size.w / 3 - gap * 2).max(1);
        (width, (width * area.size.h / area.size.w).max(1)).into()
    };
    let (along, across) = if horizontal {
        (card.w, card.h)
    } else {
        (card.h, card.w)
    };
    let available = if horizontal { area.size.w } else { area.size.h };

    let step = along + gap;
    let visible = ((available - gap) / step).clamp(1, count as i32);
    // One gap between every pair of cards plus one at each end.
    let panel_along = visible * step + gap;
    let panel_across = across + gap * 2;
    let panel = if horizontal {
        Rectangle::new(
            (
                area.loc.x + (area.size.w - panel_along) / 2,
                area.loc.y + cross_offset(position, area.size.h, panel_across),
            )
                .into(),
            (panel_along, panel_across).into(),
        )
    } else {
        Rectangle::new(
            (
                area.loc.x + cross_offset(position, area.size.w, panel_across),
                area.loc.y + (area.size.h - panel_along) / 2,
            )
                .into(),
            (panel_across, panel_along).into(),
        )
    };

    // Centre the selection, but stop at either end so the panel never shows
    // empty space where the list has run out.
    let first = (selected as i32 - visible / 2).clamp(0, count as i32 - visible);
    let cards = (0..visible)
        .map(|slot| {
            let offset = gap + slot * step;
            let location = if horizontal {
                (panel.loc.x + offset, panel.loc.y + gap)
            } else {
                (panel.loc.x + gap, panel.loc.y + offset)
            };
            (
                (first + slot) as usize,
                Rectangle::new(location.into(), card),
            )
        })
        .collect();
    (panel, cards)
}

/// Where the panel sits across its scroll axis: against the near edge, the far
/// edge, or in the middle.
fn cross_offset(position: ScratchpadOverviewPosition, available: i32, panel: i32) -> i32 {
    match position {
        ScratchpadOverviewPosition::Top | ScratchpadOverviewPosition::Left => 0,
        ScratchpadOverviewPosition::Bottom | ScratchpadOverviewPosition::Right => available - panel,
        ScratchpadOverviewPosition::Centered => (available - panel) / 2,
    }
}

/// How far workspace contents shrink at this point in the transition: 1.0 while
/// closed, the configured zoom once open.
fn overview_zoom(progress: f64, zoom: f64) -> f64 {
    1.0 + (zoom - 1.0) * progress.clamp(0.0, 1.0)
}

/// Where a window that really sits at `geometry` lands inside its card.
pub(crate) fn overview_screen_rect(
    geometry: Rectangle<i32, Logical>,
    card: Rectangle<i32, Logical>,
    zoom: f64,
) -> Rectangle<i32, Logical> {
    let axis = |value: i32| (f64::from(value) * zoom).round() as i32;
    Rectangle::new(
        (
            card.loc.x + axis(geometry.loc.x),
            card.loc.y + axis(geometry.loc.y),
        )
            .into(),
        (axis(geometry.size.w).max(1), axis(geometry.size.h).max(1)).into(),
    )
}

/// Where every window of `workspace` sits, exactly as the real layout would
/// place it in `area`. The preview goes through the same helpers as
/// `relayout_with_animation`, so what the overview shows is what you get when
/// you switch to it.
fn overview_window_placements(
    workspace: &Workspace,
    area: Rectangle<i32, Logical>,
    output: Rectangle<i32, Logical>,
    offset: i32,
    config: &Config,
    manual_floating: &HashMap<usize, ManualFloating>,
) -> Vec<(Window, Rectangle<i32, Logical>)> {
    if let Some(window) = &workspace.fullscreen {
        return vec![(window.clone(), output)];
    }

    let tiled = workspace
        .columns
        .iter()
        .flat_map(|column| column.windows.iter().cloned())
        .collect::<Vec<_>>();
    let mut placements = if workspace.layout == LayoutMode::Scrolling {
        let (gap, widths) = column_widths(
            &workspace.columns,
            area.size,
            config.layout.scrolling.column_width,
            workspace_gap(workspace, config),
            lone_full_width(config),
        );
        place_columns(&workspace.columns, area, &widths, gap, offset)
    } else {
        layout_rectangles(
            workspace,
            area,
            tiled.len(),
            workspace_gap(workspace, config),
            &config.layout,
        )
        .into_iter()
        .zip(tiled)
        .map(|(rectangle, window)| (window, rectangle))
        .collect()
    };

    let bar = bar_height(config);
    for (window, rectangle) in &mut placements {
        if has_bar(workspace, window, config) {
            *rectangle = bar_client_rectangle(*rectangle, bar);
        }
    }
    placements.extend(
        workspace
            .windows
            .iter()
            .filter(|window| workspace.is_floating(window))
            .map(|window| {
                let area = if has_bar(workspace, window, config) {
                    bar_client_rectangle(area, bar)
                } else {
                    area
                };
                (
                    window.clone(),
                    floating_rectangle(window, area, manual_floating.get(&window.id()), None),
                )
            }),
    );
    placements
}

/// How tall the bar above a window is: the titlebar height while titlebars are
/// on, else a tab group's fixed one.
fn bar_height(config: &Config) -> i32 {
    if config.appearance.titlebars {
        config.appearance.titlebar_height
    } else {
        TAB_BAR_HEIGHT
    }
}

/// Whether a bar sits above `window`: its tab group's, or its own titlebar.
/// Never on a fullscreen window, and never on a scratchpad, which the
/// workspace does not lay out.
fn has_bar(workspace: &Workspace, window: &Window, config: &Config) -> bool {
    workspace.windows.contains(window)
        && workspace.fullscreen.as_ref() != Some(window)
        && (workspace.tab_group_index(window).is_some()
            || (config.appearance.titlebars && crate::decoration::has_titlebar(window)))
}

/// The window's own part of a tile whose top `height` pixels are its bar.
fn bar_client_rectangle(
    mut outer: Rectangle<i32, Logical>,
    height: i32,
) -> Rectangle<i32, Logical> {
    let height = height.min((outer.size.h - 1).max(0));
    outer.loc.y += height;
    outer.size.h -= height;
    outer
}

/// A window plus the bar above it.
fn bar_outer_rectangle(
    mut client: Rectangle<i32, Logical>,
    height: i32,
) -> Rectangle<i32, Logical> {
    client.loc.y -= height;
    client.size.h += height;
    client
}

fn tab_index_at(position: f64, width: i32, count: usize) -> Option<usize> {
    (count > 0).then(|| {
        ((position.max(0.0) * count as f64 / f64::from(width.max(1))) as usize).min(count - 1)
    })
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

fn column_neighbor<T: Clone + PartialEq>(
    columns: &[Column<T>],
    floating: &[T],
    current: &T,
    delta: i32,
) -> Option<T> {
    let current = columns
        .iter()
        .position(|column| column.windows.contains(current))
        .or_else(|| {
            floating
                .iter()
                .position(|window| window == current)
                .map(|index| columns.len() + index)
        })?;
    let target = neighbor(current, columns.len() + floating.len(), delta)?;
    columns
        .get(target)
        .and_then(Column::focused_window)
        // `checked_sub`, not `saturating_sub`: a column whose `focused` index has
        // gone stale would otherwise fall through to the first floating window
        // instead of reporting that it has nothing to offer.
        .or_else(|| floating.get(target.checked_sub(columns.len())?))
        .cloned()
}

fn row_neighbor<T: Clone + PartialEq>(columns: &[Column<T>], current: &T, delta: i32) -> Option<T> {
    let column = columns
        .iter()
        .find(|column| column.windows.contains(current))?;
    let current = column.windows.iter().position(|window| window == current)?;
    column
        .windows
        .get(neighbor(current, column.windows.len(), delta)?)
        .cloned()
}

fn swap_column_window<T>(column: &mut Column<T>, delta: i32) -> bool {
    let Some(next) = neighbor(column.focused, column.windows.len(), delta) else {
        return false;
    };
    column.windows.swap(column.focused, next);
    column.row_weights.swap(column.focused, next);
    column.focused = next;
    true
}

fn consume_or_expel_columns<T>(columns: &mut Vec<Column<T>>, current: usize, delta: i32) -> bool {
    if columns[current].windows.len() == 1 {
        let Some(target) = neighbor(current, columns.len(), delta) else {
            return false;
        };
        let mut source = columns.remove(current);
        let window = source.windows.remove(0);
        let target = if target > current { target - 1 } else { target };
        let weight = average_weight(&columns[target].row_weights);
        columns[target].windows.push(window);
        columns[target].row_weights.push(weight);
        columns[target].focused = columns[target].windows.len() - 1;
    } else {
        let column = &mut columns[current];
        // Clamped: `remove` would panic on a stale index, and this is the last
        // place that indexes `windows` by `focused` directly.
        let row = column.focused.min(column.windows.len() - 1);
        let window = column.windows.remove(row);
        column.row_weights.remove(row);
        column.focused = row.min(column.windows.len() - 1);
        let width = column.width;
        let target = if delta < 0 { current } else { current + 1 };
        columns.insert(
            target,
            Column {
                windows: vec![window],
                row_weights: vec![1.0],
                focused: 0,
                width,
            },
        );
    }
    true
}

fn average_weight(weights: &[f64]) -> f64 {
    weights.iter().sum::<f64>() / weights.len().max(1) as f64
}

fn cycle_column_width<T>(column: &mut Column<T>, default: f64, presets: &[f64]) {
    let current = column.width.unwrap_or(default);
    column.width = Some(next_preset(current, presets));
}

fn next_preset(current: f64, presets: &[f64]) -> f64 {
    let next = presets
        .iter()
        .position(|width| (*width - current).abs() < f64::EPSILON)
        .map_or(0, |index| (index + 1) % presets.len());
    presets[next]
}

/// `current` moved `delta` places through `len` items, wrapping at both ends.
///
/// Guards `len == 0` because `rem_euclid(0)` panics, and two of the callers
/// count a list a client can empty.
fn wrapping_index(current: usize, len: usize, delta: i32) -> usize {
    if len == 0 {
        return 0;
    }
    (current as i32 + delta).rem_euclid(len as i32) as usize
}

fn workspace_neighbor(current: usize, delta: i32) -> usize {
    wrapping_index(current, WORKSPACE_COUNT, delta)
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

fn should_reveal_output(reveal_focus: bool, focused_output: Option<&str>, output: &str) -> bool {
    reveal_focus && focused_output == Some(output)
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

/// The width of every column of a scrolling workspace, and the gap that fits
/// between them. Shared by the real layout and the overview preview so the two
/// cannot drift apart. With `lone_full_width`, a workspace holding a single
/// window gives it the whole width.
fn column_widths<T>(
    columns: &[Column<T>],
    area: Size<i32, Logical>,
    default_width: f64,
    requested_gap: i32,
    lone_full_width: bool,
) -> (i32, Vec<i32>) {
    let lone = lone_full_width && is_lone_window(columns);
    let fractions = columns
        .iter()
        .map(|column| {
            if lone {
                1.0
            } else {
                column.width.unwrap_or(default_width)
            }
        })
        .collect::<Vec<_>>();
    let requested_width = fractions
        .iter()
        .map(|fraction| (f64::from(area.w) * fraction).round() as i32)
        .min()
        .unwrap_or(area.w);
    let gap = effective_gap(requested_gap, requested_width, area);
    let widths = fractions
        .iter()
        .map(|fraction| column_width(area.w, *fraction, gap))
        .collect();
    (gap, widths)
}

/// Where a workspace's preview should be scrolled to so that `column` lands on
/// its card.
///
/// The card is screen-sized, so this is the same question the real viewport
/// answers when you focus a column - and it is answered with the same
/// `viewport_offset`, so panning in the overview and focusing on the desktop can
/// never disagree. A strip that already fits clamps to `0`, which is what keeps
/// the preview equal to reality in the common case.
#[allow(clippy::too_many_arguments)]
fn overview_pan_target<T>(
    columns: &[Column<T>],
    area: Size<i32, Logical>,
    column: Option<usize>,
    current: i32,
    default_width: f64,
    requested_gap: i32,
    lone_full_width: bool,
    center: bool,
) -> i32 {
    // No column means a floating window is selected. Those do not scroll with
    // the strip on the real desktop either, and they always sit inside the work
    // area, so leaving the pan alone keeps them in view.
    let Some(column) = column else {
        return current;
    };
    let (gap, widths) = column_widths(columns, area, default_width, requested_gap, lone_full_width);
    viewport_offset(current, Some(column), &widths, area.w, gap, center, true)
}

/// Where the tiled windows of a scrolling workspace sit, for a viewport scrolled
/// to `offset`.
fn place_columns<T: Clone>(
    columns: &[Column<T>],
    area: Rectangle<i32, Logical>,
    widths: &[i32],
    gap: i32,
    offset: i32,
) -> Vec<(T, Rectangle<i32, Logical>)> {
    let mut column_x = area.loc.x + gap - offset;
    let mut placements = Vec::new();
    for (column, width) in columns.iter().zip(widths) {
        let rows = stacked_rows(
            area.loc.y,
            area.size.h,
            &column.row_weights,
            gap,
            MIN_ROW_HEIGHT,
        );
        placements.extend(column.windows.iter().zip(rows).map(|(window, row)| {
            (
                window.clone(),
                Rectangle::new(
                    (column_x, row.loc.y).into(),
                    (width - gap, row.size.h).into(),
                ),
            )
        }));
        column_x += width;
    }
    placements
}

/// Where a floating window sits inside `area`: at its manual override if it has
/// one, else where it already was, else centred.
fn floating_rectangle(
    window: &Window,
    area: Rectangle<i32, Logical>,
    override_: Option<&ManualFloating>,
    current: Option<Point<i32, Logical>>,
) -> Rectangle<i32, Logical> {
    let size = override_
        .and_then(|override_| override_.size)
        .unwrap_or_else(|| floating_size(window));
    let location = override_
        .and_then(|override_| override_.location)
        .or(current)
        .map_or_else(
            || centered_rectangle(area, size).loc,
            |location| constrain_window(area, size, location),
        );
    Rectangle::new(location, size)
}

/// The gap a workspace's tiled windows keep: `appearance.gaps`, or none
/// around a lone tiled window under `appearance.smart_gaps`.
fn workspace_gap(workspace: &Workspace, config: &Config) -> i32 {
    tiled_gap(
        workspace.tiled_count(),
        config.appearance.gaps,
        config.appearance.smart_gaps,
    )
}

/// Whether a scrolling workspace's only window should span the whole width.
fn lone_full_width(config: &Config) -> bool {
    config.appearance.smart_gaps || config.layout.scrolling.lone_window_full_width
}

fn is_lone_window<T>(columns: &[Column<T>]) -> bool {
    matches!(columns, [column] if column.windows.len() == 1)
}

fn tiled_gap(tiled: usize, gaps: i32, smart_gaps: bool) -> i32 {
    if smart_gaps && tiled == 1 { 0 } else { gaps }
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

/// Where each tiled window of a non-scrolling workspace goes, in order.
fn layout_rectangles(
    workspace: &Workspace,
    area: Rectangle<i32, Logical>,
    count: usize,
    requested_gap: i32,
    config: &LayoutConfig,
) -> Vec<Rectangle<i32, Logical>> {
    if count == 0 {
        return Vec::new();
    }
    let gap = effective_gap(requested_gap, area.size.w, area.size);
    let inner = inset_rectangle(area, gap);
    match workspace.layout {
        LayoutMode::Scrolling => Vec::new(),
        LayoutMode::Monocle if config.monocle.gaps => vec![inner; count],
        LayoutMode::Monocle => vec![area; count],
        LayoutMode::MasterStack => {
            let master_count = workspace.effective_master_count(count, &config.master_stack);
            let weights = &workspace.master_weights;
            let frame = MasterFrame::new(inner, config.master_stack.position);
            let inner = frame.area();
            let rectangles = if master_count == count {
                weighted_rows(inner, count, gap, &weights[..count.min(weights.len())])
            } else {
                let ratio = workspace.master_ratio(&config.master_stack);
                let (master, stack) = split_rectangle(inner, true, ratio, gap);
                let mut rectangles = weighted_rows(
                    master,
                    master_count,
                    gap,
                    &weights[..master_count.min(weights.len())],
                );
                rectangles.extend(weighted_rows(
                    stack,
                    count - master_count,
                    gap,
                    weights.get(master_count..).unwrap_or_default(),
                ));
                rectangles
            };
            rectangles
                .into_iter()
                .map(|rectangle| frame.to_output(rectangle))
                .collect()
        }
        LayoutMode::Dwindle => {
            let cuts = dwindle_cuts(
                inner,
                count,
                gap,
                &workspace.dwindle_ratios,
                &config.dwindle,
            );
            let last = cuts.last().map_or(inner, |cut| cut.rest);
            cuts.iter()
                .map(|cut| cut.first)
                .chain(std::iter::once(last))
                .collect()
        }
    }
}

/// master_stack is laid out as if the master area were on the left, then
/// turned to `layout.master_stack.position`. Placement, drag-resizing and
/// directional focus all go through this one mapping, so the layout itself
/// only has to be right once.
#[derive(Clone, Copy)]
struct MasterFrame {
    area: Rectangle<i32, Logical>,
    position: MasterPosition,
}

impl MasterFrame {
    fn new(area: Rectangle<i32, Logical>, position: MasterPosition) -> Self {
        Self { area, position }
    }

    /// The area in the frame: the output's, turned on its side when the master
    /// area is above or below the stack.
    fn area(self) -> Rectangle<i32, Logical> {
        Rectangle::new(self.area.loc, self.frame_size(self.area.size))
    }

    fn frame_size(self, size: Size<i32, Logical>) -> Size<i32, Logical> {
        if self.position.transposed() {
            (size.h, size.w).into()
        } else {
            size
        }
    }

    fn to_output(self, rectangle: Rectangle<i32, Logical>) -> Rectangle<i32, Logical> {
        let frame_width = self.area().size.w;
        let mut x = rectangle.loc.x - self.area.loc.x;
        let mut y = rectangle.loc.y - self.area.loc.y;
        if self.position.mirrored() {
            x = frame_width - x - rectangle.size.w;
        }
        if self.position.transposed() {
            std::mem::swap(&mut x, &mut y);
        }
        Rectangle::new(
            (self.area.loc.x + x, self.area.loc.y + y).into(),
            self.frame_size(rectangle.size),
        )
    }

    fn to_frame(self, rectangle: Rectangle<i32, Logical>) -> Rectangle<i32, Logical> {
        let mut x = rectangle.loc.x - self.area.loc.x;
        let mut y = rectangle.loc.y - self.area.loc.y;
        let size = self.frame_size(rectangle.size);
        if self.position.transposed() {
            std::mem::swap(&mut x, &mut y);
        }
        if self.position.mirrored() {
            x = self.area().size.w - x - size.w;
        }
        Rectangle::new((self.area.loc.x + x, self.area.loc.y + y).into(), size)
    }

    fn point_to_frame(self, point: Point<f64, Logical>) -> Point<f64, Logical> {
        let mut x = point.x - f64::from(self.area.loc.x);
        let mut y = point.y - f64::from(self.area.loc.y);
        if self.position.transposed() {
            std::mem::swap(&mut x, &mut y);
        }
        if self.position.mirrored() {
            x = f64::from(self.area().size.w) - x;
        }
        (
            f64::from(self.area.loc.x) + x,
            f64::from(self.area.loc.y) + y,
        )
            .into()
    }
}

/// One dwindle split: the window it makes room for, and what is left.
#[derive(Clone, Copy, Debug, PartialEq)]
struct DwindleCut {
    first: Rectangle<i32, Logical>,
    rest: Rectangle<i32, Logical>,
    /// Side by side rather than one above the other.
    vertical: bool,
    /// The length the split divides, for turning a drag into a ratio.
    length: i32,
}

/// Every split of a dwindle layout of `count` windows, in order. The one place
/// that decides which way a split cuts, for laying out, dragging and keyboard
/// resizing alike.
fn dwindle_cuts(
    inner: Rectangle<i32, Logical>,
    count: usize,
    gap: i32,
    ratios: &[Option<f64>],
    config: &DwindleConfig,
) -> Vec<DwindleCut> {
    let mut remaining = inner;
    let mut cuts = Vec::with_capacity(count.saturating_sub(1));
    for index in 0..count.saturating_sub(1) {
        let vertical = match config.split {
            DwindleSplit::Alternate => index % 2 == 0,
            DwindleSplit::Longest => remaining.size.w >= remaining.size.h,
        };
        let ratio = ratios
            .get(index)
            .copied()
            .flatten()
            .unwrap_or(config.split_ratio);
        let (first, rest) = split_rectangle(remaining, vertical, ratio, gap);
        cuts.push(DwindleCut {
            first,
            rest,
            vertical,
            length: split_length(remaining, vertical, gap),
        });
        remaining = rest;
    }
    cuts
}

fn inset_rectangle(rectangle: Rectangle<i32, Logical>, amount: i32) -> Rectangle<i32, Logical> {
    Rectangle::new(
        (rectangle.loc.x + amount, rectangle.loc.y + amount).into(),
        (rectangle.size.w - amount * 2, rectangle.size.h - amount * 2).into(),
    )
}

fn split_length(rectangle: Rectangle<i32, Logical>, vertical: bool, requested_gap: i32) -> i32 {
    let length = if vertical {
        rectangle.size.w
    } else {
        rectangle.size.h
    };
    length - requested_gap.min((length - 2).max(0))
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct DividerHit {
    split: usize,
    vertical: bool,
    length: i32,
    distance: f64,
}

fn divider_distance(
    pointer: Point<f64, Logical>,
    first: Rectangle<i32, Logical>,
    rest: Rectangle<i32, Logical>,
    vertical: bool,
    margin: i32,
) -> Option<f64> {
    let (position, first_edge, rest_edge, cross, cross_start, cross_end) = if vertical {
        (
            pointer.x,
            first.loc.x + first.size.w,
            rest.loc.x,
            pointer.y,
            first.loc.y,
            first.loc.y + first.size.h,
        )
    } else {
        (
            pointer.y,
            first.loc.y + first.size.h,
            rest.loc.y,
            pointer.x,
            first.loc.x,
            first.loc.x + first.size.w,
        )
    };
    if cross < f64::from(cross_start) || cross >= f64::from(cross_end) {
        return None;
    }
    let distance = (position - f64::from(first_edge))
        .abs()
        .min((position - f64::from(rest_edge)).abs());
    (distance <= f64::from(margin)).then_some(distance)
}

#[cfg(test)]
fn dwindle_divider_at(
    inner: Rectangle<i32, Logical>,
    count: usize,
    gap: i32,
    ratios: &[Option<f64>],
    pointer: Point<f64, Logical>,
    margin: i32,
) -> Option<DividerHit> {
    let cuts = dwindle_cuts(inner, count, gap, ratios, &DwindleConfig::default());
    let (vertical, horizontal) = dwindle_dividers_at(&cuts, pointer, margin);
    [vertical, horizontal]
        .into_iter()
        .flatten()
        .min_by(|left, right| left.distance.total_cmp(&right.distance))
}

fn dwindle_dividers_at(
    cuts: &[DwindleCut],
    pointer: Point<f64, Logical>,
    margin: i32,
) -> (Option<DividerHit>, Option<DividerHit>) {
    let mut vertical_hit = None::<DividerHit>;
    let mut horizontal_hit = None::<DividerHit>;
    for (split, cut) in cuts.iter().enumerate() {
        let vertical = cut.vertical;
        if let Some(distance) = divider_distance(pointer, cut.first, cut.rest, vertical, margin) {
            let hit = DividerHit {
                split,
                vertical,
                length: cut.length,
                distance,
            };
            let closest = if vertical {
                &mut vertical_hit
            } else {
                &mut horizontal_hit
            };
            if closest.is_none_or(|current| distance < current.distance) {
                *closest = Some(hit);
            }
        }
    }
    (vertical_hit, horizontal_hit)
}

fn split_rectangle(
    rectangle: Rectangle<i32, Logical>,
    vertical: bool,
    ratio: f64,
    requested_gap: i32,
) -> (Rectangle<i32, Logical>, Rectangle<i32, Logical>) {
    let length = if vertical {
        rectangle.size.w
    } else {
        rectangle.size.h
    };
    if length <= 1 {
        return (rectangle, rectangle);
    }
    let gap = requested_gap.min((length - 2).max(0));
    let available = length - gap;
    let first =
        ((f64::from(available) * ratio.clamp(0.1, 0.9)).round() as i32).clamp(1, available - 1);
    if vertical {
        (
            Rectangle::new(rectangle.loc, (first, rectangle.size.h).into()),
            Rectangle::new(
                (rectangle.loc.x + first + gap, rectangle.loc.y).into(),
                (available - first, rectangle.size.h).into(),
            ),
        )
    } else {
        (
            Rectangle::new(rectangle.loc, (rectangle.size.w, first).into()),
            Rectangle::new(
                (rectangle.loc.x, rectangle.loc.y + first + gap).into(),
                (rectangle.size.w, available - first).into(),
            ),
        )
    }
}

/// Split `available` pixels across `weights`, giving every row at least
/// `minimum` and handing the rounding remainder to the last one.
///
/// Shared by the two row layouts, which differ only in how many gaps they take
/// out first and what they wrap the heights in. Weights that do not add up to
/// anything positive - all zero, or a caller that never set them - fall back to
/// an even split rather than dividing by zero.
fn distribute(available: i32, weights: &[f64], minimum: i32) -> Vec<i32> {
    let count = weights.len() as i32;
    if count == 0 {
        return Vec::new();
    }
    let distributable = available - minimum * count;
    let total = weights.iter().map(|weight| weight.max(0.0)).sum::<f64>();
    let equal = total <= 0.0;
    let total = if equal { f64::from(count) } else { total };
    let mut cumulative = 0.0;
    let mut distributed = 0;
    weights
        .iter()
        .enumerate()
        .map(|(row, weight)| {
            cumulative += if equal { 1.0 } else { weight.max(0.0) };
            let target = if row + 1 == weights.len() {
                distributable
            } else {
                (f64::from(distributable) * cumulative / total).round() as i32
            };
            let height = minimum + target - distributed;
            distributed = target;
            height
        })
        .collect()
}

fn weighted_rows(
    rectangle: Rectangle<i32, Logical>,
    count: usize,
    requested_gap: i32,
    weights: &[f64],
) -> Vec<Rectangle<i32, Logical>> {
    if count == 0 {
        return Vec::new();
    }
    let rows = count as i32;
    // Gaps sit between the rows only; the caller owns the outer margin.
    let gap = if rows == 1 {
        0
    } else {
        requested_gap.min(((rectangle.size.h - rows) / (rows - 1)).max(0))
    };
    let available = (rectangle.size.h - gap * (rows - 1)).max(rows);
    let minimum = MIN_ROW_HEIGHT.min(available / rows).max(1);
    let weights = (0..count)
        .map(|index| weights.get(index).copied().unwrap_or(1.0))
        .collect::<Vec<_>>();
    let mut y = rectangle.loc.y;
    distribute(available, &weights, minimum)
        .into_iter()
        .map(|height| {
            let result = Rectangle::new(
                (rectangle.loc.x, y).into(),
                (rectangle.size.w, height).into(),
            );
            y += height + gap;
            result
        })
        .collect()
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

/// Rows stacked down an output, with a gap above the first and below the last
/// as well as between them. Only y and height are filled in; the caller places
/// x and width.
fn stacked_rows(
    output_y: i32,
    output_height: i32,
    weights: &[f64],
    requested_gap: i32,
    requested_minimum: i32,
) -> Vec<Rectangle<i32, Logical>> {
    if weights.is_empty() {
        return Vec::new();
    }
    let count = weights.len() as i32;
    let gap = requested_gap.min(((output_height - count) / (count + 1)).max(0));
    let available = (output_height - gap * (count + 1)).max(count);
    let minimum = requested_minimum.min(available / count).max(1);
    let mut y = output_y + gap;
    distribute(available, weights, minimum)
        .into_iter()
        .map(|row_height| {
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
    fn a_bar_comes_out_of_the_top_of_its_tile() {
        let tile = Rectangle::new((10, 20).into(), (400, 300).into());
        let client = bar_client_rectangle(tile, 28);
        assert_eq!(client, Rectangle::new((10, 48).into(), (400, 272).into()));
        assert_eq!(bar_outer_rectangle(client, 28), tile);
        // A tile shorter than its bar still leaves the window a pixel.
        let squashed = bar_client_rectangle(Rectangle::new((0, 0).into(), (100, 10).into()), 28);
        assert_eq!(squashed.size.h, 1);
    }

    #[test]
    fn titlebars_set_the_bar_height_and_make_room_for_buttons() {
        let mut config = crate::config::defaults();
        assert_eq!(bar_height(&config), TAB_BAR_HEIGHT);
        config.appearance.titlebars = true;
        config.appearance.titlebar_height = 30;
        assert_eq!(bar_height(&config), 30);
        assert_eq!(bar_buttons_width(true, 30), 90);
        assert_eq!(bar_button_at(0.0, 30), BarPart::Minimize);
        assert_eq!(bar_button_at(45.0, 30), BarPart::Float);
        assert_eq!(bar_button_at(89.9, 30), BarPart::Close);
        assert_eq!(bar_buttons_width(false, 30), 0);
    }

    #[test]
    fn tab_hit_selects_each_segment_and_clamps_the_far_edge() {
        assert_eq!(tab_index_at(0.0, 300, 3), Some(0));
        assert_eq!(tab_index_at(99.9, 300, 3), Some(0));
        assert_eq!(tab_index_at(100.0, 300, 3), Some(1));
        assert_eq!(tab_index_at(200.0, 300, 3), Some(2));
        assert_eq!(tab_index_at(300.0, 300, 3), Some(2));
        assert_eq!(tab_index_at(0.0, 300, 0), None);
    }

    #[test]
    fn overview_filters_orders_and_centers_workspaces() {
        let visible = visible_workspace_indices(
            [false, true, false, false, false, true, false, false, false].into_iter(),
            3,
        );
        assert_eq!(visible, [1, 3, 5]);
        assert_eq!(wrapped_selection(&visible, 1, -1), Some(5));
        assert_eq!(wrapped_selection(&visible, 5, 1), Some(1));

        // Closed, the centred card is exactly the output: that seam is what
        // lets the overview animate out of the live desktop without a jump.
        let output: Size<i32, Logical> = (1920, 1080).into();
        let closed = overview_cards(output, &visible, 1.0, 0.0, 0.5);
        assert_eq!(closed[1].0, 3);
        assert_eq!(closed[1].1, Rectangle::from_size(output));
        assert_eq!(overview_zoom(0.0, 0.5), 1.0);

        // Open, cards keep the output's proportions and sit centred.
        let cards = overview_cards(output, &visible, 1.0, 1.0, 0.5);
        assert_eq!(
            cards[1].1,
            Rectangle::new((480, 270).into(), (960, 540).into())
        );
        assert!(cards[0].1.loc.y < cards[1].1.loc.y);
        assert!(cards[2].1.loc.y > cards[1].1.loc.y);
        assert_eq!(cards[1].1.loc.y - cards[0].1.loc.y, 540 + OVERVIEW_GAP);
        assert_eq!(overview_zoom(1.0, 0.5), 0.5);

        // Centring follows the fractional scroll position.
        let scrolled = overview_cards(output, &visible, 2.0, 1.0, 0.5);
        assert_eq!(scrolled[2].1.loc.y, 270);
        let halfway = overview_cards(output, &visible, 1.5, 1.0, 0.5);
        assert!(halfway[1].1.loc.y < 270 && halfway[2].1.loc.y > 270);

        // A window lands inside its card at the card's own zoom.
        assert_eq!(
            overview_screen_rect(
                Rectangle::new((960, 540).into(), (960, 540).into()),
                cards[1].1,
                0.5,
            ),
            Rectangle::new((960, 540).into(), (480, 270).into())
        );
    }

    #[test]
    fn columns_are_placed_the_same_for_the_desktop_and_the_preview() {
        let columns = [
            Column {
                windows: vec![1, 2],
                row_weights: vec![1.0, 1.0],
                focused: 0,
                width: Some(0.5),
            },
            Column {
                windows: vec![3],
                row_weights: vec![1.0],
                focused: 0,
                width: Some(0.5),
            },
        ];
        let area = Rectangle::new((0, 0).into(), (1000, 600).into());
        let (gap, widths) = column_widths(&columns, area.size, 0.6, 15, false);
        assert_eq!(gap, 15);
        assert_eq!(widths, vec![493, 493]);

        // Both callers pass the workspace's own offset, so an unscrolled and a
        // scrolled viewport differ by exactly that offset and nothing else.
        let resting = place_columns(&columns, area, &widths, gap, 0);
        assert_eq!(resting.len(), 3);
        assert_eq!(resting[0].1.loc.x, gap);
        assert_eq!(resting[0].1.size.w, 493 - gap);
        assert_eq!(resting[2].1.loc.x, gap + 493);
        // Two rows stack inside the first column, one fills the second.
        assert!(resting[1].1.loc.y > resting[0].1.loc.y);
        assert_eq!(resting[0].1.loc.x, resting[1].1.loc.x);
        assert!(resting[2].1.size.h > resting[0].1.size.h);

        let scrolled = place_columns(&columns, area, &widths, gap, 200);
        for (rested, scrolled) in resting.iter().zip(&scrolled) {
            assert_eq!(scrolled.1.loc.x, rested.1.loc.x - 200);
            assert_eq!(scrolled.1.size, rested.1.size);
        }
    }

    #[test]
    fn tween_eases_between_its_ends_and_reverses_from_where_it_stands() {
        let now = Duration::from_millis(500);
        let duration = Some(Duration::from_millis(200));
        let mut tween = Tween::default();
        assert_eq!(tween.value(now), 0.0);
        assert!(tween.finished(now));

        tween.retarget(1.0, now, duration);
        assert_eq!(tween.value(now), 0.0);
        assert!(!tween.finished(now));
        let middle = tween.value(now + Duration::from_millis(100));
        assert!(middle > 0.0 && middle < 1.0);
        // Absolute time, so an overshooting read still lands exactly on the end
        // rather than leaving the overview a hair short of open.
        assert_eq!(tween.value(now + Duration::from_millis(200)), 1.0);
        assert_eq!(tween.value(now + Duration::from_secs(9)), 1.0);
        assert!(tween.finished(now + Duration::from_millis(200)));

        // Re-aiming at the same target must not restart the animation.
        let later = now + Duration::from_millis(100);
        tween.retarget(1.0, later, duration);
        assert_eq!(tween.value(later), middle);

        // Reversing mid-flight resumes from what is on screen.
        tween.retarget(0.0, later, duration);
        assert_eq!(tween.value(later), middle);
        assert_eq!(tween.value(later + Duration::from_millis(200)), 0.0);

        // No duration means no animation, the same as everything else here.
        tween.retarget(1.0, later, None);
        assert_eq!(tween.value(later), 1.0);
        assert!(tween.finished(later));
    }

    #[test]
    fn overview_navigates_columns_rows_and_floating_windows() {
        let columns = [
            Column {
                windows: vec![1, 2],
                row_weights: vec![1.0, 1.0],
                focused: 1,
                width: None,
            },
            Column {
                windows: vec![3, 4],
                row_weights: vec![1.0, 1.0],
                focused: 0,
                width: None,
            },
        ];
        let floating = [5, 6];

        assert_eq!(column_neighbor(&columns, &floating, &1, 1), Some(3));
        assert_eq!(column_neighbor(&columns, &floating, &3, -1), Some(2));
        assert_eq!(column_neighbor(&columns, &floating, &4, 1), Some(5));
        assert_eq!(column_neighbor(&columns, &floating, &5, 1), Some(6));
        assert_eq!(column_neighbor(&columns, &floating, &6, 1), None);
        assert_eq!(row_neighbor(&columns, &1, 1), Some(2));
        assert_eq!(row_neighbor(&columns, &2, 1), None);
        assert_eq!(row_neighbor(&columns, &5, 1), None);

        // A column whose `focused` index has gone stale offers nothing, rather
        // than falling through to the first floating window.
        let stale = [Column {
            windows: vec![1],
            row_weights: vec![1.0],
            focused: 7,
            width: None,
        }];
        assert_eq!(column_neighbor(&stale, &floating, &5, -1), None);
    }

    #[test]
    fn overview_pans_only_far_enough_to_reveal_the_selected_column() {
        // Six half-width columns on a 1000px area. `column_width` makes each 493
        // wide with a 15px gap, so the visible span is 970 and a column occupies
        // 478 of it: two columns overhang it by a single pixel.
        let columns = (0..6)
            .map(|index| Column {
                windows: vec![index],
                row_weights: vec![1.0],
                focused: 0,
                width: Some(0.5),
            })
            .collect::<Vec<_>>();
        let area: Size<i32, Logical> = (1000, 600).into();
        let pan = |column: Option<usize>, current: i32| {
            overview_pan_target(&columns, area, column, current, 0.5, 15, false, false)
        };

        // The first column is already on the card, so the view does not move.
        assert_eq!(pan(Some(0), 0), 0);
        // Anything further is revealed by exactly what it overhangs by, never
        // more: column 2 starts at 2*493 and needs 478 of the 970 visible.
        assert_eq!(pan(Some(2), 0), 493 * 2 + 478 - 970);
        // The last column pins the view at its far end.
        let far = pan(Some(5), 0);
        assert_eq!(far, 493 * 5 + 478 - 970);
        assert!(far > pan(Some(2), 0));
        assert_eq!(pan(Some(5), far), far);
        // Coming back scrolls the other way, and only to the column's own edge.
        assert_eq!(pan(Some(1), far), 493);
        // A floating selection has no column and leaves the pan where it is.
        assert_eq!(pan(None, far), far);
        // `center_focused_column` is honoured.
        assert_eq!(
            overview_pan_target(&columns, area, Some(3), 0, 0.5, 15, false, true),
            493 * 3 - (970 - 478) / 2
        );

        // Three narrower columns fit the card outright, so nothing pans and the
        // preview stays equal to the real desktop - the common case.
        let fitting = (0..3)
            .map(|index| Column {
                windows: vec![index],
                row_weights: vec![1.0],
                focused: 0,
                width: Some(0.3),
            })
            .collect::<Vec<_>>();
        for column in 0..3 {
            assert_eq!(
                overview_pan_target(&fitting, area, Some(column), 0, 0.3, 15, false, false),
                0
            );
        }
    }

    #[test]
    fn navigation_viewport_and_monitor_direction() {
        assert!(should_reveal_output(true, Some("DP-1"), "DP-1"));
        assert!(!should_reveal_output(true, Some("DP-1"), "HDMI-A-1"));
        assert!(!should_reveal_output(false, Some("DP-1"), "DP-1"));
        assert!(!should_reveal_output(true, None, "DP-1"));
        assert_eq!(column_insertion_index(Some(0), 3), 1);
        assert_eq!(column_insertion_index(Some(2), 3), 3);
        assert_eq!(column_insertion_index(None, 3), 3);
        assert_eq!(adjusted_column_width(0.5, 0.05), 0.55);
        assert_eq!(adjusted_column_width(0.1, -0.05), 0.1);
        assert_eq!(adjusted_column_width(1.0, 0.05), 1.0);
        let geometry = Rectangle::new((100, 20).into(), (400, 300).into());
        assert_eq!(
            column_edge(105.0, geometry, 8),
            Some((ColumnEdge::Left, 5.0))
        );
        assert_eq!(
            column_edge(495.0, geometry, 8),
            Some((ColumnEdge::Right, 5.0))
        );
        assert_eq!(column_edge(300.0, geometry, 8), None);
        assert_eq!(row_boundary(22.0, geometry, 1, 3, 8), Some((0, 2.0)));
        assert_eq!(row_boundary(318.0, geometry, 1, 3, 8), Some((1, 2.0)));
        assert_eq!(row_boundary(170.0, geometry, 1, 3, 8), None);
        assert_eq!(insertion_index(&[100, 300, 500], 50.0), 0);
        assert_eq!(insertion_index(&[100, 300, 500], 350.0), 2);
        assert_eq!(insertion_index(&[100, 300, 500], 550.0), 3);
        let (width, offset) = resized_column(0.5, 100, ColumnEdge::Left, 100.0, 1000, 10);
        assert!(width < 0.5);
        assert!(offset < 100);
        let (width, offset) = resized_column(0.5, 100, ColumnEdge::Right, 100.0, 1000, 10);
        assert!(width > 0.5);
        assert_eq!(offset, 100);
        assert_eq!(
            constrain_window(
                Rectangle::new((100, 50).into(), (500, 400).into()),
                (200, 100).into(),
                (550, 500).into(),
            ),
            (400, 350).into()
        );
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
            stacked_rows(50, 100, &[1.0, 1.0], 10, MIN_ROW_HEIGHT),
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
            row_weights: vec![1.0; 3],
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
    fn layouts_cycle_per_workspace_and_honor_enabled_list() {
        let enabled = [
            LayoutMode::Scrolling,
            LayoutMode::Dwindle,
            LayoutMode::Monocle,
        ];
        let mut first = Workspace::default();
        let second = Workspace::default();
        assert!(first.cycle_layout(&enabled));
        assert_eq!(first.layout, LayoutMode::Dwindle);
        assert_eq!(second.layout, LayoutMode::Scrolling);
        assert!(!first.switch_layout(LayoutMode::MasterStack, &enabled));
        assert!(first.switch_layout(LayoutMode::Monocle, &enabled));
        assert!(first.cycle_layout(&enabled));
        assert_eq!(first.layout, LayoutMode::Scrolling);
    }

    /// `layout_rectangles` for a workspace with these adjustments, under the
    /// default `[layout]`.
    fn tiled_rectangles(
        layout: LayoutMode,
        area: Rectangle<i32, Logical>,
        count: usize,
        gap: i32,
        master_count: usize,
        master_ratio: f64,
        master_weights: &[f64],
        dwindle_ratios: &[f64],
    ) -> Vec<Rectangle<i32, Logical>> {
        let workspace = Workspace {
            layout,
            master_count: Some(master_count),
            master_ratio: Some(master_ratio),
            master_weights: master_weights.to_vec(),
            dwindle_ratios: dwindle_ratios.iter().copied().map(Some).collect(),
            ..Workspace::default()
        };
        layout_rectangles(&workspace, area, count, gap, &LayoutConfig::default())
    }

    #[test]
    fn alternate_layout_rectangles_are_stable_and_positive() {
        let area = Rectangle::new((0, 0).into(), (1000, 800).into());
        let monocle = tiled_rectangles(LayoutMode::Monocle, area, 3, 10, 1, 0.6, &[], &[]);
        assert_eq!(
            monocle,
            vec![Rectangle::new((10, 10).into(), (980, 780).into()); 3]
        );

        let master = tiled_rectangles(LayoutMode::MasterStack, area, 3, 10, 1, 0.6, &[], &[]);
        assert_eq!(
            master[0],
            Rectangle::new((10, 10).into(), (582, 780).into())
        );
        assert_eq!(
            master[1],
            Rectangle::new((602, 10).into(), (388, 385).into())
        );
        assert_eq!(
            master[2],
            Rectangle::new((602, 405).into(), (388, 385).into())
        );

        let dwindle = tiled_rectangles(LayoutMode::Dwindle, area, 4, 10, 1, 0.6, &[], &[]);
        assert_eq!(
            dwindle[0],
            Rectangle::new((10, 10).into(), (485, 780).into())
        );
        assert_eq!(
            dwindle[1],
            Rectangle::new((505, 10).into(), (485, 385).into())
        );
        assert_eq!(
            dwindle[2],
            Rectangle::new((505, 405).into(), (238, 385).into())
        );
        assert_eq!(
            dwindle[3],
            Rectangle::new((753, 405).into(), (237, 385).into())
        );

        let tiny = tiled_rectangles(
            LayoutMode::Dwindle,
            Rectangle::new((0, 0).into(), (2, 2).into()),
            8,
            15,
            1,
            0.6,
            &[],
            &[],
        );
        assert!(
            tiny.iter()
                .all(|rectangle| rectangle.size.w > 0 && rectangle.size.h > 0)
        );
    }

    #[test]
    fn dwindle_navigation_follows_geometry() {
        let geometries = tiled_rectangles(
            LayoutMode::Dwindle,
            Rectangle::new((0, 0).into(), (1000, 800).into()),
            4,
            10,
            1,
            0.6,
            &[],
            &[],
        );
        assert_eq!(
            directional_index(0, Direction::Left, &geometries, true),
            None
        );
        assert_eq!(
            directional_index(0, Direction::Right, &geometries, true),
            Some(2)
        );
        assert_eq!(
            directional_index(0, Direction::Up, &geometries, true),
            Some(1)
        );
        assert_eq!(
            directional_index(0, Direction::Down, &geometries, true),
            Some(2)
        );
        assert_eq!(
            directional_index(1, Direction::Left, &geometries, true),
            Some(0)
        );
        assert_eq!(
            directional_index(2, Direction::Left, &geometries, true),
            Some(0)
        );
        assert_eq!(
            directional_index(2, Direction::Right, &geometries, true),
            Some(3)
        );
        assert_eq!(
            directional_index(2, Direction::Up, &geometries, true),
            Some(1)
        );
        assert_eq!(
            directional_index(2, Direction::Down, &geometries, true),
            None
        );

        let diagonal = [
            Rectangle::new((100, 100).into(), (50, 50).into()),
            Rectangle::new((0, 0).into(), (50, 50).into()),
        ];
        assert_eq!(
            directional_index(0, Direction::Left, &diagonal, true),
            Some(1)
        );
    }

    #[test]
    fn alternate_layout_ratios_resize_expected_splits() {
        let area = Rectangle::new((0, 0).into(), (1000, 800).into());
        let master = tiled_rectangles(
            LayoutMode::MasterStack,
            area,
            3,
            10,
            1,
            0.6,
            &[1.0, 3.0, 1.0],
            &[],
        );
        assert_eq!(
            master[1],
            Rectangle::new((602, 10).into(), (388, 554).into())
        );
        assert_eq!(
            master[2],
            Rectangle::new((602, 574).into(), (388, 216).into())
        );

        let dwindle = tiled_rectangles(
            LayoutMode::Dwindle,
            area,
            4,
            10,
            1,
            0.6,
            &[],
            &[0.7, 0.25, 0.6],
        );
        assert_eq!(
            dwindle[0],
            Rectangle::new((10, 10).into(), (679, 780).into())
        );
        assert_eq!(
            dwindle[1],
            Rectangle::new((699, 10).into(), (291, 193).into())
        );
        assert_eq!(
            dwindle[2],
            Rectangle::new((699, 213).into(), (169, 577).into())
        );
        assert_eq!(
            dwindle[3],
            Rectangle::new((878, 213).into(), (112, 577).into())
        );

        assert_eq!(adjusted_split_ratio(0.5, 1.0), 0.9);
        assert_eq!(adjusted_split_ratio(0.5, -1.0), 0.1);
        let alternate = [true, false, true];
        assert_eq!(dwindle_split(0, 4, true, &alternate), Some(0));
        assert_eq!(dwindle_split(0, 4, false, &alternate), None);
        assert_eq!(dwindle_split(3, 4, true, &alternate), Some(2));
        assert_eq!(dwindle_split(3, 4, false, &alternate), Some(1));
    }

    #[test]
    fn mouse_hits_only_internal_layout_dividers() {
        let inner = Rectangle::new((10, 10).into(), (980, 780).into());
        let ratios = [Some(0.7), Some(0.25), Some(0.6)];
        assert_eq!(
            dwindle_divider_at(inner, 4, 10, &ratios, (689.0, 100.0).into(), 8),
            Some(DividerHit {
                split: 0,
                vertical: true,
                length: 970,
                distance: 0.0,
            })
        );
        assert_eq!(
            dwindle_divider_at(inner, 4, 10, &ratios, (800.0, 203.0).into(), 8)
                .map(|hit| hit.split),
            Some(1)
        );
        assert_eq!(
            dwindle_divider_at(inner, 4, 10, &ratios, (10.0, 100.0).into(), 8),
            None
        );
        let cuts = dwindle_cuts(inner, 4, 10, &[], &DwindleConfig::default());
        let (vertical, horizontal) = dwindle_dividers_at(&cuts, (505.0, 405.0).into(), 8);
        assert!(vertical.is_some() && horizontal.is_some());
        let (master, stack) = split_rectangle(inner, true, 0.6, 10);
        assert_eq!(
            divider_distance((592.0, 100.0).into(), master, stack, true, 8),
            Some(0.0)
        );
        assert_eq!(
            divider_distance((10.0, 100.0).into(), master, stack, true, 8),
            None
        );
    }

    #[test]
    fn layout_resize_state_is_per_workspace_and_tracks_tile_count() {
        let mut first = Workspace::default();
        let second = Workspace::default();
        first.master_count = Some(2);
        first.master_ratio = Some(0.7);
        first.dwindle_ratios = vec![Some(0.6), Some(0.4)];
        first.sync_layout_sizes(4);
        assert_eq!(first.master_ratio, Some(0.7));
        // A new split follows `layout.dwindle.split_ratio` until it is resized.
        assert_eq!(first.dwindle_ratios, [Some(0.6), Some(0.4), None]);
        assert_eq!(first.master_weights, [1.0, 1.0, 1.0, 1.0]);
        first.sync_layout_sizes(2);
        assert_eq!(first.dwindle_ratios, [Some(0.6)]);
        assert_eq!(second.master_ratio, None);
        assert!(second.dwindle_ratios.is_empty());
        let mut restored = Workspace::default();
        restored.adopt_layout_state(&first);
        assert_eq!(restored.master_count, Some(2));
        assert_eq!(restored.master_ratio, Some(0.7));
        assert_eq!(restored.dwindle_ratios, [Some(0.6)]);
    }

    #[test]
    fn master_count_and_spatial_neighbors_are_bounded() {
        let area = Rectangle::new((0, 0).into(), (1000, 800).into());
        let split = tiled_rectangles(LayoutMode::MasterStack, area, 4, 10, 2, 0.6, &[], &[]);
        assert_eq!(split[0], Rectangle::new((10, 10).into(), (582, 385).into()));
        assert_eq!(
            split[1],
            Rectangle::new((10, 405).into(), (582, 385).into())
        );
        assert_eq!(
            split[2],
            Rectangle::new((602, 10).into(), (388, 385).into())
        );

        let all_master = tiled_rectangles(LayoutMode::MasterStack, area, 4, 10, 4, 0.6, &[], &[]);
        assert!(all_master.iter().all(|rectangle| rectangle.loc.x == 10));
        assert!(all_master.iter().all(|rectangle| rectangle.size.w == 980));

        let geometries = [
            Rectangle::new((0, 0).into(), (600, 100).into()),
            Rectangle::new((0, 100).into(), (600, 100).into()),
            Rectangle::new((600, 0).into(), (400, 60).into()),
            Rectangle::new((600, 60).into(), (400, 60).into()),
            Rectangle::new((600, 120).into(), (400, 60).into()),
        ];
        assert_eq!(master_neighbor(1, 2, true, 1, &geometries), Some(4));
        assert_eq!(master_neighbor(3, 2, true, -1, &geometries), Some(0));
        assert_eq!(master_neighbor(0, 2, true, -1, &geometries), None);
        assert_eq!(master_neighbor(2, 2, true, 1, &geometries), None);
        assert_eq!(master_neighbor(2, 2, false, 1, &geometries), Some(3));
    }

    #[test]
    fn smart_gaps_drop_only_a_lone_windows_gaps() {
        assert_eq!(tiled_gap(1, 15, false), 15);
        assert_eq!(tiled_gap(1, 15, true), 0);
        assert_eq!(tiled_gap(2, 15, true), 15);
        assert_eq!(tiled_gap(0, 15, true), 15);
    }

    #[test]
    fn a_lone_window_can_take_the_full_width() {
        let column = |windows: Vec<u32>| Column {
            row_weights: vec![1.0; windows.len()],
            windows,
            focused: 0,
            width: Some(0.5),
        };
        let area = Size::from((1000, 600));

        let lone = [column(vec![1])];
        assert_eq!(column_widths(&lone, area, 0.6, 0, false).1, vec![500]);
        assert_eq!(column_widths(&lone, area, 0.6, 0, true).1, vec![1000]);

        // A second window, in its own column or the same one, ends it.
        let two_columns = [column(vec![1]), column(vec![2])];
        assert_eq!(
            column_widths(&two_columns, area, 0.6, 0, true).1,
            vec![500, 500]
        );
        let stacked = [column(vec![1, 2])];
        assert_eq!(column_widths(&stacked, area, 0.6, 0, true).1, vec![500]);
    }

    #[test]
    fn layout_options_shape_each_layout() {
        let area = Rectangle::new((0, 0).into(), (1000, 800).into());
        let workspace = |layout| Workspace {
            layout,
            ..Workspace::default()
        };

        // master_stack: the config's ratio and count, on the configured side.
        let mut config = LayoutConfig::default();
        config.master_stack.master_ratio = 0.5;
        config.master_stack.master_count = 2;
        let left = layout_rectangles(&workspace(LayoutMode::MasterStack), area, 3, 10, &config);
        assert_eq!(left[0], Rectangle::new((10, 10).into(), (485, 385).into()));
        assert_eq!(left[1], Rectangle::new((10, 405).into(), (485, 385).into()));
        assert_eq!(left[2], Rectangle::new((505, 10).into(), (485, 780).into()));
        config.master_stack.position = MasterPosition::Right;
        let right = layout_rectangles(&workspace(LayoutMode::MasterStack), area, 3, 10, &config);
        assert_eq!(
            right[0],
            Rectangle::new((505, 10).into(), (485, 385).into())
        );
        assert_eq!(right[2], Rectangle::new((10, 10).into(), (485, 780).into()));
        config.master_stack.position = MasterPosition::Top;
        let top = layout_rectangles(&workspace(LayoutMode::MasterStack), area, 3, 10, &config);
        assert_eq!(top[0], Rectangle::new((10, 10).into(), (485, 385).into()));
        assert_eq!(top[1], Rectangle::new((505, 10).into(), (485, 385).into()));
        assert_eq!(top[2], Rectangle::new((10, 405).into(), (980, 385).into()));
        config.master_stack.position = MasterPosition::Bottom;
        let bottom = layout_rectangles(&workspace(LayoutMode::MasterStack), area, 3, 10, &config);
        assert_eq!(bottom[0].loc.y, 405);
        assert_eq!(
            bottom[2],
            Rectangle::new((10, 10).into(), (980, 385).into())
        );

        // The frame round-trips, so a drag lands where it was aimed.
        let frame = MasterFrame::new(
            Rectangle::new((10, 10).into(), (980, 780).into()),
            MasterPosition::Bottom,
        );
        assert_eq!(frame.to_frame(frame.to_output(left[1])), left[1]);

        // A workspace's own adjustment still beats the config.
        let adjusted = Workspace {
            master_count: Some(1),
            ..workspace(LayoutMode::MasterStack)
        };
        config.master_stack.position = MasterPosition::Left;
        let one = layout_rectangles(&adjusted, area, 3, 10, &config);
        assert_eq!(one[0].size.h, 780);

        // dwindle: a "longest" split cuts across whichever side is longer.
        let mut config = LayoutConfig::default();
        config.dwindle.split = DwindleSplit::Longest;
        let tall = Rectangle::new((0, 0).into(), (800, 1000).into());
        let dwindle = layout_rectangles(&workspace(LayoutMode::Dwindle), tall, 2, 0, &config);
        assert_eq!(dwindle[0], Rectangle::new((0, 0).into(), (800, 500).into()));
        config.dwindle.split_ratio = 0.25;
        let dwindle = layout_rectangles(&workspace(LayoutMode::Dwindle), area, 2, 0, &config);
        assert_eq!(dwindle[0].size.w, 250);

        // monocle: gaps can be turned off.
        let mut config = LayoutConfig::default();
        config.monocle.gaps = false;
        let monocle = layout_rectangles(&workspace(LayoutMode::Monocle), area, 2, 10, &config);
        assert_eq!(monocle, vec![area; 2]);
    }

    #[test]
    fn stacked_rows_preserve_weights_and_move_with_windows() {
        let rows = stacked_rows(0, 300, &[1.0, 2.0], 10, MIN_ROW_HEIGHT);
        assert_eq!(rows[0].size.h, 106);
        assert_eq!(rows[1].size.h, 164);

        let weights = resized_row_weights(&[106.0, 164.0], 0, 50.0, MIN_ROW_HEIGHT).unwrap();
        let rows = stacked_rows(0, 300, &weights, 10, MIN_ROW_HEIGHT);
        assert_eq!([rows[0].size.h, rows[1].size.h], [156, 114]);
        let weights = resized_row_weights(&[106.0, 164.0], 0, 1000.0, MIN_ROW_HEIGHT).unwrap();
        let rows = stacked_rows(0, 300, &weights, 10, MIN_ROW_HEIGHT);
        assert_eq!([rows[0].size.h, rows[1].size.h], [222, 48]);

        let mut columns = vec![
            Column {
                windows: vec![1, 2],
                row_weights: vec![2.0, 4.0],
                focused: 0,
                width: None,
            },
            Column::new(3),
        ];
        assert!(consume_or_expel_columns(&mut columns, 1, -1));
        assert_eq!(columns[0].row_weights, [2.0, 4.0, 3.0]);

        let mut column = Column {
            windows: vec![1, 2, 3],
            row_weights: vec![1.0, 1.0, 1.0],
            focused: 1,
            width: None,
        };
        assert!(adjust_row_weights(&mut column, -0.05));
        assert!((column.row_weights[1] - 0.85).abs() < f64::EPSILON);
        assert!((column.row_weights[2] - 1.15).abs() < f64::EPSILON);
        assert!(adjust_row_weights(&mut column, 0.05));
        assert!(
            column
                .row_weights
                .iter()
                .zip([1.0, 1.0, 1.0])
                .all(|(actual, expected)| (actual - expected).abs() < f64::EPSILON)
        );
        column.focused = 2;
        assert!(adjust_row_weights(&mut column, 0.05));
        assert!((column.row_weights.iter().sum::<f64>() - 3.0).abs() < f64::EPSILON);
        column.row_weights[2] = 0.0;
        assert!(!adjust_row_weights(&mut column, -0.05));
        assert!(!adjust_row_weights(&mut Column::new(1), 0.05));
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
    fn tiled_drag_preview_scales_around_the_grab_point() {
        let geometry = Rectangle::new((100, 50).into(), (800, 600).into());
        assert_eq!(drag_preview_size(geometry.size), (600, 450).into());
        assert_eq!(
            drag_grab_offset((300.0, 200.0).into(), geometry),
            (150.0, 112.5).into()
        );
        assert_eq!(drag_preview_size((1, 1).into()), (1, 1).into());
    }

    #[test]
    fn floating_resize_tracks_edges_and_stays_in_work_area() {
        let area = Rectangle::new((0, 0).into(), (1000, 800).into());
        let initial = Rectangle::new((250, 200).into(), (500, 400).into());
        assert_eq!(default_floating_size(area.size), (500, 400).into());
        assert_eq!(
            floating_resize_edges((250.0, 200.0).into(), initial, 8),
            (Some(FloatingEdge::Start), Some(FloatingEdge::Start))
        );
        assert_eq!(
            floating_resize_edges((500.0, 400.0).into(), initial, 8),
            (None, None)
        );
        assert_eq!(
            floating_resize_edges((750.0, 600.0).into(), initial, 8),
            (Some(FloatingEdge::End), Some(FloatingEdge::End))
        );

        assert_eq!(
            resized_floating_rectangle(
                initial,
                Some(FloatingEdge::Start),
                Some(FloatingEdge::Start),
                (100.0, 50.0).into(),
                area,
                MIN_ROW_HEIGHT,
            ),
            Rectangle::new((350, 250).into(), (400, 350).into())
        );
        assert_eq!(
            resized_floating_rectangle(
                initial,
                Some(FloatingEdge::End),
                Some(FloatingEdge::End),
                (1000.0, 1000.0).into(),
                area,
                MIN_ROW_HEIGHT,
            ),
            Rectangle::new((250, 200).into(), (750, 600).into())
        );
        assert_eq!(
            resized_floating_rectangle(
                initial,
                Some(FloatingEdge::Start),
                Some(FloatingEdge::Start),
                (1000.0, 1000.0).into(),
                area,
                MIN_ROW_HEIGHT,
            ),
            Rectangle::new((702, 552).into(), (48, 48).into())
        );
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

    #[test]
    fn scratchpad_geometry_uses_percentages_and_each_edge() {
        let area = Rectangle::new((100, 50).into(), (1000, 800).into());
        let mut config = ScratchpadConfig {
            name: "term".into(),
            command: vec!["kitty".into()],
            app_id: "paw-term".into(),
            edge: ScratchpadEdge::Top,
            size: [50.0, 50.0],
            position: None,
            margin: 20,
        };
        let rectangle = configured_scratchpad_rectangle(&config, area);
        assert_eq!(
            rectangle,
            Rectangle::new((350, 70).into(), (500, 400).into())
        );
        assert_eq!(
            hidden_scratchpad_location(config.edge, area, rectangle),
            (350, -350).into()
        );
        config.edge = ScratchpadEdge::Right;
        let rectangle = configured_scratchpad_rectangle(&config, area);
        assert_eq!(rectangle.loc, (580, 250).into());
        assert_eq!(
            hidden_scratchpad_location(config.edge, area, rectangle),
            (1100, 250).into()
        );
    }

    #[test]
    fn scratchpad_strip_is_one_line_that_hugs_its_cards() {
        let area = Rectangle::new((15, 15).into(), (970, 770).into());
        let position = ScratchpadOverviewPosition::Centered;
        let (panel, cards) = scratchpad_strip_layout(area, 3, position, 1);

        assert_eq!(cards.len(), 3);
        assert!(cards.iter().all(|(_, card)| panel.contains_rect(*card)));
        // One row: same size, same y, marching right, no overlap.
        assert!(cards.windows(2).all(|pair| pair[0].1.size == pair[1].1.size
            && pair[0].1.loc.y == pair[1].1.loc.y
            && pair[1].1.loc.x > pair[0].1.loc.x
            && !pair[0].1.overlaps(pair[1].1)));
        // The panel is exactly wide enough for the cards plus the gaps around
        // them, and sits centred in the usable area.
        let step = cards[0].1.size.w + OVERVIEW_GAP;
        assert_eq!(panel.size.w, 3 * step + OVERVIEW_GAP);
        assert_eq!(panel.size.h, cards[0].1.size.h + OVERVIEW_GAP * 2);
        assert_eq!(panel.loc.x, area.loc.x + (area.size.w - panel.size.w) / 2);
        assert_eq!(panel.loc.y, area.loc.y + (area.size.h - panel.size.h) / 2);
    }

    #[test]
    fn scratchpad_strip_stays_inside_the_usable_area() {
        // A 40px bar at the top, then the window gap.
        let zone = Rectangle::new((0, 40).into(), (1000, 760).into());
        let area = inset_rectangle(zone, 15);
        for position in [
            ScratchpadOverviewPosition::Top,
            ScratchpadOverviewPosition::Bottom,
            ScratchpadOverviewPosition::Centered,
            ScratchpadOverviewPosition::Left,
            ScratchpadOverviewPosition::Right,
        ] {
            let (panel, cards) = scratchpad_strip_layout(area, 4, position, 0);
            assert!(area.contains_rect(panel), "{position:?} escaped the area");
            // The border is drawn outside the panel, and the gap we inset by is
            // never smaller than focus_ring_width, so it clears the bar too.
            assert!(zone.contains_rect(inset_rectangle(panel, -15)));
            assert!(cards.iter().all(|(_, card)| panel.contains_rect(*card)));
        }

        let (top, _) = scratchpad_strip_layout(area, 4, ScratchpadOverviewPosition::Top, 0);
        assert_eq!(top.loc.y, area.loc.y);
        let (bottom, _) = scratchpad_strip_layout(area, 4, ScratchpadOverviewPosition::Bottom, 0);
        assert_eq!(bottom.loc.y + bottom.size.h, area.loc.y + area.size.h);
        let (left, cards) = scratchpad_strip_layout(area, 3, ScratchpadOverviewPosition::Left, 0);
        assert_eq!(left.loc.x, area.loc.x);
        // Left stacks the cards down the screen instead of across it.
        assert!(
            cards
                .windows(2)
                .all(|pair| pair[0].1.loc.x == pair[1].1.loc.x && pair[1].1.loc.y > pair[0].1.loc.y)
        );
    }

    #[test]
    fn scratchpad_strip_shows_whole_cards_and_stops_at_both_ends() {
        let area = Rectangle::new((15, 15).into(), (970, 770).into());
        let position = ScratchpadOverviewPosition::Centered;
        let count = 20;
        let (panel, cards) = scratchpad_strip_layout(area, count, position, 0);
        // Too many to fit, so the panel caps and only whole cards are built.
        assert!(cards.len() < count);
        assert!(panel.size.w <= area.size.w);
        assert!(cards.iter().all(|(_, card)| panel.contains_rect(*card)));

        // Clamped at the near end: the first card is index 0, not a blank slot.
        assert_eq!(cards[0].0, 0);
        // Clamped at the far end: the last card is the last scratchpad.
        let (_, cards) = scratchpad_strip_layout(area, count, position, count - 1);
        assert_eq!(cards.last().unwrap().0, count - 1);
        // In the middle the selection is centred.
        let (_, cards) = scratchpad_strip_layout(area, count, position, 10);
        assert_eq!(cards[cards.len() / 2].0, 10);
    }

    #[test]
    fn scratchpad_geometry_state_round_trips() {
        let state = HashMap::from([(
            "term".to_string(),
            HashMap::from([(
                "DP-1".to_string(),
                SavedScratchpadGeometry {
                    location: [120, 80],
                    size: [900, 600],
                },
            )]),
        )]);
        let encoded = serde_json::to_vec(&state).unwrap();
        assert_eq!(
            serde_json::from_slice::<HashMap<String, HashMap<String, SavedScratchpadGeometry>>>(
                &encoded
            )
            .unwrap(),
            state
        );
    }
}
