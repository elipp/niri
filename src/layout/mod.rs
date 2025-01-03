//! Window layout logic.
//!
//! Niri implements scrollable tiling with workspaces. There's one primary output, and potentially
//! multiple other outputs.
//!
//! Our layout has the following invariants:
//!
//! 1. Disconnecting and reconnecting the same output must not change the layout.
//!    * This includes both secondary outputs and the primary output.
//! 2. Connecting an output must not change the layout for any workspaces that were never on that
//!    output.
//!
//! Therefore, we implement the following logic: every workspace keeps track of which output it
//! originated on. When an output disconnects, its workspace (or workspaces, in case of the primary
//! output disconnecting) are appended to the (potentially new) primary output, but remember their
//! original output. Then, if the original output connects again, all workspaces originally from
//! there move back to that output.
//!
//! In order to avoid surprising behavior, if the user creates or moves any new windows onto a
//! workspace, it forgets its original output, and its current output becomes its original output.
//! Imagine a scenario: the user works with a laptop and a monitor at home, then takes their laptop
//! with them, disconnecting the monitor, and keeps working as normal, using the second monitor's
//! workspace just like any other. Then they come back, reconnect the second monitor, and now we
//! don't want an unassuming workspace to end up on it.
//!
//! ## Workspaces-only-on-primary considerations
//!
//! If this logic results in more than one workspace present on a secondary output, then as a
//! compromise we only keep the first workspace there, and move the rest to the primary output,
//! making the primary output their original output.

use std::cmp::min;
use std::mem;
use std::rc::Rc;
use std::time::Duration;

use niri_config::{
    CenterFocusedColumn, Config, CornerRadius, FloatOrInt, PresetSize, Struts,
    Workspace as WorkspaceConfig,
};
use niri_ipc::SizeChange;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::Id;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::output::{self, Output};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Scale, Serial, Size, Transform};
use tile::{Tile, TileRenderElement};
use workspace::WorkspaceId;

pub use self::monitor::MonitorRenderElement;
use self::monitor::{Monitor, WorkspaceSwitch};
use self::workspace::{compute_working_area, Column, ColumnWidth, InsertHint, OutputId, Workspace};
use crate::layout::workspace::InsertPosition;
use crate::niri_render_elements;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::snapshot::RenderSnapshot;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::TextureBuffer;
use crate::render_helpers::{BakedBuffer, RenderTarget, SplitElements};
use crate::rubber_band::RubberBand;
use crate::utils::transaction::{Transaction, TransactionBlocker};
use crate::utils::{output_matches_name, output_size, round_logical_in_physical_max1, ResizeEdge};
use crate::window::ResolvedWindowRules;

pub mod closing_window;
pub mod focus_ring;
pub mod insert_hint_element;
pub mod monitor;
pub mod opening_window;
pub mod tile;
pub mod workspace;

/// Size changes up to this many pixels don't animate.
pub const RESIZE_ANIMATION_THRESHOLD: f64 = 10.;

/// Pointer needs to move this far to pull a window from the layout.
const INTERACTIVE_MOVE_START_THRESHOLD: f64 = 256. * 256.;

niri_render_elements! {
    LayoutElementRenderElement<R> => {
        Wayland = WaylandSurfaceRenderElement<R>,
        SolidColor = SolidColorRenderElement,
    }
}

pub type LayoutElementRenderSnapshot =
    RenderSnapshot<BakedBuffer<TextureBuffer<GlesTexture>>, BakedBuffer<SolidColorBuffer>>;

#[derive(Debug)]
enum InteractiveMoveState<W: LayoutElement> {
    /// Initial rubberbanding; the window remains in the layout.
    Starting {
        /// The window we're moving.
        window_id: W::Id,
        /// Current pointer delta from the starting location.
        pointer_delta: Point<f64, Logical>,
        /// Pointer location within the visual window geometry as ratio from geometry size.
        ///
        /// This helps the pointer remain inside the window as it resizes.
        pointer_ratio_within_window: (f64, f64),
    },
    /// Moving; the window is no longer in the layout.
    Moving(InteractiveMoveData<W>),
}

#[derive(Debug)]
struct InteractiveMoveData<W: LayoutElement> {
    /// The window being moved.
    pub(self) tile: Tile<W>,
    /// Output where the window is currently located/rendered.
    pub(self) output: Output,
    /// Current pointer position within output.
    pub(self) pointer_pos_within_output: Point<f64, Logical>,
    /// Window column width.
    pub(self) width: ColumnWidth,
    /// Whether the window column was full-width.
    pub(self) is_full_width: bool,
    /// Pointer location within the visual window geometry as ratio from geometry size.
    ///
    /// This helps the pointer remain inside the window as it resizes.
    pub(self) pointer_ratio_within_window: (f64, f64),
}

#[derive(Debug, Clone, Copy)]
pub struct InteractiveResizeData {
    pub(self) edges: ResizeEdge,
}

#[derive(Debug, Clone, Copy)]
pub enum ConfigureIntent {
    /// A configure is not needed (no changes to server pending state).
    NotNeeded,
    /// A configure is throttled (due to resizing too fast for example).
    Throttled,
    /// Can send the configure if it isn't throttled externally (only size changed).
    CanSend,
    /// Should send the configure regardless of external throttling (something other than size
    /// changed).
    ShouldSend,
}

pub trait LayoutElement {
    /// Type that can be used as a unique ID of this element.
    type Id: PartialEq + std::fmt::Debug + Clone;

    /// Unique ID of this element.
    fn id(&self) -> &Self::Id;

    /// Visual size of the element.
    ///
    /// This is what the user would consider the size, i.e. excluding CSD shadows and whatnot.
    /// Corresponds to the Wayland window geometry size.
    fn size(&self) -> Size<i32, Logical>;

    /// Returns the location of the element's buffer relative to the element's visual geometry.
    ///
    /// I.e. if the element has CSD shadows, its buffer location will have negative coordinates.
    fn buf_loc(&self) -> Point<i32, Logical>;

    /// Checks whether a point is in the element's input region.
    ///
    /// The point is relative to the element's visual geometry.
    fn is_in_input_region(&self, point: Point<f64, Logical>) -> bool;

    /// Renders the element at the given visual location.
    ///
    /// The element should be rendered in such a way that its visual geometry ends up at the given
    /// location.
    fn render<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        location: Point<f64, Logical>,
        scale: Scale<f64>,
        alpha: f32,
        target: RenderTarget,
    ) -> SplitElements<LayoutElementRenderElement<R>>;

    /// Renders the non-popup parts of the element.
    fn render_normal<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        location: Point<f64, Logical>,
        scale: Scale<f64>,
        alpha: f32,
        target: RenderTarget,
    ) -> Vec<LayoutElementRenderElement<R>> {
        self.render(renderer, location, scale, alpha, target).normal
    }

    /// Renders the popups of the element.
    fn render_popups<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        location: Point<f64, Logical>,
        scale: Scale<f64>,
        alpha: f32,
        target: RenderTarget,
    ) -> Vec<LayoutElementRenderElement<R>> {
        self.render(renderer, location, scale, alpha, target).popups
    }

    fn request_size(
        &mut self,
        size: Size<i32, Logical>,
        animate: bool,
        transaction: Option<Transaction>,
    );
    fn request_fullscreen(&self, size: Size<i32, Logical>);
    fn min_size(&self) -> Size<i32, Logical>;
    fn max_size(&self) -> Size<i32, Logical>;
    fn is_wl_surface(&self, wl_surface: &WlSurface) -> bool;
    fn has_ssd(&self) -> bool;
    fn set_preferred_scale_transform(&self, scale: output::Scale, transform: Transform);
    fn output_enter(&self, output: &Output);
    fn output_leave(&self, output: &Output);
    fn set_offscreen_element_id(&self, id: Option<Id>);
    fn set_activated(&mut self, active: bool);
    fn set_active_in_column(&mut self, active: bool);
    fn set_bounds(&self, bounds: Size<i32, Logical>);

    fn configure_intent(&self) -> ConfigureIntent;
    fn send_pending_configure(&mut self);

    /// Whether the element is currently fullscreen.
    ///
    /// This will *not* switch immediately after a [`LayoutElement::request_fullscreen()`] call.
    fn is_fullscreen(&self) -> bool;

    /// Whether we're requesting the element to be fullscreen.
    ///
    /// This *will* switch immediately after a [`LayoutElement::request_fullscreen()`] call.
    fn is_pending_fullscreen(&self) -> bool;

    /// Size previously requested through [`LayoutElement::request_size()`].
    fn requested_size(&self) -> Option<Size<i32, Logical>>;

    fn rules(&self) -> &ResolvedWindowRules;

    /// Runs periodic clean-up tasks.
    fn refresh(&self);

    fn animation_snapshot(&self) -> Option<&LayoutElementRenderSnapshot>;
    fn take_animation_snapshot(&mut self) -> Option<LayoutElementRenderSnapshot>;

    fn set_interactive_resize(&mut self, data: Option<InteractiveResizeData>);
    fn cancel_interactive_resize(&mut self);
    fn update_interactive_resize(&mut self, serial: Serial);
    fn interactive_resize_data(&self) -> Option<InteractiveResizeData>;
}

#[derive(Debug)]
pub struct Layout<W: LayoutElement> {
    /// Monitors and workspaes in the layout.
    monitor_set: MonitorSet<W>,
    /// Whether the layout should draw as active.
    ///
    /// This normally indicates that the layout has keyboard focus, but not always. E.g. when the
    /// screenshot UI is open, it keeps the layout drawing as active.
    is_active: bool,
    /// Ongoing interactive move.
    interactive_move: Option<InteractiveMoveState<W>>,
    /// Configurable properties of the layout.
    options: Rc<Options>,
}

#[derive(Debug)]
enum MonitorSet<W: LayoutElement> {
    /// At least one output is connected.
    Normal {
        /// Connected monitors.
        monitors: Vec<Monitor<W>>,
        /// Index of the primary monitor.
        primary_idx: usize,
        /// Index of the active monitor.
        active_monitor_idx: usize,
    },
    /// No outputs are connected, and these are the workspaces.
    NoOutputs {
        /// The workspaces.
        workspaces: Vec<Workspace<W>>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    /// Padding around windows in logical pixels.
    pub gaps: f64,
    /// Extra padding around the working area in logical pixels.
    pub struts: Struts,
    pub focus_ring: niri_config::FocusRing,
    pub border: niri_config::Border,
    pub insert_hint: niri_config::InsertHint,
    pub center_focused_column: CenterFocusedColumn,
    pub always_center_single_column: bool,
    /// Column widths that `toggle_width()` switches between.
    pub preset_column_widths: Vec<ColumnWidth>,
    /// Initial width for new columns.
    pub default_column_width: Option<ColumnWidth>,
    /// Window height that `toggle_window_height()` switches between.
    pub preset_window_heights: Vec<PresetSize>,
    pub animations: niri_config::Animations,
    // Debug flags.
    pub disable_resize_throttling: bool,
    pub disable_transactions: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            gaps: 16.,
            struts: Default::default(),
            focus_ring: Default::default(),
            border: Default::default(),
            insert_hint: Default::default(),
            center_focused_column: Default::default(),
            always_center_single_column: false,
            preset_column_widths: vec![
                ColumnWidth::Proportion(1. / 3.),
                ColumnWidth::Proportion(0.5),
                ColumnWidth::Proportion(2. / 3.),
            ],
            default_column_width: None,
            animations: Default::default(),
            disable_resize_throttling: false,
            disable_transactions: false,
            preset_window_heights: vec![
                PresetSize::Proportion(1. / 3.),
                PresetSize::Proportion(0.5),
                PresetSize::Proportion(2. / 3.),
            ],
        }
    }
}

/// Tile that was just removed from the layout.
pub struct RemovedTile<W: LayoutElement> {
    tile: Tile<W>,
    /// Width of the column the tile was in.
    width: ColumnWidth,
    /// Whether the column the tile was in was full-width.
    is_full_width: bool,
}

impl<W: LayoutElement> InteractiveMoveState<W> {
    fn moving(&self) -> Option<&InteractiveMoveData<W>> {
        match self {
            InteractiveMoveState::Moving(move_) => Some(move_),
            _ => None,
        }
    }
}

impl<W: LayoutElement> InteractiveMoveData<W> {
    fn tile_render_location(&self) -> Point<f64, Logical> {
        let scale = Scale::from(self.output.current_scale().fractional_scale());
        let window_size = self.tile.window_size();
        let pointer_offset_within_window = Point::from((
            window_size.w * self.pointer_ratio_within_window.0,
            window_size.h * self.pointer_ratio_within_window.1,
        ));
        let pos =
            self.pointer_pos_within_output - pointer_offset_within_window - self.tile.window_loc()
                + self.tile.render_offset();
        // Round to physical pixels.
        pos.to_physical_precise_round(scale).to_logical(scale)
    }
}

impl Options {
    fn from_config(config: &Config) -> Self {
        let layout = &config.layout;

        let preset_column_widths = if layout.preset_column_widths.is_empty() {
            Options::default().preset_column_widths
        } else {
            layout
                .preset_column_widths
                .iter()
                .copied()
                .map(ColumnWidth::from)
                .collect()
        };
        let preset_window_heights = if layout.preset_window_heights.is_empty() {
            Options::default().preset_window_heights
        } else {
            layout.preset_window_heights.clone()
        };

        // Missing default_column_width maps to Some(ColumnWidth::Proportion(0.5)),
        // while present, but empty, maps to None.
        let default_column_width = layout
            .default_column_width
            .as_ref()
            .map(|w| w.0.map(ColumnWidth::from))
            .unwrap_or(Some(ColumnWidth::Proportion(0.5)));

        Self {
            gaps: layout.gaps.0,
            struts: layout.struts,
            focus_ring: layout.focus_ring,
            border: layout.border,
            insert_hint: layout.insert_hint,
            center_focused_column: layout.center_focused_column,
            always_center_single_column: layout.always_center_single_column,
            preset_column_widths,
            default_column_width,
            animations: config.animations.clone(),
            disable_resize_throttling: config.debug.disable_resize_throttling,
            disable_transactions: config.debug.disable_transactions,
            preset_window_heights,
        }
    }

    fn adjusted_for_scale(mut self, scale: f64) -> Self {
        let round = |logical: f64| round_logical_in_physical_max1(scale, logical);

        self.gaps = round(self.gaps);
        self.focus_ring.width = FloatOrInt(round(self.focus_ring.width.0));
        self.border.width = FloatOrInt(round(self.border.width.0));

        self
    }
}

impl<W: LayoutElement> Layout<W> {
    pub fn new(config: &Config) -> Self {
        Self::with_options_and_workspaces(config, Options::from_config(config))
    }

    pub fn with_options(options: Options) -> Self {
        Self {
            monitor_set: MonitorSet::NoOutputs { workspaces: vec![] },
            is_active: true,
            interactive_move: None,
            options: Rc::new(options),
        }
    }

    fn with_options_and_workspaces(config: &Config, options: Options) -> Self {
        let opts = Rc::new(options);

        let workspaces = config
            .workspaces
            .iter()
            .map(|ws| Workspace::new_with_config_no_outputs(Some(ws.clone()), opts.clone()))
            .collect();

        Self {
            monitor_set: MonitorSet::NoOutputs { workspaces },
            is_active: true,
            interactive_move: None,
            options: opts,
        }
    }

    pub fn add_output(&mut self, output: Output) {
        self.monitor_set = match mem::take(&mut self.monitor_set) {
            MonitorSet::Normal {
                mut monitors,
                primary_idx,
                active_monitor_idx,
            } => {
                let primary = &mut monitors[primary_idx];

                let mut stopped_primary_ws_switch = false;

                let mut workspaces = vec![];
                for i in (0..primary.workspaces.len()).rev() {
                    if primary.workspaces[i].original_output.matches(&output) {
                        let ws = primary.workspaces.remove(i);

                        // FIXME: this can be coded in a way that the workspace switch won't be
                        // affected if the removed workspace is invisible. But this is good enough
                        // for now.
                        if primary.workspace_switch.is_some() {
                            primary.workspace_switch = None;
                            stopped_primary_ws_switch = true;
                        }

                        // The user could've closed a window while remaining on this workspace, on
                        // another monitor. However, we will add an empty workspace in the end
                        // instead.
                        if ws.has_windows() || ws.name.is_some() {
                            workspaces.push(ws);
                        }

                        if i <= primary.active_workspace_idx {
                            primary.active_workspace_idx =
                                primary.active_workspace_idx.saturating_sub(1);
                        }
                    }
                }

                // If we stopped a workspace switch, then we might need to clean up workspaces.
                if stopped_primary_ws_switch {
                    primary.clean_up_workspaces();
                }

                workspaces.reverse();

                // Make sure there's always an empty workspace.
                workspaces.push(Workspace::new(output.clone(), self.options.clone()));

                for ws in &mut workspaces {
                    ws.set_output(Some(output.clone()));
                }

                monitors.push(Monitor::new(output, workspaces, self.options.clone()));
                MonitorSet::Normal {
                    monitors,
                    primary_idx,
                    active_monitor_idx,
                }
            }
            MonitorSet::NoOutputs { mut workspaces } => {
                // We know there are no empty workspaces there, so add one.
                workspaces.push(Workspace::new(output.clone(), self.options.clone()));

                for workspace in &mut workspaces {
                    workspace.set_output(Some(output.clone()));
                }

                let monitor = Monitor::new(output, workspaces, self.options.clone());

                MonitorSet::Normal {
                    monitors: vec![monitor],
                    primary_idx: 0,
                    active_monitor_idx: 0,
                }
            }
        }
    }

    pub fn remove_output(&mut self, output: &Output) {
        self.monitor_set = match mem::take(&mut self.monitor_set) {
            MonitorSet::Normal {
                mut monitors,
                mut primary_idx,
                mut active_monitor_idx,
            } => {
                let idx = monitors
                    .iter()
                    .position(|mon| &mon.output == output)
                    .expect("trying to remove non-existing output");
                let monitor = monitors.remove(idx);
                let mut workspaces = monitor.workspaces;

                for ws in &mut workspaces {
                    ws.set_output(None);
                }

                // Get rid of empty workspaces.
                workspaces.retain(|ws| ws.has_windows() || ws.name.is_some());

                if monitors.is_empty() {
                    // Removed the last monitor.
                    MonitorSet::NoOutputs { workspaces }
                } else {
                    if primary_idx >= idx {
                        // Update primary_idx to either still point at the same monitor, or at some
                        // other monitor if the primary has been removed.
                        primary_idx = primary_idx.saturating_sub(1);
                    }
                    if active_monitor_idx >= idx {
                        // Update active_monitor_idx to either still point at the same monitor, or
                        // at some other monitor if the active monitor has
                        // been removed.
                        active_monitor_idx = active_monitor_idx.saturating_sub(1);
                    }

                    let primary = &mut monitors[primary_idx];
                    for ws in &mut workspaces {
                        ws.set_output(Some(primary.output.clone()));
                    }

                    let empty_was_focused =
                        primary.active_workspace_idx == primary.workspaces.len() - 1;

                    // Push the workspaces from the removed monitor in the end, right before the
                    // last, empty, workspace.
                    let empty = primary.workspaces.remove(primary.workspaces.len() - 1);
                    primary.workspaces.extend(workspaces);
                    primary.workspaces.push(empty);

                    // If the empty workspace was focused on the primary monitor, keep it focused.
                    if empty_was_focused {
                        primary.active_workspace_idx = primary.workspaces.len() - 1;
                    }

                    MonitorSet::Normal {
                        monitors,
                        primary_idx,
                        active_monitor_idx,
                    }
                }
            }
            MonitorSet::NoOutputs { .. } => {
                panic!("tried to remove output when there were already none")
            }
        }
    }

    pub fn add_window_by_idx(
        &mut self,
        monitor_idx: usize,
        workspace_idx: usize,
        window: W,
        activate: bool,
        width: ColumnWidth,
        is_full_width: bool,
    ) {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        else {
            panic!()
        };

        monitors[monitor_idx].add_window(workspace_idx, window, activate, width, is_full_width);

        if activate {
            *active_monitor_idx = monitor_idx;
        }
    }

    /// Adds a new window to the layout on a specific workspace.
    pub fn add_window_to_named_workspace(
        &mut self,
        workspace_name: &str,
        window: W,
        width: Option<ColumnWidth>,
        is_full_width: bool,
    ) -> Option<&Output> {
        let width = self.resolve_default_width(&window, width);

        match &mut self.monitor_set {
            MonitorSet::Normal {
                monitors,
                active_monitor_idx,
                ..
            } => {
                let (mon_idx, mon, ws_idx) = monitors
                    .iter_mut()
                    .enumerate()
                    .find_map(|(mon_idx, mon)| {
                        mon.find_named_workspace_index(workspace_name)
                            .map(move |ws_idx| (mon_idx, mon, ws_idx))
                    })
                    .unwrap();

                // Don't steal focus from an active fullscreen window.
                let mut activate = true;
                let ws = &mon.workspaces[ws_idx];
                if mon_idx == *active_monitor_idx
                    && !ws.columns.is_empty()
                    && ws.columns[ws.active_column_idx].is_fullscreen
                {
                    activate = false;
                }

                // Don't activate if on a different workspace.
                if mon.active_workspace_idx != ws_idx {
                    activate = false;
                }

                mon.add_window(ws_idx, window, activate, width, is_full_width);
                Some(&mon.output)
            }
            MonitorSet::NoOutputs { workspaces } => {
                let ws = workspaces
                    .iter_mut()
                    .find(|ws| {
                        ws.name
                            .as_ref()
                            .map_or(false, |name| name.eq_ignore_ascii_case(workspace_name))
                    })
                    .unwrap();
                ws.add_window(None, window, true, width, is_full_width);
                None
            }
        }
    }

    pub fn add_column_by_idx(
        &mut self,
        monitor_idx: usize,
        workspace_idx: usize,
        column: Column<W>,
        activate: bool,
    ) {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        else {
            panic!()
        };

        monitors[monitor_idx].add_column(workspace_idx, column, activate);

        if activate {
            *active_monitor_idx = monitor_idx;
        }
    }

    /// Adds a new window to the layout.
    ///
    /// Returns an output that the window was added to, if there were any outputs.
    pub fn add_window(
        &mut self,
        window: W,
        width: Option<ColumnWidth>,
        is_full_width: bool,
    ) -> Option<&Output> {
        let width = self.resolve_default_width(&window, width);

        match &mut self.monitor_set {
            MonitorSet::Normal {
                monitors,
                active_monitor_idx,
                ..
            } => {
                let mon = &mut monitors[*active_monitor_idx];

                // Don't steal focus from an active fullscreen window.
                let mut activate = true;
                let ws = &mon.workspaces[mon.active_workspace_idx];
                if !ws.columns.is_empty() && ws.columns[ws.active_column_idx].is_fullscreen {
                    activate = false;
                }

                mon.add_window(
                    mon.active_workspace_idx,
                    window,
                    activate,
                    width,
                    is_full_width,
                );
                Some(&mon.output)
            }
            MonitorSet::NoOutputs { workspaces } => {
                let ws = if let Some(ws) = workspaces.get_mut(0) {
                    ws
                } else {
                    workspaces.push(Workspace::new_no_outputs(self.options.clone()));
                    &mut workspaces[0]
                };
                ws.add_window(None, window, true, width, is_full_width);
                None
            }
        }
    }

    /// Adds a new window to the layout immediately to the right of another window.
    ///
    /// If that another window was active, activates the new window.
    ///
    /// Returns an output that the window was added to, if there were any outputs.
    pub fn add_window_right_of(
        &mut self,
        right_of: &W::Id,
        window: W,
        width: Option<ColumnWidth>,
        is_full_width: bool,
    ) -> Option<&Output> {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if right_of == move_.tile.window().id() {
                let output = move_.output.clone();
                if self.monitor_for_output(&output).is_some() {
                    self.add_window_on_output(&output, window, width, is_full_width);
                    return Some(&self.monitor_for_output(&output).unwrap().output);
                } else {
                    return self.add_window(window, width, is_full_width);
                }
            }
        }

        let width = self.resolve_default_width(&window, width);

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                let mon = monitors
                    .iter_mut()
                    .find(|mon| mon.workspaces.iter().any(|ws| ws.has_window(right_of)))
                    .unwrap();

                mon.add_window_right_of(right_of, window, width, is_full_width);
                Some(&mon.output)
            }
            MonitorSet::NoOutputs { workspaces } => {
                let ws = workspaces
                    .iter_mut()
                    .find(|ws| ws.has_window(right_of))
                    .unwrap();
                ws.add_window_right_of(right_of, window, width, is_full_width);
                None
            }
        }
    }

    /// Adds a new window to the layout on a specific output.
    pub fn add_window_on_output(
        &mut self,
        output: &Output,
        window: W,
        width: Option<ColumnWidth>,
        is_full_width: bool,
    ) {
        let width = self.resolve_default_width(&window, width);

        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        else {
            panic!()
        };

        let (mon_idx, mon) = monitors
            .iter_mut()
            .enumerate()
            .find(|(_, mon)| mon.output == *output)
            .unwrap();

        // Don't steal focus from an active fullscreen window.
        let mut activate = true;
        let ws = &mon.workspaces[mon.active_workspace_idx];
        if mon_idx == *active_monitor_idx
            && !ws.columns.is_empty()
            && ws.columns[ws.active_column_idx].is_fullscreen
        {
            activate = false;
        }

        mon.add_window(
            mon.active_workspace_idx,
            window,
            activate,
            width,
            is_full_width,
        );
    }

    pub fn remove_window(
        &mut self,
        window: &W::Id,
        transaction: Transaction,
    ) -> Option<RemovedTile<W>> {
        if let Some(state) = &self.interactive_move {
            match state {
                InteractiveMoveState::Starting { window_id, .. } => {
                    if window_id == window {
                        self.interactive_move_end(window);
                    }
                }
                InteractiveMoveState::Moving(move_) => {
                    if move_.tile.window().id() == window {
                        let Some(InteractiveMoveState::Moving(move_)) =
                            self.interactive_move.take()
                        else {
                            unreachable!()
                        };
                        return Some(RemovedTile {
                            tile: move_.tile,
                            width: move_.width,
                            is_full_width: move_.is_full_width,
                        });
                    }
                }
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for (idx, ws) in mon.workspaces.iter_mut().enumerate() {
                        if ws.has_window(window) {
                            let removed = ws.remove_tile(window, transaction);

                            // Clean up empty workspaces that are not active and not last.
                            if !ws.has_windows()
                                && ws.name.is_none()
                                && idx != mon.active_workspace_idx
                                && idx != mon.workspaces.len() - 1
                                && mon.workspace_switch.is_none()
                            {
                                mon.workspaces.remove(idx);

                                if idx < mon.active_workspace_idx {
                                    mon.active_workspace_idx -= 1;
                                }
                            }

                            return Some(removed);
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for (idx, ws) in workspaces.iter_mut().enumerate() {
                    if ws.has_window(window) {
                        let removed = ws.remove_tile(window, transaction);

                        // Clean up empty workspaces.
                        if !ws.has_windows() && ws.name.is_none() {
                            workspaces.remove(idx);
                        }

                        return Some(removed);
                    }
                }
            }
        }

        None
    }

    pub fn update_window(&mut self, window: &W::Id, serial: Option<Serial>) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if move_.tile.window().id() == window {
                move_.tile.update_window();
                return;
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        if ws.has_window(window) {
                            ws.update_window(window, serial);
                            return;
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    if ws.has_window(window) {
                        ws.update_window(window, serial);
                        return;
                    }
                }
            }
        }
    }

    pub fn find_window_and_output(&self, wl_surface: &WlSurface) -> Option<(&W, &Output)> {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().is_wl_surface(wl_surface) {
                return Some((move_.tile.window(), &move_.output));
            }
        }

        if let MonitorSet::Normal { monitors, .. } = &self.monitor_set {
            for mon in monitors {
                for ws in &mon.workspaces {
                    if let Some(window) = ws.find_wl_surface(wl_surface) {
                        return Some((window, &mon.output));
                    }
                }
            }
        }

        None
    }

    pub fn find_workspace_by_id(&self, id: WorkspaceId) -> Option<(usize, &Workspace<W>)> {
        match &self.monitor_set {
            MonitorSet::Normal { ref monitors, .. } => {
                for mon in monitors {
                    if let Some((index, workspace)) = mon
                        .workspaces
                        .iter()
                        .enumerate()
                        .find(|(_, w)| w.id() == id)
                    {
                        return Some((index, workspace));
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces } => {
                if let Some((index, workspace)) =
                    workspaces.iter().enumerate().find(|(_, w)| w.id() == id)
                {
                    return Some((index, workspace));
                }
            }
        }

        None
    }

    pub fn find_workspace_by_name(&self, workspace_name: &str) -> Option<(usize, &Workspace<W>)> {
        match &self.monitor_set {
            MonitorSet::Normal { ref monitors, .. } => {
                for mon in monitors {
                    if let Some((index, workspace)) =
                        mon.workspaces.iter().enumerate().find(|(_, w)| {
                            w.name
                                .as_ref()
                                .map_or(false, |name| name.eq_ignore_ascii_case(workspace_name))
                        })
                    {
                        return Some((index, workspace));
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces } => {
                if let Some((index, workspace)) = workspaces.iter().enumerate().find(|(_, w)| {
                    w.name
                        .as_ref()
                        .map_or(false, |name| name.eq_ignore_ascii_case(workspace_name))
                }) {
                    return Some((index, workspace));
                }
            }
        }

        None
    }

    pub fn unname_workspace(&mut self, workspace_name: &str) {
        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    if mon.unname_workspace(workspace_name) {
                        if mon.workspace_switch.is_none() {
                            mon.clean_up_workspaces();
                        }
                        return;
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces } => {
                for (idx, ws) in workspaces.iter_mut().enumerate() {
                    if ws
                        .name
                        .as_ref()
                        .map_or(false, |name| name.eq_ignore_ascii_case(workspace_name))
                    {
                        ws.unname();

                        // Clean up empty workspaces.
                        if !ws.has_windows() {
                            workspaces.remove(idx);
                        }

                        return;
                    }
                }
            }
        }
    }

    pub fn find_window_and_output_mut(
        &mut self,
        wl_surface: &WlSurface,
    ) -> Option<(&mut W, Option<&Output>)> {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if move_.tile.window().is_wl_surface(wl_surface) {
                return Some((move_.tile.window_mut(), Some(&move_.output)));
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        if let Some(window) = ws.find_wl_surface_mut(wl_surface) {
                            return Some((window, Some(&mon.output)));
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces } => {
                for ws in workspaces {
                    if let Some(window) = ws.find_wl_surface_mut(wl_surface) {
                        return Some((window, None));
                    }
                }
            }
        }

        None
    }

    pub fn window_loc(&self, window: &W::Id) -> Option<Point<f64, Logical>> {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == window {
                return Some(move_.tile.window_loc());
            }
        }

        match &self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mon.workspaces {
                        for col in &ws.columns {
                            if let Some(idx) = col.position(window) {
                                return Some(col.window_loc(idx));
                            }
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    for col in &ws.columns {
                        if let Some(idx) = col.position(window) {
                            return Some(col.window_loc(idx));
                        }
                    }
                }
            }
        }

        None
    }

    pub fn update_output_size(&mut self, output: &Output) {
        let _span = tracy_client::span!("Layout::update_output_size");

        let MonitorSet::Normal { monitors, .. } = &mut self.monitor_set else {
            panic!()
        };

        for mon in monitors {
            if &mon.output == output {
                let scale = output.current_scale();
                let transform = output.current_transform();
                let view_size = output_size(output);
                let working_area = compute_working_area(output, self.options.struts);

                for ws in &mut mon.workspaces {
                    ws.set_view_size(scale, transform, view_size, working_area);
                }

                break;
            }
        }
    }

    pub fn scroll_amount_to_activate(&self, window: &W::Id) -> f64 {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == window {
                return 0.;
            }
        }

        let MonitorSet::Normal { monitors, .. } = &self.monitor_set else {
            return 0.;
        };

        for mon in monitors {
            for ws in &mon.workspaces {
                if ws.has_window(window) {
                    return ws.scroll_amount_to_activate(window);
                }
            }
        }

        0.
    }

    pub fn should_trigger_focus_follows_mouse_on(&self, window: &W::Id) -> bool {
        // During an animation, it's easy to trigger focus-follows-mouse on the previous workspace,
        // especially when clicking to switch workspace on a bar of some kind. This cancels the
        // workspace switch, which is annoying and not intended.
        //
        // This function allows focus-follows-mouse to trigger only on the animation target
        // workspace.
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == window {
                return true;
            }
        }

        let MonitorSet::Normal { monitors, .. } = &self.monitor_set else {
            return true;
        };

        let (mon, ws_idx) = monitors
            .iter()
            .find_map(|mon| {
                mon.workspaces
                    .iter()
                    .position(|ws| ws.has_window(window))
                    .map(|ws_idx| (mon, ws_idx))
            })
            .unwrap();

        // During a gesture, focus-follows-mouse does not cause any unintended workspace switches.
        if let Some(WorkspaceSwitch::Gesture(_)) = mon.workspace_switch {
            return true;
        }

        ws_idx == mon.active_workspace_idx
    }

    pub fn activate_window(&mut self, window: &W::Id) {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == window {
                return;
            }
        }

        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        else {
            return;
        };

        for (monitor_idx, mon) in monitors.iter_mut().enumerate() {
            for (workspace_idx, ws) in mon.workspaces.iter_mut().enumerate() {
                if ws.has_window(window) {
                    *active_monitor_idx = monitor_idx;
                    ws.activate_window(window);

                    // If currently in the middle of a vertical swipe between the target workspace
                    // and some other, don't switch the workspace.
                    match &mon.workspace_switch {
                        Some(WorkspaceSwitch::Gesture(gesture))
                            if gesture.current_idx.floor() == workspace_idx as f64
                                || gesture.current_idx.ceil() == workspace_idx as f64 => {}
                        _ => mon.switch_workspace(workspace_idx),
                    }

                    break;
                }
            }
        }
    }

    pub fn activate_output(&mut self, output: &Output) {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        else {
            return;
        };

        let idx = monitors
            .iter()
            .position(|mon| &mon.output == output)
            .unwrap();
        *active_monitor_idx = idx;
    }

    pub fn active_output(&self) -> Option<&Output> {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &self.monitor_set
        else {
            return None;
        };

        Some(&monitors[*active_monitor_idx].output)
    }

    pub fn active_workspace(&self) -> Option<&Workspace<W>> {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &self.monitor_set
        else {
            return None;
        };

        let mon = &monitors[*active_monitor_idx];
        Some(&mon.workspaces[mon.active_workspace_idx])
    }

    pub fn active_workspace_mut(&mut self) -> Option<&mut Workspace<W>> {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        else {
            return None;
        };

        let mon = &mut monitors[*active_monitor_idx];
        Some(&mut mon.workspaces[mon.active_workspace_idx])
    }

    pub fn active_window(&self) -> Option<(&W, &Output)> {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            return Some((move_.tile.window(), &move_.output));
        }

        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &self.monitor_set
        else {
            return None;
        };

        let mon = &monitors[*active_monitor_idx];
        let ws = &mon.workspaces[mon.active_workspace_idx];

        if ws.columns.is_empty() {
            return None;
        }

        let col = &ws.columns[ws.active_column_idx];
        Some((col.tiles[col.active_tile_idx].window(), &mon.output))
    }

    pub fn windows_for_output(&self, output: &Output) -> impl Iterator<Item = &W> + '_ {
        let MonitorSet::Normal { monitors, .. } = &self.monitor_set else {
            panic!()
        };

        let moving_window = self
            .interactive_move
            .as_ref()
            .and_then(|x| x.moving())
            .filter(|move_| move_.output == *output)
            .map(|move_| move_.tile.window())
            .into_iter();

        let mon = monitors.iter().find(|mon| &mon.output == output).unwrap();
        let mon_windows = mon.workspaces.iter().flat_map(|ws| ws.windows());

        moving_window.chain(mon_windows)
    }

    pub fn with_windows(&self, mut f: impl FnMut(&W, Option<&Output>, Option<WorkspaceId>)) {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            f(move_.tile.window(), Some(&move_.output), None);
        }

        match &self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mon.workspaces {
                        for win in ws.windows() {
                            f(win, Some(&mon.output), Some(ws.id()));
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces } => {
                for ws in workspaces {
                    for win in ws.windows() {
                        f(win, None, Some(ws.id()));
                    }
                }
            }
        }
    }

    pub fn with_windows_mut(&mut self, mut f: impl FnMut(&mut W, Option<&Output>)) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            f(move_.tile.window_mut(), Some(&move_.output));
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        for win in ws.windows_mut() {
                            f(win, Some(&mon.output));
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces } => {
                for ws in workspaces {
                    for win in ws.windows_mut() {
                        f(win, None);
                    }
                }
            }
        }
    }

    fn active_monitor(&mut self) -> Option<&mut Monitor<W>> {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        else {
            return None;
        };

        Some(&mut monitors[*active_monitor_idx])
    }

    pub fn active_monitor_ref(&self) -> Option<&Monitor<W>> {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &self.monitor_set
        else {
            return None;
        };

        Some(&monitors[*active_monitor_idx])
    }

    pub fn monitor_for_output(&self, output: &Output) -> Option<&Monitor<W>> {
        let MonitorSet::Normal { monitors, .. } = &self.monitor_set else {
            return None;
        };

        monitors.iter().find(|mon| &mon.output == output)
    }

    pub fn monitor_for_output_mut(&mut self, output: &Output) -> Option<&mut Monitor<W>> {
        let MonitorSet::Normal { monitors, .. } = &mut self.monitor_set else {
            return None;
        };

        monitors.iter_mut().find(|mon| &mon.output == output)
    }

    pub fn monitor_for_workspace(&self, workspace_name: &str) -> Option<&Monitor<W>> {
        let MonitorSet::Normal { monitors, .. } = &self.monitor_set else {
            return None;
        };

        monitors.iter().find(|monitor| {
            monitor.workspaces.iter().any(|ws| {
                ws.name
                    .as_ref()
                    .map_or(false, |name| name.eq_ignore_ascii_case(workspace_name))
            })
        })
    }

    pub fn outputs(&self) -> impl Iterator<Item = &Output> + '_ {
        let monitors = if let MonitorSet::Normal { monitors, .. } = &self.monitor_set {
            &monitors[..]
        } else {
            &[][..]
        };

        monitors.iter().map(|mon| &mon.output)
    }

    pub fn move_left(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_left();
    }

    pub fn move_right(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_right();
    }

    pub fn move_column_to_first(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_column_to_first();
    }

    pub fn move_column_to_last(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_column_to_last();
    }

    pub fn move_column_left_or_to_output(&mut self, output: &Output) -> bool {
        if let Some(monitor) = self.active_monitor() {
            let workspace = monitor.active_workspace();
            let curr_idx = workspace.active_column_idx;

            if !workspace.columns.is_empty() && curr_idx != 0 {
                monitor.move_left();
                return false;
            }
        }

        self.move_column_to_output(output);
        true
    }

    pub fn move_column_right_or_to_output(&mut self, output: &Output) -> bool {
        if let Some(monitor) = self.active_monitor() {
            let workspace = monitor.active_workspace();
            let curr_idx = workspace.active_column_idx;

            if !workspace.columns.is_empty() && curr_idx != workspace.columns.len() - 1 {
                monitor.move_right();
                return false;
            }
        }

        self.move_column_to_output(output);
        true
    }

    pub fn move_down(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_down();
    }

    pub fn move_up(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_up();
    }

    pub fn move_down_or_to_workspace_down(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_down_or_to_workspace_down();
    }

    pub fn move_up_or_to_workspace_up(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_up_or_to_workspace_up();
    }

    pub fn consume_or_expel_window_left(&mut self, window: Option<&W::Id>) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window == Some(move_.tile.window().id()) {
                return;
            }
        }

        let workspace = if let Some(window) = window {
            Some(
                self.workspaces_mut()
                    .find(|ws| ws.has_window(window))
                    .unwrap(),
            )
        } else {
            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.consume_or_expel_window_left(window);
    }

    pub fn consume_or_expel_window_right(&mut self, window: Option<&W::Id>) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window == Some(move_.tile.window().id()) {
                return;
            }
        }

        let workspace = if let Some(window) = window {
            Some(
                self.workspaces_mut()
                    .find(|ws| ws.has_window(window))
                    .unwrap(),
            )
        } else {
            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.consume_or_expel_window_right(window);
    }

    pub fn focus_left(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.focus_left();
    }

    pub fn focus_right(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.focus_right();
    }

    pub fn focus_column_first(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.focus_column_first();
    }

    pub fn focus_column_last(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.focus_column_last();
    }

    pub fn focus_column_right_or_first(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.focus_column_right_or_first();
    }

    pub fn focus_column_left_or_last(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.focus_column_left_or_last();
    }

    pub fn focus_window_up_or_output(&mut self, output: &Output) -> bool {
        if let Some(monitor) = self.active_monitor() {
            let workspace = monitor.active_workspace();

            if !workspace.columns.is_empty() {
                let curr_idx = workspace.columns[workspace.active_column_idx].active_tile_idx;
                let new_idx = curr_idx.saturating_sub(1);
                if curr_idx != new_idx {
                    workspace.focus_up();
                    return false;
                }
            }
        }

        self.focus_output(output);
        true
    }

    pub fn focus_window_down_or_output(&mut self, output: &Output) -> bool {
        if let Some(monitor) = self.active_monitor() {
            let workspace = monitor.active_workspace();

            if !workspace.columns.is_empty() {
                let column = &workspace.columns[workspace.active_column_idx];
                let curr_idx = column.active_tile_idx;
                let new_idx = min(column.active_tile_idx + 1, column.tiles.len() - 1);
                if curr_idx != new_idx {
                    workspace.focus_down();
                    return false;
                }
            }
        }

        self.focus_output(output);
        true
    }

    pub fn focus_column_left_or_output(&mut self, output: &Output) -> bool {
        if let Some(monitor) = self.active_monitor() {
            let workspace = monitor.active_workspace();
            let curr_idx = workspace.active_column_idx;

            if !workspace.columns.is_empty() && curr_idx != 0 {
                monitor.focus_left();
                return false;
            }
        }

        self.focus_output(output);
        true
    }

    pub fn focus_column_right_or_output(&mut self, output: &Output) -> bool {
        if let Some(monitor) = self.active_monitor() {
            let workspace = monitor.active_workspace();
            let curr_idx = workspace.active_column_idx;
            let columns = &workspace.columns;

            if !workspace.columns.is_empty() && curr_idx != columns.len() - 1 {
                monitor.focus_right();
                return false;
            }
        }

        self.focus_output(output);
        true
    }

    pub fn focus_down(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.focus_down();
    }

    pub fn focus_up(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.focus_up();
    }

    pub fn focus_down_or_left(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.focus_down_or_left();
    }

    pub fn focus_down_or_right(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.focus_down_or_right();
    }

    pub fn focus_up_or_left(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.focus_up_or_left();
    }

    pub fn focus_up_or_right(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.focus_up_or_right();
    }

    pub fn focus_window_or_workspace_down(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.focus_window_or_workspace_down();
    }

    pub fn focus_window_or_workspace_up(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.focus_window_or_workspace_up();
    }

    pub fn move_to_workspace_up(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_to_workspace_up();
    }

    pub fn move_to_workspace_down(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_to_workspace_down();
    }

    pub fn move_to_workspace(&mut self, window: Option<&W::Id>, idx: usize) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window == Some(move_.tile.window().id()) {
                return;
            }
        }

        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_to_workspace(window, idx);
    }

    pub fn move_column_to_workspace_up(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_column_to_workspace_up();
    }

    pub fn move_column_to_workspace_down(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_column_to_workspace_down();
    }

    pub fn move_column_to_workspace(&mut self, idx: usize) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_column_to_workspace(idx);
    }

    pub fn move_column_to_workspace_on_output(&mut self, output: &Output, idx: usize) {
        self.move_column_to_output(output);
        self.focus_output(output);
        self.move_column_to_workspace(idx);
    }

    pub fn switch_workspace_up(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.switch_workspace_up();
    }

    pub fn switch_workspace_down(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.switch_workspace_down();
    }

    pub fn switch_workspace(&mut self, idx: usize) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.switch_workspace(idx);
    }

    pub fn switch_workspace_auto_back_and_forth(&mut self, idx: usize) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.switch_workspace_auto_back_and_forth(idx);
    }

    pub fn switch_workspace_previous(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.switch_workspace_previous();
    }

    pub fn consume_into_column(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.consume_into_column();
    }

    pub fn expel_from_column(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.expel_from_column();
    }

    pub fn center_column(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.center_column();
    }

    pub fn focus(&self) -> Option<&W> {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            return Some(move_.tile.window());
        }

        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &self.monitor_set
        else {
            return None;
        };

        monitors[*active_monitor_idx].focus()
    }

    /// Returns the window under the cursor and the position of its toplevel surface within the
    /// output.
    ///
    /// `Some((w, Some(p)))` means that the cursor is within the window's input region and can be
    /// used for delivering events to the window. `Some((w, None))` means that the cursor is within
    /// the window's activation region, but not within the window's input region. For example, the
    /// cursor may be on the window's server-side border.
    pub fn window_under(
        &self,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<(&W, Option<Point<f64, Logical>>)> {
        let MonitorSet::Normal { monitors, .. } = &self.monitor_set else {
            return None;
        };

        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            let tile_pos = move_.tile_render_location();
            let pos_within_tile = pos_within_output - tile_pos;

            if move_.tile.is_in_input_region(pos_within_tile) {
                let pos_within_surface = tile_pos + move_.tile.buf_loc();
                return Some((move_.tile.window(), Some(pos_within_surface)));
            } else if move_.tile.is_in_activation_region(pos_within_tile) {
                return Some((move_.tile.window(), None));
            }

            return None;
        };

        let mon = monitors.iter().find(|mon| &mon.output == output)?;
        mon.window_under(pos_within_output)
    }

    pub fn resize_edges_under(
        &self,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<ResizeEdge> {
        let MonitorSet::Normal { monitors, .. } = &self.monitor_set else {
            return None;
        };

        let mon = monitors.iter().find(|mon| &mon.output == output)?;
        mon.resize_edges_under(pos_within_output)
    }

    #[cfg(test)]
    fn verify_invariants(&self) {
        use std::collections::HashSet;

        use approx::assert_abs_diff_eq;

        use crate::layout::monitor::WorkspaceSwitch;

        let mut move_win_id = None;
        if let Some(state) = &self.interactive_move {
            match state {
                InteractiveMoveState::Starting {
                    window_id,
                    pointer_delta: _,
                    pointer_ratio_within_window: _,
                } => {
                    assert!(
                        self.has_window(window_id),
                        "interactive move must be on an existing window"
                    );
                    move_win_id = Some(window_id.clone());
                }
                InteractiveMoveState::Moving(move_) => {
                    let scale = move_.output.current_scale().fractional_scale();
                    let options = Options::clone(&self.options).adjusted_for_scale(scale);
                    assert_eq!(
                        &*move_.tile.options, &options,
                        "interactive moved tile options must be \
                         base options adjusted for output scale"
                    );

                    let tile_pos = move_.tile_render_location();
                    let rounded_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);

                    // Tile position must be rounded to physical pixels.
                    assert_abs_diff_eq!(tile_pos.x, rounded_pos.x, epsilon = 1e-5);
                    assert_abs_diff_eq!(tile_pos.y, rounded_pos.y, epsilon = 1e-5);
                }
            }
        }

        let mut seen_workspace_id = HashSet::new();
        let mut seen_workspace_name = Vec::<String>::new();

        let (monitors, &primary_idx, &active_monitor_idx) = match &self.monitor_set {
            MonitorSet::Normal {
                monitors,
                primary_idx,
                active_monitor_idx,
            } => (monitors, primary_idx, active_monitor_idx),
            MonitorSet::NoOutputs { workspaces } => {
                for workspace in workspaces {
                    assert!(
                        workspace.has_windows() || workspace.name.is_some(),
                        "with no outputs there cannot be empty unnamed workspaces"
                    );

                    assert_eq!(
                        workspace.base_options, self.options,
                        "workspace base options must be synchronized with layout"
                    );

                    let options = Options::clone(&workspace.base_options)
                        .adjusted_for_scale(workspace.scale().fractional_scale());
                    assert_eq!(
                        &*workspace.options, &options,
                        "workspace options must be base options adjusted for workspace scale"
                    );

                    assert!(
                        seen_workspace_id.insert(workspace.id()),
                        "workspace id must be unique"
                    );

                    if let Some(name) = &workspace.name {
                        assert!(
                            !seen_workspace_name
                                .iter()
                                .any(|n| n.eq_ignore_ascii_case(name)),
                            "workspace name must be unique"
                        );
                        seen_workspace_name.push(name.clone());
                    }

                    workspace.verify_invariants(move_win_id.as_ref());
                }

                return;
            }
        };

        assert!(primary_idx < monitors.len());
        assert!(active_monitor_idx < monitors.len());

        for (idx, monitor) in monitors.iter().enumerate() {
            assert!(
                !monitor.workspaces.is_empty(),
                "monitor must have at least one workspace"
            );
            assert!(monitor.active_workspace_idx < monitor.workspaces.len());

            assert_eq!(
                monitor.options, self.options,
                "monitor options must be synchronized with layout"
            );

            if let Some(WorkspaceSwitch::Animation(anim)) = &monitor.workspace_switch {
                let before_idx = anim.from() as usize;
                let after_idx = anim.to() as usize;

                assert!(before_idx < monitor.workspaces.len());
                assert!(after_idx < monitor.workspaces.len());
            }

            if idx == primary_idx {
                for ws in &monitor.workspaces {
                    if ws.original_output.matches(&monitor.output) {
                        // This is the primary monitor's own workspace.
                        continue;
                    }

                    let own_monitor_exists = monitors
                        .iter()
                        .any(|m| ws.original_output.matches(&m.output));
                    assert!(
                        !own_monitor_exists,
                        "primary monitor cannot have workspaces for which their own monitor exists"
                    );
                }
            } else {
                assert!(
                    monitor
                        .workspaces
                        .iter()
                        .any(|workspace| workspace.original_output.matches(&monitor.output)),
                    "secondary monitor must not have any non-own workspaces"
                );
            }

            assert!(
                monitor.workspaces.last().unwrap().columns.is_empty(),
                "monitor must have an empty workspace in the end"
            );

            assert!(
                monitor.workspaces.last().unwrap().name.is_none(),
                "monitor must have an unnamed workspace in the end"
            );

            // If there's no workspace switch in progress, there can't be any non-last non-active
            // empty workspaces.
            if monitor.workspace_switch.is_none() {
                for (idx, ws) in monitor.workspaces.iter().enumerate().rev().skip(1) {
                    if idx != monitor.active_workspace_idx {
                        assert!(
                            !ws.columns.is_empty() || ws.name.is_some(),
                            "non-active workspace can't be empty and unnamed except the last one"
                        );
                    }
                }
            }

            // FIXME: verify that primary doesn't have any workspaces for which their own monitor
            // exists.

            for workspace in &monitor.workspaces {
                assert_eq!(
                    workspace.base_options, self.options,
                    "workspace options must be synchronized with layout"
                );

                let options = Options::clone(&workspace.base_options)
                    .adjusted_for_scale(workspace.scale().fractional_scale());
                assert_eq!(
                    &*workspace.options, &options,
                    "workspace options must be base options adjusted for workspace scale"
                );

                assert!(
                    seen_workspace_id.insert(workspace.id()),
                    "workspace id must be unique"
                );

                if let Some(name) = &workspace.name {
                    assert!(
                        !seen_workspace_name
                            .iter()
                            .any(|n| n.eq_ignore_ascii_case(name)),
                        "workspace name must be unique"
                    );
                    seen_workspace_name.push(name.clone());
                }

                workspace.verify_invariants(move_win_id.as_ref());
            }
        }
    }

    pub fn advance_animations(&mut self, current_time: Duration) {
        let _span = tracy_client::span!("Layout::advance_animations");

        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            move_.tile.advance_animations(current_time);
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    mon.advance_animations(current_time);
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    ws.advance_animations(current_time);
                }
            }
        }
    }

    pub fn are_animations_ongoing(&self, output: Option<&Output>) -> bool {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.are_animations_ongoing() {
                return true;
            }
        }

        let MonitorSet::Normal { monitors, .. } = &self.monitor_set else {
            return false;
        };

        for mon in monitors {
            if output.map_or(false, |output| mon.output != *output) {
                continue;
            }

            if mon.are_animations_ongoing() {
                return true;
            }
        }

        false
    }

    pub fn update_render_elements(&mut self, output: Option<&Output>) {
        let _span = tracy_client::span!("Layout::update_render_elements");

        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if output.map_or(true, |output| move_.output == *output) {
                let pos_within_output = move_.tile_render_location();
                let view_rect = Rectangle::from_loc_and_size(
                    pos_within_output.upscale(-1.),
                    output_size(&move_.output),
                );
                move_.tile.update(true, view_rect);
            }
        }

        self.update_insert_hint(output);

        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        else {
            error!("update_render_elements called with no monitors");
            return;
        };

        for (idx, mon) in monitors.iter_mut().enumerate() {
            if output.map_or(true, |output| mon.output == *output) {
                let is_active = self.is_active
                    && idx == *active_monitor_idx
                    && !matches!(self.interactive_move, Some(InteractiveMoveState::Moving(_)));
                mon.update_render_elements(is_active);
            }
        }
    }

    pub fn update_shaders(&mut self) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            move_.tile.update_shaders();
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        ws.update_shaders();
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    ws.update_shaders();
                }
            }
        }
    }

    fn update_insert_hint(&mut self, output: Option<&Output>) {
        let _span = tracy_client::span!("Layout::update_insert_hint");

        let _span = tracy_client::span!("Layout::update_insert_hint::clear");
        for ws in self.workspaces_mut() {
            ws.clear_insert_hint();
        }

        if !matches!(self.interactive_move, Some(InteractiveMoveState::Moving(_))) {
            return;
        }
        let Some(InteractiveMoveState::Moving(move_)) = self.interactive_move.take() else {
            unreachable!()
        };
        if output.map_or(false, |out| &move_.output != out) {
            self.interactive_move = Some(InteractiveMoveState::Moving(move_));
            return;
        }

        let _span = tracy_client::span!("Layout::update_insert_hint::update");

        if let Some(mon) = self.monitor_for_output_mut(&move_.output) {
            if let Some((ws, offset)) = mon.workspace_under(move_.pointer_pos_within_output) {
                let ws_id = ws.id();
                let ws = mon
                    .workspaces
                    .iter_mut()
                    .find(|ws| ws.id() == ws_id)
                    .unwrap();

                let position = ws.get_insert_position(move_.pointer_pos_within_output - offset);

                let rules = move_.tile.window().rules();
                let border_width = move_.tile.effective_border_width().unwrap_or(0.);
                let corner_radius = rules
                    .geometry_corner_radius
                    .map_or(CornerRadius::default(), |radius| {
                        radius.expanded_by(border_width as f32)
                    });

                ws.set_insert_hint(InsertHint {
                    position,
                    width: move_.width,
                    is_full_width: move_.is_full_width,
                    corner_radius,
                });
            }
        }

        self.interactive_move = Some(InteractiveMoveState::Moving(move_));
    }

    pub fn ensure_named_workspace(&mut self, ws_config: &WorkspaceConfig) {
        if self.find_workspace_by_name(&ws_config.name.0).is_some() {
            return;
        }

        let options = self.options.clone();

        match &mut self.monitor_set {
            MonitorSet::Normal {
                monitors,
                primary_idx,
                active_monitor_idx,
            } => {
                let mon_idx = ws_config
                    .open_on_output
                    .as_deref()
                    .map(|name| {
                        monitors
                            .iter_mut()
                            .position(|monitor| output_matches_name(&monitor.output, name))
                            .unwrap_or(*primary_idx)
                    })
                    .unwrap_or(*active_monitor_idx);
                let mon = &mut monitors[mon_idx];

                let ws = Workspace::new_with_config(
                    mon.output.clone(),
                    Some(ws_config.clone()),
                    options,
                );
                mon.workspaces.insert(0, ws);
                mon.active_workspace_idx += 1;
                mon.workspace_switch = None;
                mon.clean_up_workspaces();
            }
            MonitorSet::NoOutputs { workspaces } => {
                let ws = Workspace::new_with_config_no_outputs(Some(ws_config.clone()), options);
                workspaces.insert(0, ws);
            }
        }
    }

    pub fn update_config(&mut self, config: &Config) {
        let options = Rc::new(Options::from_config(config));

        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            let scale = move_.output.current_scale().fractional_scale();
            move_.tile.update_config(scale, options.clone());
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    mon.update_config(options.clone());
                }
            }
            MonitorSet::NoOutputs { workspaces } => {
                for ws in workspaces {
                    ws.update_config(options.clone());
                }
            }
        }

        self.options = options;
    }

    pub fn toggle_width(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.toggle_width();
    }

    pub fn toggle_window_height(&mut self, window: Option<&W::Id>) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window == Some(move_.tile.window().id()) {
                return;
            }
        }

        let workspace = if let Some(window) = window {
            Some(
                self.workspaces_mut()
                    .find(|ws| ws.has_window(window))
                    .unwrap(),
            )
        } else {
            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.toggle_window_height(window);
    }

    pub fn toggle_full_width(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.toggle_full_width();
    }

    pub fn set_column_width(&mut self, change: SizeChange) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.set_column_width(change);
    }

    pub fn set_window_height(&mut self, window: Option<&W::Id>, change: SizeChange) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window == Some(move_.tile.window().id()) {
                return;
            }
        }

        let workspace = if let Some(window) = window {
            Some(
                self.workspaces_mut()
                    .find(|ws| ws.has_window(window))
                    .unwrap(),
            )
        } else {
            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.set_window_height(window, change);
    }

    pub fn reset_window_height(&mut self, window: Option<&W::Id>) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window == Some(move_.tile.window().id()) {
                return;
            }
        }

        let workspace = if let Some(window) = window {
            Some(
                self.workspaces_mut()
                    .find(|ws| ws.has_window(window))
                    .unwrap(),
            )
        } else {
            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.reset_window_height(window);
    }

    pub fn focus_output(&mut self, output: &Output) {
        if let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        {
            for (idx, mon) in monitors.iter().enumerate() {
                if &mon.output == output {
                    *active_monitor_idx = idx;
                    return;
                }
            }
        }
    }

    pub fn move_to_output(
        &mut self,
        window: Option<&W::Id>,
        output: &Output,
        target_ws_idx: Option<usize>,
    ) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window == Some(move_.tile.window().id()) {
                return;
            }
        }

        if let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        {
            let new_idx = monitors
                .iter()
                .position(|mon| &mon.output == output)
                .unwrap();

            let (mon_idx, ws_idx, col_idx, tile_idx) = if let Some(window) = window {
                monitors
                    .iter()
                    .enumerate()
                    .find_map(|(mon_idx, mon)| {
                        mon.workspaces.iter().enumerate().find_map(|(ws_idx, ws)| {
                            ws.columns.iter().enumerate().find_map(|(col_idx, col)| {
                                col.tiles
                                    .iter()
                                    .position(|tile| tile.window().id() == window)
                                    .map(|tile_idx| (mon_idx, ws_idx, col_idx, tile_idx))
                            })
                        })
                    })
                    .unwrap()
            } else {
                let mon_idx = *active_monitor_idx;
                let mon = &monitors[mon_idx];
                let ws_idx = mon.active_workspace_idx;
                let ws = &mon.workspaces[ws_idx];

                if ws.columns.is_empty() {
                    return;
                }

                let col_idx = ws.active_column_idx;
                let tile_idx = ws.columns[col_idx].active_tile_idx;
                (mon_idx, ws_idx, col_idx, tile_idx)
            };

            let workspace_idx = target_ws_idx.unwrap_or(monitors[new_idx].active_workspace_idx);
            if mon_idx == new_idx && ws_idx == workspace_idx {
                return;
            }

            let mon = &mut monitors[mon_idx];
            let ws = &mut mon.workspaces[ws_idx];
            let column = &ws.columns[col_idx];
            let activate = mon_idx == *active_monitor_idx
                && ws_idx == mon.active_workspace_idx
                && col_idx == ws.active_column_idx
                && tile_idx == column.active_tile_idx;

            let removed = ws.remove_tile_by_idx(col_idx, tile_idx, Transaction::new(), None);

            self.add_window_by_idx(
                new_idx,
                workspace_idx,
                removed.tile.into_window(),
                activate,
                removed.width,
                removed.is_full_width,
            );

            let MonitorSet::Normal { monitors, .. } = &mut self.monitor_set else {
                unreachable!()
            };
            let mon = &mut monitors[mon_idx];
            if mon.workspace_switch.is_none() {
                monitors[mon_idx].clean_up_workspaces();
            }
        }
    }

    pub fn move_column_to_output(&mut self, output: &Output) {
        if let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        {
            let new_idx = monitors
                .iter()
                .position(|mon| &mon.output == output)
                .unwrap();

            let current = &mut monitors[*active_monitor_idx];
            let ws = current.active_workspace();
            if !ws.has_windows() {
                return;
            }
            let column = ws.remove_column_by_idx(ws.active_column_idx, None);

            let workspace_idx = monitors[new_idx].active_workspace_idx;
            self.add_column_by_idx(new_idx, workspace_idx, column, true);
        }
    }

    pub fn move_workspace_to_output(&mut self, output: &Output) {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        else {
            return;
        };

        let current = &mut monitors[*active_monitor_idx];
        if current.active_workspace_idx == current.workspaces.len() - 1 {
            // Insert a new empty workspace.
            let ws = Workspace::new(current.output.clone(), current.options.clone());
            current.workspaces.push(ws);
        }
        let mut ws = current.workspaces.remove(current.active_workspace_idx);
        current.active_workspace_idx = current.active_workspace_idx.saturating_sub(1);
        current.workspace_switch = None;
        current.clean_up_workspaces();

        ws.set_output(Some(output.clone()));
        ws.original_output = OutputId::new(output);

        let target_idx = monitors
            .iter()
            .position(|mon| &mon.output == output)
            .unwrap();
        let target = &mut monitors[target_idx];

        target.previous_workspace_id = Some(target.workspaces[target.active_workspace_idx].id());

        // Insert the workspace after the currently active one. Unless the currently active one is
        // the last empty workspace, then insert before.
        let target_ws_idx = min(target.active_workspace_idx + 1, target.workspaces.len() - 1);
        target.workspaces.insert(target_ws_idx, ws);
        target.active_workspace_idx = target_ws_idx;
        target.workspace_switch = None;
        target.clean_up_workspaces();

        *active_monitor_idx = target_idx;
    }

    pub fn set_fullscreen(&mut self, window: &W::Id, is_fullscreen: bool) {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == window {
                return;
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        if ws.has_window(window) {
                            ws.set_fullscreen(window, is_fullscreen);
                            return;
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    if ws.has_window(window) {
                        ws.set_fullscreen(window, is_fullscreen);
                        return;
                    }
                }
            }
        }
    }

    pub fn toggle_fullscreen(&mut self, window: &W::Id) {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == window {
                return;
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        if ws.has_window(window) {
                            ws.toggle_fullscreen(window);
                            return;
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    if ws.has_window(window) {
                        ws.toggle_fullscreen(window);
                        return;
                    }
                }
            }
        }
    }

    pub fn workspace_switch_gesture_begin(&mut self, output: &Output, is_touchpad: bool) {
        let monitors = match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => monitors,
            MonitorSet::NoOutputs { .. } => unreachable!(),
        };

        for monitor in monitors {
            // Cancel the gesture on other outputs.
            if &monitor.output != output {
                monitor.workspace_switch_gesture_end(true, None);
                continue;
            }

            monitor.workspace_switch_gesture_begin(is_touchpad);
        }
    }

    pub fn workspace_switch_gesture_update(
        &mut self,
        delta_y: f64,
        timestamp: Duration,
        is_touchpad: bool,
    ) -> Option<Option<Output>> {
        let monitors = match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => monitors,
            MonitorSet::NoOutputs { .. } => return None,
        };

        for monitor in monitors {
            if let Some(refresh) =
                monitor.workspace_switch_gesture_update(delta_y, timestamp, is_touchpad)
            {
                if refresh {
                    return Some(Some(monitor.output.clone()));
                } else {
                    return Some(None);
                }
            }
        }

        None
    }

    pub fn workspace_switch_gesture_end(
        &mut self,
        cancelled: bool,
        is_touchpad: Option<bool>,
    ) -> Option<Output> {
        let monitors = match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => monitors,
            MonitorSet::NoOutputs { .. } => return None,
        };

        for monitor in monitors {
            if monitor.workspace_switch_gesture_end(cancelled, is_touchpad) {
                return Some(monitor.output.clone());
            }
        }

        None
    }

    pub fn view_offset_gesture_begin(&mut self, output: &Output, is_touchpad: bool) {
        let monitors = match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => monitors,
            MonitorSet::NoOutputs { .. } => unreachable!(),
        };

        for monitor in monitors {
            for (idx, ws) in monitor.workspaces.iter_mut().enumerate() {
                // Cancel the gesture on other workspaces.
                if &monitor.output != output || idx != monitor.active_workspace_idx {
                    ws.view_offset_gesture_end(true, None);
                    continue;
                }

                ws.view_offset_gesture_begin(is_touchpad);
            }
        }
    }

    pub fn view_offset_gesture_update(
        &mut self,
        delta_x: f64,
        timestamp: Duration,
        is_touchpad: bool,
    ) -> Option<Option<Output>> {
        let monitors = match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => monitors,
            MonitorSet::NoOutputs { .. } => return None,
        };

        for monitor in monitors {
            for ws in &mut monitor.workspaces {
                if let Some(refresh) =
                    ws.view_offset_gesture_update(delta_x, timestamp, is_touchpad)
                {
                    if refresh {
                        return Some(Some(monitor.output.clone()));
                    } else {
                        return Some(None);
                    }
                }
            }
        }

        None
    }

    pub fn view_offset_gesture_end(
        &mut self,
        cancelled: bool,
        is_touchpad: Option<bool>,
    ) -> Option<Output> {
        let monitors = match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => monitors,
            MonitorSet::NoOutputs { .. } => return None,
        };

        for monitor in monitors {
            for ws in &mut monitor.workspaces {
                if ws.view_offset_gesture_end(cancelled, is_touchpad) {
                    return Some(monitor.output.clone());
                }
            }
        }

        None
    }

    pub fn interactive_move_begin(
        &mut self,
        window_id: W::Id,
        output: &Output,
        start_pos_within_output: Point<f64, Logical>,
    ) -> bool {
        if self.interactive_move.is_some() {
            return false;
        }

        let MonitorSet::Normal { monitors, .. } = &mut self.monitor_set else {
            return false;
        };

        let Some((mon, (ws, ws_offset))) = monitors.iter().find_map(|mon| {
            mon.workspaces_with_render_positions()
                .find(|(ws, _)| ws.has_window(&window_id))
                .map(|rv| (mon, rv))
        }) else {
            return false;
        };

        if mon.output() != output {
            return false;
        }

        let (tile, tile_offset) = ws
            .tiles_with_render_positions()
            .find(|(tile, _)| tile.window().id() == &window_id)
            .unwrap();
        let window_offset = tile.window_loc();

        let tile_pos = ws_offset + tile_offset;

        let pointer_offset_within_window = start_pos_within_output - tile_pos - window_offset;
        let window_size = tile.window_size();
        let pointer_ratio_within_window = (
            f64::clamp(pointer_offset_within_window.x / window_size.w, 0., 1.),
            f64::clamp(pointer_offset_within_window.y / window_size.h, 0., 1.),
        );

        self.interactive_move = Some(InteractiveMoveState::Starting {
            window_id,
            pointer_delta: Point::from((0., 0.)),
            pointer_ratio_within_window,
        });

        true
    }

    pub fn interactive_move_update(
        &mut self,
        window: &W::Id,
        delta: Point<f64, Logical>,
        output: Output,
        pointer_pos_within_output: Point<f64, Logical>,
    ) -> bool {
        let Some(state) = self.interactive_move.take() else {
            return false;
        };

        match state {
            InteractiveMoveState::Starting {
                window_id,
                mut pointer_delta,
                pointer_ratio_within_window,
            } => {
                if window_id != *window {
                    self.interactive_move = Some(InteractiveMoveState::Starting {
                        window_id,
                        pointer_delta,
                        pointer_ratio_within_window,
                    });
                    return false;
                }

                pointer_delta += delta;

                let (cx, cy) = (pointer_delta.x, pointer_delta.y);
                let sq_dist = cx * cx + cy * cy;

                let factor = RubberBand {
                    stiffness: 1.0,
                    limit: 0.5,
                }
                .band(sq_dist / INTERACTIVE_MOVE_START_THRESHOLD);

                let tile = self
                    .workspaces_mut()
                    .flat_map(|ws| ws.tiles_mut())
                    .find(|tile| *tile.window().id() == window_id)
                    .unwrap();
                tile.interactive_move_offset = pointer_delta.upscale(factor);

                // Put it back to be able to easily return.
                self.interactive_move = Some(InteractiveMoveState::Starting {
                    window_id: window_id.clone(),
                    pointer_delta,
                    pointer_ratio_within_window,
                });

                if sq_dist < INTERACTIVE_MOVE_START_THRESHOLD {
                    return true;
                }

                // If the pointer is currently on the window's own output, then we can animate the
                // window movement from its current (rubberbanded and possibly moved away) position
                // to the pointer. Otherwise, we just teleport it as the layout code is not aware
                // of monitor positions.
                //
                // FIXME: with floating layer, the layout code will know about monitor positions,
                // so this will be potentially animatable.
                let mut tile_pos = None;
                if let MonitorSet::Normal { monitors, .. } = &self.monitor_set {
                    if let Some((mon, (ws, ws_offset))) = monitors.iter().find_map(|mon| {
                        mon.workspaces_with_render_positions()
                            .find(|(ws, _)| ws.has_window(window))
                            .map(|rv| (mon, rv))
                    }) {
                        if mon.output() == &output {
                            let (_, tile_offset) = ws
                                .tiles_with_render_positions()
                                .find(|(tile, _)| tile.window().id() == window)
                                .unwrap();

                            tile_pos = Some(ws_offset + tile_offset);
                        }
                    }
                }

                let RemovedTile {
                    mut tile,
                    width,
                    is_full_width,
                } = self.remove_window(window, Transaction::new()).unwrap();

                tile.stop_move_animations();
                tile.interactive_move_offset = Point::from((0., 0.));
                tile.window().output_enter(&output);
                tile.window().set_preferred_scale_transform(
                    output.current_scale(),
                    output.current_transform(),
                );

                let scale = output.current_scale().fractional_scale();
                tile.update_config(
                    scale,
                    Rc::new(Options::clone(&self.options).adjusted_for_scale(scale)),
                );

                // Unfullscreen and let the window pick a natural size.
                //
                // When we have floating, we will want to always send a (0, 0) size here, not just
                // to unfullscreen. However, when implementing that, remember to check how GTK
                // tiled window size restoration works. It seems to remember *some* last size with
                // prefer-no-csd, and occasionally that last size can become the full-width size
                // rather than a smaller size, which is annoying. Need to see if niri can use some
                // heuristics to make this case behave better.
                if tile.window().is_pending_fullscreen() {
                    tile.window_mut()
                        .request_size(Size::from((0, 0)), true, None);
                }

                let mut data = InteractiveMoveData {
                    tile,
                    output,
                    pointer_pos_within_output,
                    width,
                    is_full_width,
                    pointer_ratio_within_window,
                };

                if let Some(tile_pos) = tile_pos {
                    let new_tile_pos = data.tile_render_location();
                    data.tile.animate_move_from(tile_pos - new_tile_pos);
                }

                self.interactive_move = Some(InteractiveMoveState::Moving(data));
            }
            InteractiveMoveState::Moving(mut move_) => {
                if window != move_.tile.window().id() {
                    self.interactive_move = Some(InteractiveMoveState::Moving(move_));
                    return false;
                }

                if output != move_.output {
                    move_.tile.window().output_leave(&move_.output);
                    move_.tile.window().output_enter(&output);
                    move_.tile.window().set_preferred_scale_transform(
                        output.current_scale(),
                        output.current_transform(),
                    );
                    let scale = output.current_scale().fractional_scale();
                    move_.tile.update_config(
                        scale,
                        Rc::new(Options::clone(&self.options).adjusted_for_scale(scale)),
                    );
                    move_.output = output.clone();
                    self.focus_output(&output);
                }

                move_.pointer_pos_within_output = pointer_pos_within_output;

                self.interactive_move = Some(InteractiveMoveState::Moving(move_));
            }
        }

        true
    }

    pub fn interactive_move_end(&mut self, window: &W::Id) {
        let Some(move_) = &self.interactive_move else {
            return;
        };

        let move_ = match move_ {
            InteractiveMoveState::Starting { window_id, .. } => {
                if window_id != window {
                    return;
                }

                let Some(InteractiveMoveState::Starting { window_id, .. }) =
                    self.interactive_move.take()
                else {
                    unreachable!()
                };

                let tile = self
                    .workspaces_mut()
                    .flat_map(|ws| ws.tiles_mut())
                    .find(|tile| *tile.window().id() == window_id)
                    .unwrap();
                let offset = tile.interactive_move_offset;
                tile.interactive_move_offset = Point::from((0., 0.));
                tile.animate_move_from(offset);

                return;
            }
            InteractiveMoveState::Moving(move_) => move_,
        };

        if window != move_.tile.window().id() {
            return;
        }

        let Some(InteractiveMoveState::Moving(move_)) = self.interactive_move.take() else {
            unreachable!()
        };

        match &mut self.monitor_set {
            MonitorSet::Normal {
                monitors,
                active_monitor_idx,
                ..
            } => {
                let (mon, ws_idx, position, offset) = if let Some(mon) =
                    monitors.iter_mut().find(|mon| mon.output == move_.output)
                {
                    let (ws, offset) = mon
                        .workspace_under(move_.pointer_pos_within_output)
                        // If the pointer is somehow outside the move output and a workspace switch
                        // is in progress, this won't necessarily do the expected thing, but also
                        // that is not really supposed to happen so eh?
                        .unwrap_or_else(|| mon.workspaces_with_render_positions().next().unwrap());

                    let ws_id = ws.id();
                    let ws_idx = mon
                        .workspaces
                        .iter_mut()
                        .position(|ws| ws.id() == ws_id)
                        .unwrap();

                    let ws = &mut mon.workspaces[ws_idx];
                    let position = ws.get_insert_position(move_.pointer_pos_within_output - offset);
                    (mon, ws_idx, position, offset)
                } else {
                    let mon = &mut monitors[*active_monitor_idx];
                    let ws_id = mon.active_workspace().id();
                    let (_, offset) = mon
                        .workspaces_with_render_positions()
                        .find(|(ws, _)| ws.id() == ws_id)
                        .unwrap();
                    let ws_idx = mon.active_workspace_idx();
                    let ws = &mut mon.workspaces[ws_idx];
                    // No point in trying to use the pointer position on the wrong output.
                    let position = InsertPosition::NewColumn(ws.columns.len());
                    (mon, ws_idx, position, offset)
                };

                let win_id = move_.tile.window().id().clone();
                let window_render_loc = move_.tile_render_location() + move_.tile.window_loc();

                match position {
                    InsertPosition::NewColumn(column_idx) => {
                        mon.add_tile(
                            ws_idx,
                            Some(column_idx),
                            move_.tile,
                            true,
                            move_.width,
                            move_.is_full_width,
                        );
                    }
                    InsertPosition::InColumn(column_idx, tile_idx) => {
                        mon.add_tile_to_column(
                            ws_idx,
                            column_idx,
                            Some(tile_idx),
                            move_.tile,
                            true,
                        );
                    }
                }

                let ws = &mut mon.workspaces[ws_idx];
                let (tile, tile_render_loc) = ws
                    .tiles_with_render_positions_mut(false)
                    .find(|(tile, _)| tile.window().id() == &win_id)
                    .unwrap();
                let new_window_render_loc = offset + tile_render_loc + tile.window_loc();

                tile.animate_move_from(window_render_loc - new_window_render_loc);
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                let ws = if let Some(ws) = workspaces.get_mut(0) {
                    ws
                } else {
                    workspaces.push(Workspace::new_no_outputs(self.options.clone()));
                    &mut workspaces[0]
                };

                // No point in trying to use the pointer position without outputs.
                ws.add_tile(
                    None,
                    move_.tile,
                    true,
                    move_.width,
                    move_.is_full_width,
                    None,
                );
            }
        }
    }

    pub fn interactive_resize_begin(&mut self, window: W::Id, edges: ResizeEdge) -> bool {
        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        if ws.has_window(&window) {
                            return ws.interactive_resize_begin(window, edges);
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    if ws.has_window(&window) {
                        return ws.interactive_resize_begin(window, edges);
                    }
                }
            }
        }

        false
    }

    pub fn interactive_resize_update(
        &mut self,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == window {
                return false;
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        if ws.has_window(window) {
                            return ws.interactive_resize_update(window, delta);
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    if ws.has_window(window) {
                        return ws.interactive_resize_update(window, delta);
                    }
                }
            }
        }

        false
    }

    pub fn interactive_resize_end(&mut self, window: &W::Id) {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == window {
                return;
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        if ws.has_window(window) {
                            ws.interactive_resize_end(Some(window));
                            return;
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    if ws.has_window(window) {
                        ws.interactive_resize_end(Some(window));
                        return;
                    }
                }
            }
        }
    }

    pub fn move_workspace_down(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_workspace_down();
    }

    pub fn move_workspace_up(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_workspace_up();
    }

    pub fn start_open_animation_for_window(&mut self, window: &W::Id) {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == window {
                return;
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        for col in &mut ws.columns {
                            for tile in &mut col.tiles {
                                if tile.window().id() == window {
                                    tile.start_open_animation();
                                    return;
                                }
                            }
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    for col in &mut ws.columns {
                        for tile in &mut col.tiles {
                            if tile.window().id() == window {
                                tile.start_open_animation();
                                return;
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn store_unmap_snapshot(&mut self, renderer: &mut GlesRenderer, window: &W::Id) {
        let _span = tracy_client::span!("Layout::store_unmap_snapshot");

        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if move_.tile.window().id() == window {
                let scale = Scale::from(move_.output.current_scale().fractional_scale());
                move_.tile.store_unmap_snapshot_if_empty(renderer, scale);
                return;
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        if ws.has_window(window) {
                            ws.store_unmap_snapshot_if_empty(renderer, window);
                            return;
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    if ws.has_window(window) {
                        ws.store_unmap_snapshot_if_empty(renderer, window);
                        return;
                    }
                }
            }
        }
    }

    pub fn clear_unmap_snapshot(&mut self, window: &W::Id) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if move_.tile.window().id() == window {
                let _ = move_.tile.take_unmap_snapshot();
                return;
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        if ws.has_window(window) {
                            ws.clear_unmap_snapshot(window);
                            return;
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    if ws.has_window(window) {
                        ws.clear_unmap_snapshot(window);
                        return;
                    }
                }
            }
        }
    }

    pub fn start_close_animation_for_window(
        &mut self,
        renderer: &mut GlesRenderer,
        window: &W::Id,
        blocker: TransactionBlocker,
    ) {
        let _span = tracy_client::span!("Layout::start_close_animation_for_window");

        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if move_.tile.window().id() == window {
                let Some(snapshot) = move_.tile.take_unmap_snapshot() else {
                    return;
                };
                let tile_pos = move_.tile_render_location();
                let tile_size = move_.tile.tile_size();

                let output = move_.output.clone();
                let pointer_pos_within_output = move_.pointer_pos_within_output;
                let Some(mon) = self.monitor_for_output_mut(&output) else {
                    return;
                };
                let Some((ws, offset)) = mon.workspace_under(pointer_pos_within_output) else {
                    return;
                };
                let ws_id = ws.id();
                let ws = mon
                    .workspaces
                    .iter_mut()
                    .find(|ws| ws.id() == ws_id)
                    .unwrap();

                let tile_pos = tile_pos + Point::from((ws.view_pos(), 0.)) - offset;
                ws.start_close_animation_for_tile(renderer, snapshot, tile_size, tile_pos, blocker);
                return;
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        if ws.has_window(window) {
                            ws.start_close_animation_for_window(renderer, window, blocker);
                            return;
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    if ws.has_window(window) {
                        ws.start_close_animation_for_window(renderer, window, blocker);
                        return;
                    }
                }
            }
        }
    }

    pub fn render_floating_for_output<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        output: &Output,
        target: RenderTarget,
    ) -> impl Iterator<Item = TileRenderElement<R>> {
        let mut rv = None;

        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if &move_.output == output {
                let scale = Scale::from(move_.output.current_scale().fractional_scale());
                let location = move_.tile_render_location();
                rv = Some(move_.tile.render(renderer, location, scale, true, target));
            }
        }

        rv.into_iter().flatten()
    }

    pub fn refresh(&mut self, is_active: bool) {
        let _span = tracy_client::span!("Layout::refresh");

        self.is_active = is_active;

        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            let win = move_.tile.window_mut();

            win.set_active_in_column(true);
            win.set_activated(true);

            win.set_interactive_resize(None);

            win.set_bounds(output_size(&move_.output).to_i32_round());

            win.send_pending_configure();
            win.refresh();
        }

        match &mut self.monitor_set {
            MonitorSet::Normal {
                monitors,
                active_monitor_idx,
                ..
            } => {
                for (idx, mon) in monitors.iter_mut().enumerate() {
                    let is_active = self.is_active && idx == *active_monitor_idx;
                    for (ws_idx, ws) in mon.workspaces.iter_mut().enumerate() {
                        let is_active = is_active
                            && ws_idx == mon.active_workspace_idx
                            && !matches!(
                                self.interactive_move,
                                Some(InteractiveMoveState::Moving(_))
                            );
                        ws.refresh(is_active);

                        // Cancel the view offset gesture after workspace switches, moves, etc.
                        if ws_idx != mon.active_workspace_idx {
                            ws.view_offset_gesture_end(false, None);
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    ws.refresh(false);
                    ws.view_offset_gesture_end(false, None);
                }
            }
        }
    }

    pub fn workspaces(
        &self,
    ) -> impl Iterator<Item = (Option<&Monitor<W>>, usize, &Workspace<W>)> + '_ {
        let iter_normal;
        let iter_no_outputs;

        match &self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                let it = monitors.iter().flat_map(|mon| {
                    mon.workspaces
                        .iter()
                        .enumerate()
                        .map(move |(idx, ws)| (Some(mon), idx, ws))
                });

                iter_normal = Some(it);
                iter_no_outputs = None;
            }
            MonitorSet::NoOutputs { workspaces } => {
                let it = workspaces
                    .iter()
                    .enumerate()
                    .map(|(idx, ws)| (None, idx, ws));

                iter_normal = None;
                iter_no_outputs = Some(it);
            }
        }

        let iter_normal = iter_normal.into_iter().flatten();
        let iter_no_outputs = iter_no_outputs.into_iter().flatten();
        iter_normal.chain(iter_no_outputs)
    }

    pub fn workspaces_mut(&mut self) -> impl Iterator<Item = &mut Workspace<W>> + '_ {
        let iter_normal;
        let iter_no_outputs;

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                let it = monitors
                    .iter_mut()
                    .flat_map(|mon| mon.workspaces.iter_mut());

                iter_normal = Some(it);
                iter_no_outputs = None;
            }
            MonitorSet::NoOutputs { workspaces } => {
                let it = workspaces.iter_mut();

                iter_normal = None;
                iter_no_outputs = Some(it);
            }
        }

        let iter_normal = iter_normal.into_iter().flatten();
        let iter_no_outputs = iter_no_outputs.into_iter().flatten();
        iter_normal.chain(iter_no_outputs)
    }

    pub fn windows(&self) -> impl Iterator<Item = (Option<&Monitor<W>>, &W)> {
        let moving_window = self
            .interactive_move
            .as_ref()
            .and_then(|x| x.moving())
            .map(|move_| (self.monitor_for_output(&move_.output), move_.tile.window()))
            .into_iter();

        let rest = self
            .workspaces()
            .flat_map(|(mon, _, ws)| ws.windows().map(move |win| (mon, win)));

        moving_window.chain(rest)
    }

    pub fn has_window(&self, window: &W::Id) -> bool {
        self.windows().any(|(_, win)| win.id() == window)
    }

    fn resolve_default_width(&self, window: &W, width: Option<ColumnWidth>) -> ColumnWidth {
        let mut width = width.unwrap_or_else(|| ColumnWidth::Fixed(f64::from(window.size().w)));
        if let ColumnWidth::Fixed(w) = &mut width {
            let rules = window.rules();
            let border_config = rules.border.resolve_against(self.options.border);
            if !border_config.off {
                *w += border_config.width.0 * 2.;
            }
        }
        width
    }
}

impl<W: LayoutElement> Default for MonitorSet<W> {
    fn default() -> Self {
        Self::NoOutputs { workspaces: vec![] }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use niri_config::{FloatOrInt, OutputName, WorkspaceName};
    use proptest::prelude::*;
    use proptest_derive::Arbitrary;
    use smithay::output::{Mode, PhysicalProperties, Subpixel};
    use smithay::utils::Rectangle;

    use super::*;
    use crate::utils::round_logical_in_physical;

    impl<W: LayoutElement> Default for Layout<W> {
        fn default() -> Self {
            Self::with_options(Default::default())
        }
    }

    #[derive(Debug)]
    struct TestWindowInner {
        id: usize,
        bbox: Cell<Rectangle<i32, Logical>>,
        initial_bbox: Rectangle<i32, Logical>,
        requested_size: Cell<Option<Size<i32, Logical>>>,
        min_size: Size<i32, Logical>,
        max_size: Size<i32, Logical>,
        pending_fullscreen: Cell<bool>,
    }

    #[derive(Debug, Clone)]
    struct TestWindow(Rc<TestWindowInner>);

    impl TestWindow {
        fn new(
            id: usize,
            bbox: Rectangle<i32, Logical>,
            min_size: Size<i32, Logical>,
            max_size: Size<i32, Logical>,
        ) -> Self {
            Self(Rc::new(TestWindowInner {
                id,
                bbox: Cell::new(bbox),
                initial_bbox: bbox,
                requested_size: Cell::new(None),
                min_size,
                max_size,
                pending_fullscreen: Cell::new(false),
            }))
        }

        fn communicate(&self) -> bool {
            if let Some(size) = self.0.requested_size.get() {
                assert!(size.w >= 0);
                assert!(size.h >= 0);

                let mut new_bbox = self.0.initial_bbox;
                if size.w != 0 {
                    new_bbox.size.w = size.w;
                }
                if size.h != 0 {
                    new_bbox.size.h = size.h;
                }

                if self.0.bbox.get() != new_bbox {
                    self.0.bbox.set(new_bbox);
                    return true;
                }
            }

            false
        }
    }

    impl LayoutElement for TestWindow {
        type Id = usize;

        fn id(&self) -> &Self::Id {
            &self.0.id
        }

        fn size(&self) -> Size<i32, Logical> {
            self.0.bbox.get().size
        }

        fn buf_loc(&self) -> Point<i32, Logical> {
            (0, 0).into()
        }

        fn is_in_input_region(&self, _point: Point<f64, Logical>) -> bool {
            false
        }

        fn render<R: NiriRenderer>(
            &self,
            _renderer: &mut R,
            _location: Point<f64, Logical>,
            _scale: Scale<f64>,
            _alpha: f32,
            _target: RenderTarget,
        ) -> SplitElements<LayoutElementRenderElement<R>> {
            SplitElements::default()
        }

        fn request_size(
            &mut self,
            size: Size<i32, Logical>,
            _animate: bool,
            _transaction: Option<Transaction>,
        ) {
            self.0.requested_size.set(Some(size));
            self.0.pending_fullscreen.set(false);
        }

        fn request_fullscreen(&self, _size: Size<i32, Logical>) {
            self.0.pending_fullscreen.set(true);
        }

        fn min_size(&self) -> Size<i32, Logical> {
            self.0.min_size
        }

        fn max_size(&self) -> Size<i32, Logical> {
            self.0.max_size
        }

        fn is_wl_surface(&self, _wl_surface: &WlSurface) -> bool {
            false
        }

        fn set_preferred_scale_transform(&self, _scale: output::Scale, _transform: Transform) {}

        fn has_ssd(&self) -> bool {
            false
        }

        fn output_enter(&self, _output: &Output) {}

        fn output_leave(&self, _output: &Output) {}

        fn set_offscreen_element_id(&self, _id: Option<Id>) {}

        fn set_activated(&mut self, _active: bool) {}

        fn set_bounds(&self, _bounds: Size<i32, Logical>) {}

        fn configure_intent(&self) -> ConfigureIntent {
            ConfigureIntent::CanSend
        }

        fn send_pending_configure(&mut self) {}

        fn set_active_in_column(&mut self, _active: bool) {}

        fn is_fullscreen(&self) -> bool {
            false
        }

        fn is_pending_fullscreen(&self) -> bool {
            self.0.pending_fullscreen.get()
        }

        fn requested_size(&self) -> Option<Size<i32, Logical>> {
            self.0.requested_size.get()
        }

        fn refresh(&self) {}

        fn rules(&self) -> &ResolvedWindowRules {
            static EMPTY: ResolvedWindowRules = ResolvedWindowRules::empty();
            &EMPTY
        }

        fn animation_snapshot(&self) -> Option<&LayoutElementRenderSnapshot> {
            None
        }

        fn take_animation_snapshot(&mut self) -> Option<LayoutElementRenderSnapshot> {
            None
        }

        fn set_interactive_resize(&mut self, _data: Option<InteractiveResizeData>) {}

        fn cancel_interactive_resize(&mut self) {}

        fn update_interactive_resize(&mut self, _serial: Serial) {}

        fn interactive_resize_data(&self) -> Option<InteractiveResizeData> {
            None
        }
    }

    fn arbitrary_bbox() -> impl Strategy<Value = Rectangle<i32, Logical>> {
        any::<(i16, i16, u16, u16)>().prop_map(|(x, y, w, h)| {
            let loc: Point<i32, _> = Point::from((x.into(), y.into()));
            let size: Size<i32, _> = Size::from((w.max(1).into(), h.max(1).into()));
            Rectangle::from_loc_and_size(loc, size)
        })
    }

    fn arbitrary_size_change() -> impl Strategy<Value = SizeChange> {
        prop_oneof![
            (0..).prop_map(SizeChange::SetFixed),
            (0f64..).prop_map(SizeChange::SetProportion),
            any::<i32>().prop_map(SizeChange::AdjustFixed),
            any::<f64>().prop_map(SizeChange::AdjustProportion),
        ]
    }

    fn arbitrary_min_max() -> impl Strategy<Value = (i32, i32)> {
        prop_oneof![
            Just((0, 0)),
            (1..65536).prop_map(|n| (n, n)),
            (1..65536).prop_map(|min| (min, 0)),
            (1..).prop_map(|max| (0, max)),
            (1..65536, 1..).prop_map(|(min, max): (i32, i32)| (min, max.max(min))),
        ]
    }

    fn arbitrary_min_max_size() -> impl Strategy<Value = (Size<i32, Logical>, Size<i32, Logical>)> {
        (arbitrary_min_max(), arbitrary_min_max()).prop_map(|((min_w, max_w), (min_h, max_h))| {
            let min_size = Size::from((min_w, min_h));
            let max_size = Size::from((max_w, max_h));
            (min_size, max_size)
        })
    }

    fn arbitrary_view_offset_gesture_delta() -> impl Strategy<Value = f64> {
        prop_oneof![(-10f64..10f64), (-50000f64..50000f64),]
    }

    fn arbitrary_resize_edge() -> impl Strategy<Value = ResizeEdge> {
        prop_oneof![
            Just(ResizeEdge::RIGHT),
            Just(ResizeEdge::BOTTOM),
            Just(ResizeEdge::LEFT),
            Just(ResizeEdge::TOP),
            Just(ResizeEdge::BOTTOM_RIGHT),
            Just(ResizeEdge::BOTTOM_LEFT),
            Just(ResizeEdge::TOP_RIGHT),
            Just(ResizeEdge::TOP_LEFT),
            Just(ResizeEdge::empty()),
        ]
    }

    fn arbitrary_scale() -> impl Strategy<Value = f64> {
        prop_oneof![Just(1.), Just(1.5), Just(2.),]
    }

    #[derive(Debug, Clone, Copy, Arbitrary)]
    enum Op {
        AddOutput(#[proptest(strategy = "1..=5usize")] usize),
        AddScaledOutput {
            #[proptest(strategy = "1..=5usize")]
            id: usize,
            #[proptest(strategy = "arbitrary_scale()")]
            scale: f64,
        },
        RemoveOutput(#[proptest(strategy = "1..=5usize")] usize),
        FocusOutput(#[proptest(strategy = "1..=5usize")] usize),
        AddNamedWorkspace {
            #[proptest(strategy = "1..=5usize")]
            ws_name: usize,
            #[proptest(strategy = "prop::option::of(1..=5usize)")]
            output_name: Option<usize>,
        },
        UnnameWorkspace {
            #[proptest(strategy = "1..=5usize")]
            ws_name: usize,
        },
        AddWindow {
            #[proptest(strategy = "1..=5usize")]
            id: usize,
            #[proptest(strategy = "arbitrary_bbox()")]
            bbox: Rectangle<i32, Logical>,
            #[proptest(strategy = "arbitrary_min_max_size()")]
            min_max_size: (Size<i32, Logical>, Size<i32, Logical>),
        },
        AddWindowRightOf {
            #[proptest(strategy = "1..=5usize")]
            id: usize,
            #[proptest(strategy = "1..=5usize")]
            right_of_id: usize,
            #[proptest(strategy = "arbitrary_bbox()")]
            bbox: Rectangle<i32, Logical>,
            #[proptest(strategy = "arbitrary_min_max_size()")]
            min_max_size: (Size<i32, Logical>, Size<i32, Logical>),
        },
        AddWindowToNamedWorkspace {
            #[proptest(strategy = "1..=5usize")]
            id: usize,
            #[proptest(strategy = "1..=5usize")]
            ws_name: usize,
            #[proptest(strategy = "arbitrary_bbox()")]
            bbox: Rectangle<i32, Logical>,
            #[proptest(strategy = "arbitrary_min_max_size()")]
            min_max_size: (Size<i32, Logical>, Size<i32, Logical>),
        },
        CloseWindow(#[proptest(strategy = "1..=5usize")] usize),
        FullscreenWindow(#[proptest(strategy = "1..=5usize")] usize),
        SetFullscreenWindow {
            #[proptest(strategy = "1..=5usize")]
            window: usize,
            is_fullscreen: bool,
        },
        FocusColumnLeft,
        FocusColumnRight,
        FocusColumnFirst,
        FocusColumnLast,
        FocusColumnRightOrFirst,
        FocusColumnLeftOrLast,
        FocusWindowOrMonitorUp(#[proptest(strategy = "1..=2u8")] u8),
        FocusWindowOrMonitorDown(#[proptest(strategy = "1..=2u8")] u8),
        FocusColumnOrMonitorLeft(#[proptest(strategy = "1..=2u8")] u8),
        FocusColumnOrMonitorRight(#[proptest(strategy = "1..=2u8")] u8),
        FocusWindowDown,
        FocusWindowUp,
        FocusWindowDownOrColumnLeft,
        FocusWindowDownOrColumnRight,
        FocusWindowUpOrColumnLeft,
        FocusWindowUpOrColumnRight,
        FocusWindowOrWorkspaceDown,
        FocusWindowOrWorkspaceUp,
        MoveColumnLeft,
        MoveColumnRight,
        MoveColumnToFirst,
        MoveColumnToLast,
        MoveColumnLeftOrToMonitorLeft(#[proptest(strategy = "1..=2u8")] u8),
        MoveColumnRightOrToMonitorRight(#[proptest(strategy = "1..=2u8")] u8),
        MoveWindowDown,
        MoveWindowUp,
        MoveWindowDownOrToWorkspaceDown,
        MoveWindowUpOrToWorkspaceUp,
        ConsumeOrExpelWindowLeft {
            #[proptest(strategy = "proptest::option::of(1..=5usize)")]
            id: Option<usize>,
        },
        ConsumeOrExpelWindowRight {
            #[proptest(strategy = "proptest::option::of(1..=5usize)")]
            id: Option<usize>,
        },
        ConsumeWindowIntoColumn,
        ExpelWindowFromColumn,
        CenterColumn,
        FocusWorkspaceDown,
        FocusWorkspaceUp,
        FocusWorkspace(#[proptest(strategy = "0..=4usize")] usize),
        FocusWorkspaceAutoBackAndForth(#[proptest(strategy = "0..=4usize")] usize),
        FocusWorkspacePrevious,
        MoveWindowToWorkspaceDown,
        MoveWindowToWorkspaceUp,
        MoveWindowToWorkspace {
            #[proptest(strategy = "proptest::option::of(1..=5usize)")]
            window_id: Option<usize>,
            #[proptest(strategy = "0..=4usize")]
            workspace_idx: usize,
        },
        MoveColumnToWorkspaceDown,
        MoveColumnToWorkspaceUp,
        MoveColumnToWorkspace(#[proptest(strategy = "0..=4usize")] usize),
        MoveWorkspaceDown,
        MoveWorkspaceUp,
        MoveWindowToOutput {
            #[proptest(strategy = "proptest::option::of(1..=5usize)")]
            window_id: Option<usize>,
            #[proptest(strategy = "1..=5u8")]
            output_id: u8,
            #[proptest(strategy = "proptest::option::of(0..=4usize)")]
            target_ws_idx: Option<usize>,
        },
        MoveColumnToOutput(#[proptest(strategy = "1..=5u8")] u8),
        SwitchPresetColumnWidth,
        SwitchPresetWindowHeight {
            #[proptest(strategy = "proptest::option::of(1..=5usize)")]
            id: Option<usize>,
        },
        MaximizeColumn,
        SetColumnWidth(#[proptest(strategy = "arbitrary_size_change()")] SizeChange),
        SetWindowHeight {
            #[proptest(strategy = "proptest::option::of(1..=5usize)")]
            id: Option<usize>,
            #[proptest(strategy = "arbitrary_size_change()")]
            change: SizeChange,
        },
        ResetWindowHeight {
            #[proptest(strategy = "proptest::option::of(1..=5usize)")]
            id: Option<usize>,
        },
        Communicate(#[proptest(strategy = "1..=5usize")] usize),
        Refresh {
            is_active: bool,
        },
        MoveWorkspaceToOutput(#[proptest(strategy = "1..=5u8")] u8),
        ViewOffsetGestureBegin {
            #[proptest(strategy = "1..=5usize")]
            output_idx: usize,
            is_touchpad: bool,
        },
        ViewOffsetGestureUpdate {
            #[proptest(strategy = "arbitrary_view_offset_gesture_delta()")]
            delta: f64,
            timestamp: Duration,
            is_touchpad: bool,
        },
        ViewOffsetGestureEnd {
            is_touchpad: Option<bool>,
        },
        WorkspaceSwitchGestureBegin {
            #[proptest(strategy = "1..=5usize")]
            output_idx: usize,
            is_touchpad: bool,
        },
        WorkspaceSwitchGestureUpdate {
            #[proptest(strategy = "-400f64..400f64")]
            delta: f64,
            timestamp: Duration,
            is_touchpad: bool,
        },
        WorkspaceSwitchGestureEnd {
            cancelled: bool,
            is_touchpad: Option<bool>,
        },
        InteractiveMoveBegin {
            #[proptest(strategy = "1..=5usize")]
            window: usize,
            #[proptest(strategy = "1..=5usize")]
            output_idx: usize,
            #[proptest(strategy = "-20000f64..20000f64")]
            px: f64,
            #[proptest(strategy = "-20000f64..20000f64")]
            py: f64,
        },
        InteractiveMoveUpdate {
            #[proptest(strategy = "1..=5usize")]
            window: usize,
            #[proptest(strategy = "-20000f64..20000f64")]
            dx: f64,
            #[proptest(strategy = "-20000f64..20000f64")]
            dy: f64,
            #[proptest(strategy = "1..=5usize")]
            output_idx: usize,
            #[proptest(strategy = "-20000f64..20000f64")]
            px: f64,
            #[proptest(strategy = "-20000f64..20000f64")]
            py: f64,
        },
        InteractiveMoveEnd {
            #[proptest(strategy = "1..=5usize")]
            window: usize,
        },
        InteractiveResizeBegin {
            #[proptest(strategy = "1..=5usize")]
            window: usize,
            #[proptest(strategy = "arbitrary_resize_edge()")]
            edges: ResizeEdge,
        },
        InteractiveResizeUpdate {
            #[proptest(strategy = "1..=5usize")]
            window: usize,
            #[proptest(strategy = "-20000f64..20000f64")]
            dx: f64,
            #[proptest(strategy = "-20000f64..20000f64")]
            dy: f64,
        },
        InteractiveResizeEnd {
            #[proptest(strategy = "1..=5usize")]
            window: usize,
        },
    }

    impl Op {
        fn apply(self, layout: &mut Layout<TestWindow>) {
            match self {
                Op::AddOutput(id) => {
                    let name = format!("output{id}");
                    if layout.outputs().any(|o| o.name() == name) {
                        return;
                    }

                    let output = Output::new(
                        name.clone(),
                        PhysicalProperties {
                            size: Size::from((1280, 720)),
                            subpixel: Subpixel::Unknown,
                            make: String::new(),
                            model: String::new(),
                        },
                    );
                    output.change_current_state(
                        Some(Mode {
                            size: Size::from((1280, 720)),
                            refresh: 60000,
                        }),
                        None,
                        None,
                        None,
                    );
                    output.user_data().insert_if_missing(|| OutputName {
                        connector: name,
                        make: None,
                        model: None,
                        serial: None,
                    });
                    layout.add_output(output.clone());
                }
                Op::AddScaledOutput { id, scale } => {
                    let name = format!("output{id}");
                    if layout.outputs().any(|o| o.name() == name) {
                        return;
                    }

                    let output = Output::new(
                        name.clone(),
                        PhysicalProperties {
                            size: Size::from((1280, 720)),
                            subpixel: Subpixel::Unknown,
                            make: String::new(),
                            model: String::new(),
                        },
                    );
                    output.change_current_state(
                        Some(Mode {
                            size: Size::from((1280, 720)),
                            refresh: 60000,
                        }),
                        None,
                        Some(smithay::output::Scale::Fractional(scale)),
                        None,
                    );
                    output.user_data().insert_if_missing(|| OutputName {
                        connector: name,
                        make: None,
                        model: None,
                        serial: None,
                    });
                    layout.add_output(output.clone());
                }
                Op::RemoveOutput(id) => {
                    let name = format!("output{id}");
                    let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                        return;
                    };

                    layout.remove_output(&output);
                }
                Op::FocusOutput(id) => {
                    let name = format!("output{id}");
                    let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                        return;
                    };

                    layout.focus_output(&output);
                }
                Op::AddNamedWorkspace {
                    ws_name,
                    output_name,
                } => {
                    layout.ensure_named_workspace(&WorkspaceConfig {
                        name: WorkspaceName(format!("ws{ws_name}")),
                        open_on_output: output_name.map(|name| format!("output{name}")),
                    });
                }
                Op::UnnameWorkspace { ws_name } => {
                    layout.unname_workspace(&format!("ws{ws_name}"));
                }
                Op::AddWindow {
                    id,
                    bbox,
                    min_max_size,
                } => {
                    if layout.has_window(&id) {
                        return;
                    }

                    let win = TestWindow::new(id, bbox, min_max_size.0, min_max_size.1);
                    layout.add_window(win, None, false);
                }
                Op::AddWindowRightOf {
                    id,
                    right_of_id,
                    bbox,
                    min_max_size,
                } => {
                    let mut found_right_of = false;

                    if let Some(InteractiveMoveState::Moving(move_)) = &layout.interactive_move {
                        if move_.tile.window().0.id == id {
                            return;
                        }
                    }

                    match &mut layout.monitor_set {
                        MonitorSet::Normal { monitors, .. } => {
                            for mon in monitors {
                                for ws in &mut mon.workspaces {
                                    for win in ws.windows() {
                                        if win.0.id == id {
                                            return;
                                        }

                                        if win.0.id == right_of_id {
                                            found_right_of = true;
                                        }
                                    }
                                }
                            }
                        }
                        MonitorSet::NoOutputs { workspaces, .. } => {
                            for ws in workspaces {
                                for win in ws.windows() {
                                    if win.0.id == id {
                                        return;
                                    }

                                    if win.0.id == right_of_id {
                                        found_right_of = true;
                                    }
                                }
                            }
                        }
                    }

                    if !found_right_of {
                        return;
                    }

                    let win = TestWindow::new(id, bbox, min_max_size.0, min_max_size.1);
                    layout.add_window_right_of(&right_of_id, win, None, false);
                }
                Op::AddWindowToNamedWorkspace {
                    id,
                    ws_name,
                    bbox,
                    min_max_size,
                } => {
                    let ws_name = format!("ws{ws_name}");
                    let mut found_workspace = false;

                    if let Some(InteractiveMoveState::Moving(move_)) = &layout.interactive_move {
                        if move_.tile.window().0.id == id {
                            return;
                        }
                    }

                    match &mut layout.monitor_set {
                        MonitorSet::Normal { monitors, .. } => {
                            for mon in monitors {
                                for ws in &mut mon.workspaces {
                                    for win in ws.windows() {
                                        if win.0.id == id {
                                            return;
                                        }
                                    }

                                    if ws
                                        .name
                                        .as_ref()
                                        .map_or(false, |name| name.eq_ignore_ascii_case(&ws_name))
                                    {
                                        found_workspace = true;
                                    }
                                }
                            }
                        }
                        MonitorSet::NoOutputs { workspaces, .. } => {
                            for ws in workspaces {
                                for win in ws.windows() {
                                    if win.0.id == id {
                                        return;
                                    }
                                }

                                if ws
                                    .name
                                    .as_ref()
                                    .map_or(false, |name| name.eq_ignore_ascii_case(&ws_name))
                                {
                                    found_workspace = true;
                                }
                            }
                        }
                    }

                    if !found_workspace {
                        return;
                    }

                    let win = TestWindow::new(id, bbox, min_max_size.0, min_max_size.1);
                    layout.add_window_to_named_workspace(&ws_name, win, None, false);
                }
                Op::CloseWindow(id) => {
                    layout.remove_window(&id, Transaction::new());
                }
                Op::FullscreenWindow(id) => {
                    layout.toggle_fullscreen(&id);
                }
                Op::SetFullscreenWindow {
                    window,
                    is_fullscreen,
                } => {
                    layout.set_fullscreen(&window, is_fullscreen);
                }
                Op::FocusColumnLeft => layout.focus_left(),
                Op::FocusColumnRight => layout.focus_right(),
                Op::FocusColumnFirst => layout.focus_column_first(),
                Op::FocusColumnLast => layout.focus_column_last(),
                Op::FocusColumnRightOrFirst => layout.focus_column_right_or_first(),
                Op::FocusColumnLeftOrLast => layout.focus_column_left_or_last(),
                Op::FocusWindowOrMonitorUp(id) => {
                    let name = format!("output{id}");
                    let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                        return;
                    };

                    layout.focus_window_up_or_output(&output);
                }
                Op::FocusWindowOrMonitorDown(id) => {
                    let name = format!("output{id}");
                    let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                        return;
                    };

                    layout.focus_window_down_or_output(&output);
                }
                Op::FocusColumnOrMonitorLeft(id) => {
                    let name = format!("output{id}");
                    let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                        return;
                    };

                    layout.focus_column_left_or_output(&output);
                }
                Op::FocusColumnOrMonitorRight(id) => {
                    let name = format!("output{id}");
                    let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                        return;
                    };

                    layout.focus_column_right_or_output(&output);
                }
                Op::FocusWindowDown => layout.focus_down(),
                Op::FocusWindowUp => layout.focus_up(),
                Op::FocusWindowDownOrColumnLeft => layout.focus_down_or_left(),
                Op::FocusWindowDownOrColumnRight => layout.focus_down_or_right(),
                Op::FocusWindowUpOrColumnLeft => layout.focus_up_or_left(),
                Op::FocusWindowUpOrColumnRight => layout.focus_up_or_right(),
                Op::FocusWindowOrWorkspaceDown => layout.focus_window_or_workspace_down(),
                Op::FocusWindowOrWorkspaceUp => layout.focus_window_or_workspace_up(),
                Op::MoveColumnLeft => layout.move_left(),
                Op::MoveColumnRight => layout.move_right(),
                Op::MoveColumnToFirst => layout.move_column_to_first(),
                Op::MoveColumnToLast => layout.move_column_to_last(),
                Op::MoveColumnLeftOrToMonitorLeft(id) => {
                    let name = format!("output{id}");
                    let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                        return;
                    };

                    layout.move_column_left_or_to_output(&output);
                }
                Op::MoveColumnRightOrToMonitorRight(id) => {
                    let name = format!("output{id}");
                    let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                        return;
                    };

                    layout.move_column_right_or_to_output(&output);
                }
                Op::MoveWindowDown => layout.move_down(),
                Op::MoveWindowUp => layout.move_up(),
                Op::MoveWindowDownOrToWorkspaceDown => layout.move_down_or_to_workspace_down(),
                Op::MoveWindowUpOrToWorkspaceUp => layout.move_up_or_to_workspace_up(),
                Op::ConsumeOrExpelWindowLeft { id } => {
                    let id = id.filter(|id| layout.has_window(id));
                    layout.consume_or_expel_window_left(id.as_ref());
                }
                Op::ConsumeOrExpelWindowRight { id } => {
                    let id = id.filter(|id| layout.has_window(id));
                    layout.consume_or_expel_window_right(id.as_ref());
                }
                Op::ConsumeWindowIntoColumn => layout.consume_into_column(),
                Op::ExpelWindowFromColumn => layout.expel_from_column(),
                Op::CenterColumn => layout.center_column(),
                Op::FocusWorkspaceDown => layout.switch_workspace_down(),
                Op::FocusWorkspaceUp => layout.switch_workspace_up(),
                Op::FocusWorkspace(idx) => layout.switch_workspace(idx),
                Op::FocusWorkspaceAutoBackAndForth(idx) => {
                    layout.switch_workspace_auto_back_and_forth(idx)
                }
                Op::FocusWorkspacePrevious => layout.switch_workspace_previous(),
                Op::MoveWindowToWorkspaceDown => layout.move_to_workspace_down(),
                Op::MoveWindowToWorkspaceUp => layout.move_to_workspace_up(),
                Op::MoveWindowToWorkspace {
                    window_id,
                    workspace_idx,
                } => {
                    let window_id = window_id.filter(|id| {
                        layout
                            .active_monitor()
                            .map_or(false, |mon| mon.has_window(id))
                    });
                    layout.move_to_workspace(window_id.as_ref(), workspace_idx);
                }
                Op::MoveColumnToWorkspaceDown => layout.move_column_to_workspace_down(),
                Op::MoveColumnToWorkspaceUp => layout.move_column_to_workspace_up(),
                Op::MoveColumnToWorkspace(idx) => layout.move_column_to_workspace(idx),
                Op::MoveWindowToOutput {
                    window_id,
                    output_id: id,
                    target_ws_idx,
                } => {
                    let name = format!("output{id}");
                    let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                        return;
                    };
                    let mon = layout.monitor_for_output(&output).unwrap();

                    let window_id = window_id.filter(|id| layout.has_window(id));
                    let target_ws_idx = target_ws_idx.filter(|idx| mon.workspaces.len() > *idx);
                    layout.move_to_output(window_id.as_ref(), &output, target_ws_idx);
                }
                Op::MoveColumnToOutput(id) => {
                    let name = format!("output{id}");
                    let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                        return;
                    };

                    layout.move_column_to_output(&output);
                }
                Op::MoveWorkspaceDown => layout.move_workspace_down(),
                Op::MoveWorkspaceUp => layout.move_workspace_up(),
                Op::SwitchPresetColumnWidth => layout.toggle_width(),
                Op::SwitchPresetWindowHeight { id } => {
                    let id = id.filter(|id| layout.has_window(id));
                    layout.toggle_window_height(id.as_ref());
                }
                Op::MaximizeColumn => layout.toggle_full_width(),
                Op::SetColumnWidth(change) => layout.set_column_width(change),
                Op::SetWindowHeight { id, change } => {
                    let id = id.filter(|id| layout.has_window(id));
                    layout.set_window_height(id.as_ref(), change);
                }
                Op::ResetWindowHeight { id } => {
                    let id = id.filter(|id| layout.has_window(id));
                    layout.reset_window_height(id.as_ref());
                }
                Op::Communicate(id) => {
                    let mut update = false;

                    if let Some(InteractiveMoveState::Moving(move_)) = &layout.interactive_move {
                        if move_.tile.window().0.id == id {
                            if move_.tile.window().communicate() {
                                update = true;
                            }

                            if update {
                                // FIXME: serial.
                                layout.update_window(&id, None);
                            }
                            return;
                        }
                    }

                    match &mut layout.monitor_set {
                        MonitorSet::Normal { monitors, .. } => {
                            'outer: for mon in monitors {
                                for ws in &mut mon.workspaces {
                                    for win in ws.windows() {
                                        if win.0.id == id {
                                            if win.communicate() {
                                                update = true;
                                            }
                                            break 'outer;
                                        }
                                    }
                                }
                            }
                        }
                        MonitorSet::NoOutputs { workspaces, .. } => {
                            'outer: for ws in workspaces {
                                for win in ws.windows() {
                                    if win.0.id == id {
                                        if win.communicate() {
                                            update = true;
                                        }
                                        break 'outer;
                                    }
                                }
                            }
                        }
                    }

                    if update {
                        // FIXME: serial.
                        layout.update_window(&id, None);
                    }
                }
                Op::Refresh { is_active } => {
                    layout.refresh(is_active);
                }
                Op::MoveWorkspaceToOutput(id) => {
                    let name = format!("output{id}");
                    let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                        return;
                    };

                    layout.move_workspace_to_output(&output);
                }
                Op::ViewOffsetGestureBegin {
                    output_idx: id,
                    is_touchpad: normalize,
                } => {
                    let name = format!("output{id}");
                    let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                        return;
                    };

                    layout.view_offset_gesture_begin(&output, normalize);
                }
                Op::ViewOffsetGestureUpdate {
                    delta,
                    timestamp,
                    is_touchpad,
                } => {
                    layout.view_offset_gesture_update(delta, timestamp, is_touchpad);
                }
                Op::ViewOffsetGestureEnd { is_touchpad } => {
                    // We don't handle cancels in this gesture.
                    layout.view_offset_gesture_end(false, is_touchpad);
                }
                Op::WorkspaceSwitchGestureBegin {
                    output_idx: id,
                    is_touchpad,
                } => {
                    let name = format!("output{id}");
                    let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                        return;
                    };

                    layout.workspace_switch_gesture_begin(&output, is_touchpad);
                }
                Op::WorkspaceSwitchGestureUpdate {
                    delta,
                    timestamp,
                    is_touchpad,
                } => {
                    layout.workspace_switch_gesture_update(delta, timestamp, is_touchpad);
                }
                Op::WorkspaceSwitchGestureEnd {
                    cancelled,
                    is_touchpad,
                } => {
                    layout.workspace_switch_gesture_end(cancelled, is_touchpad);
                }
                Op::InteractiveMoveBegin {
                    window,
                    output_idx,
                    px,
                    py,
                } => {
                    let name = format!("output{output_idx}");
                    let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                        return;
                    };
                    layout.interactive_move_begin(window, &output, Point::from((px, py)));
                }
                Op::InteractiveMoveUpdate {
                    window,
                    dx,
                    dy,
                    output_idx,
                    px,
                    py,
                } => {
                    let name = format!("output{output_idx}");
                    let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                        return;
                    };
                    layout.interactive_move_update(
                        &window,
                        Point::from((dx, dy)),
                        output,
                        Point::from((px, py)),
                    );
                }
                Op::InteractiveMoveEnd { window } => {
                    layout.interactive_move_end(&window);
                }
                Op::InteractiveResizeBegin { window, edges } => {
                    layout.interactive_resize_begin(window, edges);
                }
                Op::InteractiveResizeUpdate { window, dx, dy } => {
                    layout.interactive_resize_update(&window, Point::from((dx, dy)));
                }
                Op::InteractiveResizeEnd { window } => {
                    layout.interactive_resize_end(&window);
                }
            }
        }
    }

    #[track_caller]
    fn check_ops(ops: &[Op]) {
        let mut layout = Layout::default();
        for op in ops {
            op.apply(&mut layout);
            layout.verify_invariants();
        }
    }

    #[track_caller]
    fn check_ops_with_options(options: Options, ops: &[Op]) {
        let mut layout = Layout::with_options(options);

        for op in ops {
            op.apply(&mut layout);
            layout.verify_invariants();
        }
    }

    #[test]
    fn operations_dont_panic() {
        let every_op = [
            Op::AddOutput(0),
            Op::AddOutput(1),
            Op::AddOutput(2),
            Op::RemoveOutput(0),
            Op::RemoveOutput(1),
            Op::RemoveOutput(2),
            Op::FocusOutput(0),
            Op::FocusOutput(1),
            Op::FocusOutput(2),
            Op::AddNamedWorkspace {
                ws_name: 1,
                output_name: Some(1),
            },
            Op::UnnameWorkspace { ws_name: 1 },
            Op::AddWindow {
                id: 0,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::AddWindow {
                id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::AddWindowRightOf {
                id: 2,
                right_of_id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::AddWindowToNamedWorkspace {
                id: 3,
                ws_name: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::CloseWindow(0),
            Op::CloseWindow(1),
            Op::CloseWindow(2),
            Op::FullscreenWindow(1),
            Op::FullscreenWindow(2),
            Op::FullscreenWindow(3),
            Op::FocusColumnLeft,
            Op::FocusColumnRight,
            Op::FocusColumnRightOrFirst,
            Op::FocusColumnLeftOrLast,
            Op::FocusWindowOrMonitorUp(0),
            Op::FocusWindowOrMonitorDown(1),
            Op::FocusColumnOrMonitorLeft(0),
            Op::FocusColumnOrMonitorRight(1),
            Op::FocusWindowUp,
            Op::FocusWindowUpOrColumnLeft,
            Op::FocusWindowUpOrColumnRight,
            Op::FocusWindowOrWorkspaceUp,
            Op::FocusWindowDown,
            Op::FocusWindowDownOrColumnLeft,
            Op::FocusWindowDownOrColumnRight,
            Op::FocusWindowOrWorkspaceDown,
            Op::MoveColumnLeft,
            Op::MoveColumnRight,
            Op::MoveColumnLeftOrToMonitorLeft(0),
            Op::MoveColumnRightOrToMonitorRight(1),
            Op::ConsumeWindowIntoColumn,
            Op::ExpelWindowFromColumn,
            Op::CenterColumn,
            Op::FocusWorkspaceDown,
            Op::FocusWorkspaceUp,
            Op::FocusWorkspace(1),
            Op::FocusWorkspace(2),
            Op::MoveWindowToWorkspaceDown,
            Op::MoveWindowToWorkspaceUp,
            Op::MoveWindowToWorkspace {
                window_id: None,
                workspace_idx: 1,
            },
            Op::MoveWindowToWorkspace {
                window_id: None,
                workspace_idx: 2,
            },
            Op::MoveColumnToWorkspaceDown,
            Op::MoveColumnToWorkspaceUp,
            Op::MoveColumnToWorkspace(1),
            Op::MoveColumnToWorkspace(2),
            Op::MoveWindowDown,
            Op::MoveWindowDownOrToWorkspaceDown,
            Op::MoveWindowUp,
            Op::MoveWindowUpOrToWorkspaceUp,
            Op::ConsumeOrExpelWindowLeft { id: None },
            Op::ConsumeOrExpelWindowRight { id: None },
            Op::MoveWorkspaceToOutput(1),
        ];

        for third in every_op {
            for second in every_op {
                for first in every_op {
                    // eprintln!("{first:?}, {second:?}, {third:?}");

                    let mut layout = Layout::default();
                    first.apply(&mut layout);
                    layout.verify_invariants();
                    second.apply(&mut layout);
                    layout.verify_invariants();
                    third.apply(&mut layout);
                    layout.verify_invariants();
                }
            }
        }
    }

    #[test]
    fn operations_from_starting_state_dont_panic() {
        if std::env::var_os("RUN_SLOW_TESTS").is_none() {
            eprintln!("ignoring slow test");
            return;
        }

        // Running every op from an empty state doesn't get us to all the interesting states. So,
        // also run it from a manually-created starting state with more things going on to exercise
        // more code paths.
        let setup_ops = [
            Op::AddOutput(1),
            Op::AddWindow {
                id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::MoveWindowToWorkspaceDown,
            Op::AddWindow {
                id: 2,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::AddWindow {
                id: 3,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::FocusColumnLeft,
            Op::ConsumeWindowIntoColumn,
            Op::AddWindow {
                id: 4,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::AddOutput(2),
            Op::AddWindow {
                id: 5,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::MoveWindowToOutput {
                window_id: None,
                output_id: 2,
                target_ws_idx: None,
            },
            Op::FocusOutput(1),
            Op::Communicate(1),
            Op::Communicate(2),
            Op::Communicate(3),
            Op::Communicate(4),
            Op::Communicate(5),
        ];

        let every_op = [
            Op::AddOutput(0),
            Op::AddOutput(1),
            Op::AddOutput(2),
            Op::RemoveOutput(0),
            Op::RemoveOutput(1),
            Op::RemoveOutput(2),
            Op::FocusOutput(0),
            Op::FocusOutput(1),
            Op::FocusOutput(2),
            Op::AddNamedWorkspace {
                ws_name: 1,
                output_name: Some(1),
            },
            Op::UnnameWorkspace { ws_name: 1 },
            Op::AddWindow {
                id: 0,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::AddWindow {
                id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::AddWindow {
                id: 2,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::AddWindowRightOf {
                id: 6,
                right_of_id: 0,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::AddWindowRightOf {
                id: 7,
                right_of_id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::AddWindowToNamedWorkspace {
                id: 5,
                ws_name: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::CloseWindow(0),
            Op::CloseWindow(1),
            Op::CloseWindow(2),
            Op::FullscreenWindow(1),
            Op::FullscreenWindow(2),
            Op::FullscreenWindow(3),
            Op::SetFullscreenWindow {
                window: 1,
                is_fullscreen: false,
            },
            Op::SetFullscreenWindow {
                window: 1,
                is_fullscreen: true,
            },
            Op::SetFullscreenWindow {
                window: 2,
                is_fullscreen: false,
            },
            Op::SetFullscreenWindow {
                window: 2,
                is_fullscreen: true,
            },
            Op::FocusColumnLeft,
            Op::FocusColumnRight,
            Op::FocusColumnRightOrFirst,
            Op::FocusColumnLeftOrLast,
            Op::FocusWindowOrMonitorUp(0),
            Op::FocusWindowOrMonitorDown(1),
            Op::FocusColumnOrMonitorLeft(0),
            Op::FocusColumnOrMonitorRight(1),
            Op::FocusWindowUp,
            Op::FocusWindowUpOrColumnLeft,
            Op::FocusWindowUpOrColumnRight,
            Op::FocusWindowOrWorkspaceUp,
            Op::FocusWindowDown,
            Op::FocusWindowDownOrColumnLeft,
            Op::FocusWindowDownOrColumnRight,
            Op::FocusWindowOrWorkspaceDown,
            Op::MoveColumnLeft,
            Op::MoveColumnRight,
            Op::MoveColumnLeftOrToMonitorLeft(0),
            Op::MoveColumnRightOrToMonitorRight(1),
            Op::ConsumeWindowIntoColumn,
            Op::ExpelWindowFromColumn,
            Op::CenterColumn,
            Op::FocusWorkspaceDown,
            Op::FocusWorkspaceUp,
            Op::FocusWorkspace(1),
            Op::FocusWorkspace(2),
            Op::FocusWorkspace(3),
            Op::MoveWindowToWorkspaceDown,
            Op::MoveWindowToWorkspaceUp,
            Op::MoveWindowToWorkspace {
                window_id: None,
                workspace_idx: 1,
            },
            Op::MoveWindowToWorkspace {
                window_id: None,
                workspace_idx: 2,
            },
            Op::MoveWindowToWorkspace {
                window_id: None,
                workspace_idx: 3,
            },
            Op::MoveColumnToWorkspaceDown,
            Op::MoveColumnToWorkspaceUp,
            Op::MoveColumnToWorkspace(1),
            Op::MoveColumnToWorkspace(2),
            Op::MoveColumnToWorkspace(3),
            Op::MoveWindowDown,
            Op::MoveWindowDownOrToWorkspaceDown,
            Op::MoveWindowUp,
            Op::MoveWindowUpOrToWorkspaceUp,
            Op::ConsumeOrExpelWindowLeft { id: None },
            Op::ConsumeOrExpelWindowRight { id: None },
        ];

        for third in every_op {
            for second in every_op {
                for first in every_op {
                    // eprintln!("{first:?}, {second:?}, {third:?}");

                    let mut layout = Layout::default();
                    for op in setup_ops {
                        op.apply(&mut layout);
                    }

                    first.apply(&mut layout);
                    layout.verify_invariants();
                    second.apply(&mut layout);
                    layout.verify_invariants();
                    third.apply(&mut layout);
                    layout.verify_invariants();
                }
            }
        }
    }

    #[test]
    fn primary_active_workspace_idx_not_updated_on_output_add() {
        let ops = [
            Op::AddOutput(1),
            Op::AddOutput(2),
            Op::FocusOutput(1),
            Op::AddWindow {
                id: 0,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::FocusOutput(2),
            Op::AddWindow {
                id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::RemoveOutput(2),
            Op::FocusWorkspace(3),
            Op::AddOutput(2),
        ];

        check_ops(&ops);
    }

    #[test]
    fn window_closed_on_previous_workspace() {
        let ops = [
            Op::AddOutput(1),
            Op::AddWindow {
                id: 0,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::FocusWorkspaceDown,
            Op::CloseWindow(0),
        ];

        check_ops(&ops);
    }

    #[test]
    fn removing_output_must_keep_empty_focus_on_primary() {
        let ops = [
            Op::AddOutput(1),
            Op::AddWindow {
                id: 0,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::AddOutput(2),
            Op::RemoveOutput(1),
        ];

        let mut layout = Layout::default();
        for op in ops {
            op.apply(&mut layout);
        }

        let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
            unreachable!()
        };

        // The workspace from the removed output was inserted at position 0, so the active workspace
        // must change to 1 to keep the focus on the empty workspace.
        assert_eq!(monitors[0].active_workspace_idx, 1);
    }

    #[test]
    fn move_to_workspace_by_idx_does_not_leave_empty_workspaces() {
        let ops = [
            Op::AddOutput(1),
            Op::AddWindow {
                id: 0,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::AddOutput(2),
            Op::FocusOutput(2),
            Op::AddWindow {
                id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::RemoveOutput(1),
            Op::MoveWindowToWorkspace {
                window_id: Some(0),
                workspace_idx: 2,
            },
        ];

        let mut layout = Layout::default();
        for op in ops {
            op.apply(&mut layout);
        }

        let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
            unreachable!()
        };

        assert!(monitors[0].workspaces[1].has_windows());
    }

    #[test]
    fn empty_workspaces_dont_move_back_to_original_output() {
        let ops = [
            Op::AddOutput(1),
            Op::AddWindow {
                id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::FocusWorkspaceDown,
            Op::AddWindow {
                id: 2,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::AddOutput(2),
            Op::RemoveOutput(1),
            Op::FocusWorkspace(1),
            Op::CloseWindow(1),
            Op::AddOutput(1),
        ];

        check_ops(&ops);
    }

    #[test]
    fn large_negative_height_change() {
        let ops = [
            Op::AddOutput(1),
            Op::AddWindow {
                id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::SetWindowHeight {
                id: None,
                change: SizeChange::AdjustProportion(-1e129),
            },
        ];

        let mut options = Options::default();
        options.border.off = false;
        options.border.width = FloatOrInt(1.);

        check_ops_with_options(options, &ops);
    }

    #[test]
    fn large_max_size() {
        let ops = [
            Op::AddOutput(1),
            Op::AddWindow {
                id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: (Size::from((0, 0)), Size::from((i32::MAX, i32::MAX))),
            },
        ];

        let mut options = Options::default();
        options.border.off = false;
        options.border.width = FloatOrInt(1.);

        check_ops_with_options(options, &ops);
    }

    #[test]
    fn workspace_cleanup_during_switch() {
        let ops = [
            Op::AddOutput(1),
            Op::AddWindow {
                id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: (Size::from((0, 0)), Size::from((i32::MAX, i32::MAX))),
            },
            Op::FocusWorkspaceDown,
            Op::CloseWindow(1),
        ];

        check_ops(&ops);
    }

    #[test]
    fn workspace_transfer_during_switch() {
        let ops = [
            Op::AddOutput(1),
            Op::AddWindow {
                id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: (Size::from((0, 0)), Size::from((i32::MAX, i32::MAX))),
            },
            Op::AddOutput(2),
            Op::FocusOutput(2),
            Op::AddWindow {
                id: 2,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: (Size::from((0, 0)), Size::from((i32::MAX, i32::MAX))),
            },
            Op::RemoveOutput(1),
            Op::FocusWorkspaceDown,
            Op::FocusWorkspaceDown,
            Op::AddOutput(1),
        ];

        check_ops(&ops);
    }

    #[test]
    fn workspace_transfer_during_switch_from_last() {
        let ops = [
            Op::AddOutput(1),
            Op::AddWindow {
                id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: (Size::from((0, 0)), Size::from((i32::MAX, i32::MAX))),
            },
            Op::AddOutput(2),
            Op::RemoveOutput(1),
            Op::FocusWorkspaceUp,
            Op::AddOutput(1),
        ];

        check_ops(&ops);
    }

    #[test]
    fn workspace_transfer_during_switch_gets_cleaned_up() {
        let ops = [
            Op::AddOutput(1),
            Op::AddWindow {
                id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: (Size::from((0, 0)), Size::from((i32::MAX, i32::MAX))),
            },
            Op::RemoveOutput(1),
            Op::AddOutput(2),
            Op::MoveColumnToWorkspaceDown,
            Op::MoveColumnToWorkspaceDown,
            Op::AddOutput(1),
        ];

        check_ops(&ops);
    }

    #[test]
    fn move_workspace_to_output() {
        let ops = [
            Op::AddOutput(1),
            Op::AddOutput(2),
            Op::FocusOutput(1),
            Op::AddWindow {
                id: 0,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::MoveWorkspaceToOutput(2),
        ];

        let mut layout = Layout::default();
        for op in ops {
            op.apply(&mut layout);
        }

        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = layout.monitor_set
        else {
            unreachable!()
        };

        assert_eq!(active_monitor_idx, 1);
        assert_eq!(monitors[0].workspaces.len(), 1);
        assert!(!monitors[0].workspaces[0].has_windows());
        assert_eq!(monitors[1].active_workspace_idx, 0);
        assert_eq!(monitors[1].workspaces.len(), 2);
        assert!(monitors[1].workspaces[0].has_windows());
    }

    #[test]
    fn fullscreen() {
        let ops = [
            Op::AddOutput(1),
            Op::AddWindow {
                id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: (Size::from((0, 0)), Size::from((i32::MAX, i32::MAX))),
            },
            Op::FullscreenWindow(1),
        ];

        check_ops(&ops);
    }

    #[test]
    fn unfullscreen_window_in_column() {
        let ops = [
            Op::AddOutput(1),
            Op::AddWindow {
                id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: (Size::from((0, 0)), Size::from((i32::MAX, i32::MAX))),
            },
            Op::AddWindow {
                id: 2,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: (Size::from((0, 0)), Size::from((i32::MAX, i32::MAX))),
            },
            Op::ConsumeOrExpelWindowLeft { id: None },
            Op::SetFullscreenWindow {
                window: 2,
                is_fullscreen: false,
            },
        ];

        check_ops(&ops);
    }

    #[test]
    fn open_right_of_on_different_workspace() {
        let ops = [
            Op::AddOutput(1),
            Op::AddWindow {
                id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: (Size::from((0, 0)), Size::from((i32::MAX, i32::MAX))),
            },
            Op::FocusWorkspaceDown,
            Op::AddWindow {
                id: 2,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: (Size::from((0, 0)), Size::from((i32::MAX, i32::MAX))),
            },
            Op::AddWindowRightOf {
                id: 3,
                right_of_id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: (Size::from((0, 0)), Size::from((i32::MAX, i32::MAX))),
            },
        ];

        let mut layout = Layout::default();
        for op in ops {
            op.apply(&mut layout);
        }

        let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
            unreachable!()
        };

        let mon = monitors.into_iter().next().unwrap();
        assert_eq!(
            mon.active_workspace_idx, 1,
            "the second workspace must remain active"
        );
        assert_eq!(
            mon.workspaces[0].active_column_idx, 1,
            "the new window must become active"
        );
    }

    #[test]
    fn unfullscreen_view_offset_not_reset_on_removal() {
        let ops = [
            Op::AddOutput(1),
            Op::AddWindow {
                id: 0,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::FullscreenWindow(0),
            Op::AddWindow {
                id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::ConsumeOrExpelWindowRight { id: None },
        ];

        check_ops(&ops);
    }

    #[test]
    fn unfullscreen_view_offset_not_reset_on_consume() {
        let ops = [
            Op::AddOutput(1),
            Op::AddWindow {
                id: 0,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::FullscreenWindow(0),
            Op::AddWindow {
                id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::ConsumeWindowIntoColumn,
        ];

        check_ops(&ops);
    }

    #[test]
    fn unfullscreen_view_offset_not_reset_on_quick_double_toggle() {
        let ops = [
            Op::AddOutput(1),
            Op::AddWindow {
                id: 0,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::FullscreenWindow(0),
            Op::FullscreenWindow(0),
        ];

        check_ops(&ops);
    }

    #[test]
    fn unfullscreen_view_offset_set_on_fullscreening_inactive_tile_in_column() {
        let ops = [
            Op::AddOutput(1),
            Op::AddWindow {
                id: 0,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::AddWindow {
                id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::ConsumeOrExpelWindowLeft { id: None },
            Op::FullscreenWindow(0),
        ];

        check_ops(&ops);
    }

    #[test]
    fn unfullscreen_view_offset_not_reset_on_gesture() {
        let ops = [
            Op::AddOutput(1),
            Op::AddWindow {
                id: 0,
                bbox: Rectangle::from_loc_and_size((0, 0), (200, 200)),
                min_max_size: Default::default(),
            },
            Op::AddWindow {
                id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (1280, 200)),
                min_max_size: Default::default(),
            },
            Op::FullscreenWindow(1),
            Op::ViewOffsetGestureBegin {
                output_idx: 1,
                is_touchpad: true,
            },
            Op::ViewOffsetGestureEnd {
                is_touchpad: Some(true),
            },
        ];

        check_ops(&ops);
    }

    #[test]
    fn removing_all_outputs_preserves_empty_named_workspaces() {
        let ops = [
            Op::AddOutput(1),
            Op::AddNamedWorkspace {
                ws_name: 1,
                output_name: None,
            },
            Op::AddNamedWorkspace {
                ws_name: 2,
                output_name: None,
            },
            Op::RemoveOutput(1),
        ];

        let mut layout = Layout::default();
        for op in ops {
            op.apply(&mut layout);
        }

        let MonitorSet::NoOutputs { workspaces } = layout.monitor_set else {
            unreachable!()
        };

        assert_eq!(workspaces.len(), 2);
    }

    #[test]
    fn config_change_updates_cached_sizes() {
        let mut config = Config::default();
        config.layout.border.off = false;
        config.layout.border.width = FloatOrInt(2.);

        let mut layout = Layout::new(&config);

        Op::AddWindow {
            id: 1,
            bbox: Rectangle::from_loc_and_size((0, 0), (1280, 200)),
            min_max_size: Default::default(),
        }
        .apply(&mut layout);

        config.layout.border.width = FloatOrInt(4.);
        layout.update_config(&config);

        layout.verify_invariants();
    }

    #[test]
    fn preset_height_change_removes_preset() {
        let mut config = Config::default();
        config.layout.preset_window_heights = vec![PresetSize::Fixed(1), PresetSize::Fixed(2)];

        let mut layout = Layout::new(&config);

        let ops = [
            Op::AddOutput(1),
            Op::AddWindow {
                id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (1280, 200)),
                min_max_size: Default::default(),
            },
            Op::AddWindow {
                id: 2,
                bbox: Rectangle::from_loc_and_size((0, 0), (1280, 200)),
                min_max_size: Default::default(),
            },
            Op::ConsumeOrExpelWindowLeft { id: None },
            Op::SwitchPresetWindowHeight { id: None },
            Op::SwitchPresetWindowHeight { id: None },
        ];
        for op in ops {
            op.apply(&mut layout);
        }

        // Leave only one.
        config.layout.preset_window_heights = vec![PresetSize::Fixed(1)];

        layout.update_config(&config);

        layout.verify_invariants();
    }

    #[test]
    fn working_area_starts_at_physical_pixel() {
        let struts = Struts {
            left: FloatOrInt(0.5),
            right: FloatOrInt(1.),
            top: FloatOrInt(0.75),
            bottom: FloatOrInt(1.),
        };

        let output = Output::new(
            String::from("output"),
            PhysicalProperties {
                size: Size::from((1280, 720)),
                subpixel: Subpixel::Unknown,
                make: String::new(),
                model: String::new(),
            },
        );
        output.change_current_state(
            Some(Mode {
                size: Size::from((1280, 720)),
                refresh: 60000,
            }),
            None,
            None,
            None,
        );

        let area = compute_working_area(&output, struts);

        assert_eq!(round_logical_in_physical(1., area.loc.x), area.loc.x);
        assert_eq!(round_logical_in_physical(1., area.loc.y), area.loc.y);
    }

    #[test]
    fn large_fractional_strut() {
        let struts = Struts {
            left: FloatOrInt(0.),
            right: FloatOrInt(0.),
            top: FloatOrInt(50000.5),
            bottom: FloatOrInt(0.),
        };

        let output = Output::new(
            String::from("output"),
            PhysicalProperties {
                size: Size::from((1280, 720)),
                subpixel: Subpixel::Unknown,
                make: String::new(),
                model: String::new(),
            },
        );
        output.change_current_state(
            Some(Mode {
                size: Size::from((1280, 720)),
                refresh: 60000,
            }),
            None,
            None,
            None,
        );

        compute_working_area(&output, struts);
    }

    #[test]
    fn set_window_height_recomputes_to_auto() {
        let ops = [
            Op::AddOutput(1),
            Op::AddWindow {
                id: 0,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::AddWindow {
                id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::ConsumeOrExpelWindowLeft { id: None },
            Op::AddWindow {
                id: 2,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::ConsumeOrExpelWindowLeft { id: None },
            Op::SetWindowHeight {
                id: None,
                change: SizeChange::SetFixed(100),
            },
            Op::FocusWindowUp,
            Op::SetWindowHeight {
                id: None,
                change: SizeChange::SetFixed(200),
            },
        ];

        check_ops(&ops);
    }

    #[test]
    fn one_window_in_column_becomes_weight_1() {
        let ops = [
            Op::AddOutput(1),
            Op::AddWindow {
                id: 0,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::AddWindow {
                id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::ConsumeOrExpelWindowLeft { id: None },
            Op::AddWindow {
                id: 2,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::ConsumeOrExpelWindowLeft { id: None },
            Op::SetWindowHeight {
                id: None,
                change: SizeChange::SetFixed(100),
            },
            Op::Communicate(2),
            Op::FocusWindowUp,
            Op::SetWindowHeight {
                id: None,
                change: SizeChange::SetFixed(200),
            },
            Op::Communicate(1),
            Op::CloseWindow(0),
            Op::CloseWindow(1),
        ];

        check_ops(&ops);
    }

    #[test]
    fn one_window_in_column_becomes_weight_1_after_fullscreen() {
        let ops = [
            Op::AddOutput(1),
            Op::AddWindow {
                id: 0,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::AddWindow {
                id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::ConsumeOrExpelWindowLeft { id: None },
            Op::AddWindow {
                id: 2,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::ConsumeOrExpelWindowLeft { id: None },
            Op::SetWindowHeight {
                id: None,
                change: SizeChange::SetFixed(100),
            },
            Op::Communicate(2),
            Op::FocusWindowUp,
            Op::SetWindowHeight {
                id: None,
                change: SizeChange::SetFixed(200),
            },
            Op::Communicate(1),
            Op::CloseWindow(0),
            Op::FullscreenWindow(1),
        ];

        check_ops(&ops);
    }

    #[test]
    fn fixed_height_takes_max_non_auto_into_account() {
        let ops = [
            Op::AddOutput(1),
            Op::AddWindow {
                id: 0,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::SetWindowHeight {
                id: Some(0),
                change: SizeChange::SetFixed(704),
            },
            Op::AddWindow {
                id: 1,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::ConsumeOrExpelWindowLeft { id: None },
        ];

        let options = Options {
            border: niri_config::Border {
                off: false,
                width: niri_config::FloatOrInt(4.),
                ..Default::default()
            },
            gaps: 0.,
            ..Default::default()
        };
        check_ops_with_options(options, &ops);
    }

    #[test]
    fn start_interactive_move_then_remove_window() {
        let ops = [
            Op::AddOutput(1),
            Op::AddWindow {
                id: 0,
                bbox: Rectangle::from_loc_and_size((0, 0), (100, 200)),
                min_max_size: Default::default(),
            },
            Op::InteractiveMoveBegin {
                window: 0,
                output_idx: 1,
                px: 0.,
                py: 0.,
            },
            Op::CloseWindow(0),
        ];

        check_ops(&ops);
    }

    fn arbitrary_spacing() -> impl Strategy<Value = f64> {
        // Give equal weight to:
        // - 0: the element is disabled
        // - 4: some reasonable value
        // - random value, likely unreasonably big
        prop_oneof![Just(0.), Just(4.), ((1.)..=65535.)]
    }

    fn arbitrary_spacing_neg() -> impl Strategy<Value = f64> {
        // Give equal weight to:
        // - 0: the element is disabled
        // - 4: some reasonable value
        // - -4: some reasonable negative value
        // - random value, likely unreasonably big
        prop_oneof![Just(0.), Just(4.), Just(-4.), ((1.)..=65535.)]
    }

    fn arbitrary_struts() -> impl Strategy<Value = Struts> {
        (
            arbitrary_spacing_neg(),
            arbitrary_spacing_neg(),
            arbitrary_spacing_neg(),
            arbitrary_spacing_neg(),
        )
            .prop_map(|(left, right, top, bottom)| Struts {
                left: FloatOrInt(left),
                right: FloatOrInt(right),
                top: FloatOrInt(top),
                bottom: FloatOrInt(bottom),
            })
    }

    fn arbitrary_center_focused_column() -> impl Strategy<Value = CenterFocusedColumn> {
        prop_oneof![
            Just(CenterFocusedColumn::Never),
            Just(CenterFocusedColumn::OnOverflow),
            Just(CenterFocusedColumn::Always),
        ]
    }

    prop_compose! {
        fn arbitrary_focus_ring()(
            off in any::<bool>(),
            width in arbitrary_spacing(),
        ) -> niri_config::FocusRing {
            niri_config::FocusRing {
                off,
                width: FloatOrInt(width),
                ..Default::default()
            }
        }
    }

    prop_compose! {
        fn arbitrary_border()(
            off in any::<bool>(),
            width in arbitrary_spacing(),
        ) -> niri_config::Border {
            niri_config::Border {
                off,
                width: FloatOrInt(width),
                ..Default::default()
            }
        }
    }

    prop_compose! {
        fn arbitrary_options()(
            gaps in arbitrary_spacing(),
            struts in arbitrary_struts(),
            focus_ring in arbitrary_focus_ring(),
            border in arbitrary_border(),
            center_focused_column in arbitrary_center_focused_column(),
            always_center_single_column in any::<bool>(),
        ) -> Options {
            Options {
                gaps,
                struts,
                center_focused_column,
                always_center_single_column,
                focus_ring,
                border,
                ..Default::default()
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: if std::env::var_os("RUN_SLOW_TESTS").is_none() {
                eprintln!("ignoring slow test");
                0
            } else {
                ProptestConfig::default().cases
            },
            ..ProptestConfig::default()
        })]

        #[test]
        fn random_operations_dont_panic(ops: Vec<Op>, options in arbitrary_options()) {
            // eprintln!("{ops:?}");
            check_ops_with_options(options, &ops);
        }
    }
}
