//! The plot window: a side panel with settings and the plot itself.

use crate::hist::{self, BinRule, Histogram};
use crate::lod::{decimate_line, decimate_points};
use crate::model::{Geom, Kind, PlotState, Series};
use crate::scale::{log_label, log_ticks, to_log};
use crate::view3d::{Series3d, View3d};
use eframe::egui;
use egui_plot::{
  Bar, BarChart, GridInput, GridMark, HoverPosition, Legend, Line, Plot, PlotPoint, PlotPoints,
  PlotResponse, PlotUi, Points,
};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// Series with more points than this are decimated to what fits on screen.
const LOD_MIN: usize = 5_000;

/// Ids of the x–y and histogram plots. Fixed, so that the tests can read their transforms.
const XY_PLOT_ID: &str = "complot_plot";
const HIST_PLOT_ID: &str = "complot_hist";

/// How long the note after a copy stays on screen, in seconds.
const COPY_NOTE_SECS: f64 = 1.2;

/// Tableau 10, cycled by series index.
const PALETTE: [egui::Color32; 10] = [
  egui::Color32::from_rgb(0x4e, 0x79, 0xa7),
  egui::Color32::from_rgb(0xf2, 0x8e, 0x2b),
  egui::Color32::from_rgb(0xe1, 0x57, 0x59),
  egui::Color32::from_rgb(0x76, 0xb7, 0xb2),
  egui::Color32::from_rgb(0x59, 0xa1, 0x4f),
  egui::Color32::from_rgb(0xed, 0xc9, 0x48),
  egui::Color32::from_rgb(0xb0, 0x7a, 0xa1),
  egui::Color32::from_rgb(0xff, 0x9d, 0xa7),
  egui::Color32::from_rgb(0x9c, 0x75, 0x5f),
  egui::Color32::from_rgb(0xba, 0xb0, 0xac),
];

/// Settings panel width when expanded, the default and the range it can be dragged over.
const SETTINGS_DEFAULT: f32 = 210.0;
const SETTINGS_MIN: f32 = 200.0;
const SETTINGS_MAX: f32 = 400.0;

/// Settings panel width when hidden: just wide enough for its button.
const SETTINGS_HIDDEN: f32 = 32.0;

/// Button glyphs, present in egui's default proportional font.
const HIDE_GLYPH: &str = "⏴";
const SHOW_GLYPH: &str = "⏵";

/// Side of the colour swatch of a series row, and the width of the geometry box beside its name.
const SWATCH: f32 = 10.0;
const GEOM_BOX: f32 = 76.0;

/// The geometries, in the order their choices are listed.
const GEOMS: [Geom; 3] = [Geom::Points, Geom::Lines, Geom::Both];

/// Opens the window for plot `name` and blocks until it is closed.
///
/// `on_start` runs once the egui context exists; the server uses it to
/// start accepting connections with a context it can request repaints from.
pub fn run(
  name: &str,
  state: Arc<Mutex<PlotState>>,
  on_start: Box<dyn FnOnce(egui::Context) + Send>,
) -> Result<(), String> {
  let viewport = egui::ViewportBuilder::default()
    .with_title(format!("complot — {name}"))
    .with_inner_size([900.0, 560.0])
    .with_min_inner_size([480.0, 320.0])
    .with_app_id("complot");
  let options = eframe::NativeOptions {
    viewport,
    ..Default::default()
  };
  eframe::run_native(
    "complot",
    options,
    Box::new(move |cc| {
      on_start(cc.egui_ctx.clone());
      Ok(Box::new(PlotApp::new(name, state)))
    }),
  )
  .map_err(|e| e.to_string())
}

/// Where the settings panel was drawn in a frame. The tests check these rects.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(test), allow(dead_code))]
struct PanelRects {
  /// Outer rect of the panel, frame included.
  outer: egui::Rect,
  /// Rect of the content, as laid out.
  content: egui::Rect,
  /// Rect of the button that hides or shows the panel.
  button: egui::Rect,
}

/// How the bars of a histogram's series share each bin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistLayout {
  /// Side by side, one bar per series.
  Side,
  /// Stacked, each series on the ones before it.
  Stack,
}

/// A histogram and the key it was computed for: data version, bin rule and series count. The
/// values are binned in log10 if `log_x`.
struct HistCache {
  key: (u64, BinRule, usize),
  log_x: bool,
  /// `None` when there are no values.
  hist: Option<Histogram>,
}

/// The series with their points mapped to log10 on the log axes, and the points left out.
struct LogCache {
  /// Data version, log axes (x, y) and series count the cache was built for.
  key: (u64, [bool; 2], usize),
  series: Vec<Series>,
  /// Per series, where a run of kept points starts after left-out points; see `log_series`.
  starts: Vec<Vec<usize>>,
  /// Points left out because a coordinate has no place on a log axis.
  dropped: usize,
}

/// What the copy menu offers. A right click freezes it, so that the menu shows and copies the
/// values of the click whatever the pointer does while the menu is open. Values are as the axes
/// show them: 10^v on a log axis.
struct CopyMenu {
  /// Where the click was. The menu opens here, and the note appears here.
  at: egui::Pos2,
  /// Series and (x, y) of the data point the hover snapped to, if it did.
  point: Option<(String, [f64; 2])>,
  /// (x, y) of the cursor.
  cursor: [f64; 2],
}

/// The note after a copy, drawn at the menu until [`COPY_NOTE_SECS`] have passed.
struct CopyNote {
  at: egui::Pos2,
  text: String,
  /// egui time of the copy.
  since: f64,
}

/// Settings and view state of one window. Settings live in memory only.
struct PlotApp {
  name: String,
  state: Arc<Mutex<PlotState>>,
  point_radius: f32,
  line_width: f32,
  legend: bool,
  grid: bool,
  /// Per-series buffer limit shown in the panel, 0 = unlimited.
  max_points: usize,
  /// Set by the Fit button; the next frame turns follow on.
  fit_request: bool,
  /// Set when the Follow checkbox changes; the next frame applies it.
  follow_request: Option<bool>,
  /// Whether the plot is following the data (auto-bounds), as of the last frame.
  follow: bool,
  /// Title last sent to the window, so that only changes are sent.
  applied_title: Option<String>,
  /// Points drawn and points stored in the last frame, if a series was decimated.
  decimated: Option<(usize, usize)>,
  /// Whether the settings panel is expanded.
  settings_open: bool,
  /// Bin rule of a histogram.
  bin_rule: BinRule,
  hist_layout: HistLayout,
  /// The histogram of the data, shared by the status line and the plot.
  hist_cache: Option<HistCache>,
  /// Log scale of the x, y and z axes.
  log: [bool; 3],
  /// The series mapped to log10, shared by the x–y plot and the histogram.
  log_cache: Option<LogCache>,
  /// Points left out for a value ≤ 0 on a log axis, as of the last frame.
  dropped: usize,
  /// Camera and display state of the 3D view.
  view3d: View3d,
  /// The copy menu of the last right click on a plot, whose values it keeps.
  copy_menu: Option<CopyMenu>,
  /// The note of the last copy, while it is shown.
  copy_note: Option<CopyNote>,
}

/// Locks the plot state. A poisoned lock is still used, since a panic cannot leave the data invalid.
fn lock(state: &Mutex<PlotState>) -> MutexGuard<'_, PlotState> {
  state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Locks the plot state with the points and z values of every series contiguous, so that
/// they can be sliced.
fn lock_contiguous(state: &Mutex<PlotState>) -> MutexGuard<'_, PlotState> {
  let mut st = lock(state);
  for s in st.series.iter_mut() {
    s.points.make_contiguous();
    if let Some(z) = s.z.as_mut() {
      z.make_contiguous();
    }
  }
  st
}

/// Colour of series `i`, cycling through [`PALETTE`].
fn palette(i: usize) -> egui::Color32 {
  PALETTE[i % PALETTE.len()]
}

/// Whether a series drawn with `geom` has markers.
fn has_points(geom: Geom) -> bool {
  geom != Geom::Lines
}

/// Whether a series drawn with `geom` has a line.
fn has_lines(geom: Geom) -> bool {
  geom != Geom::Points
}

/// Name of a geometry, as the choices show it.
fn geom_name(geom: Geom) -> &'static str {
  match geom {
    Geom::Points => "Points",
    Geom::Lines => "Lines",
    Geom::Both => "Both",
  }
}

/// Whether some series of `st` is drawn with a geometry that `pred` accepts. With no series,
/// the default geometry stands in.
fn any_drawn(st: &PlotState, pred: fn(Geom) -> bool) -> bool {
  if st.series.is_empty() {
    pred(st.default_geom)
  } else {
    st.series.iter().any(|s| pred(st.geom_of(&s.name)))
  }
}

/// Range to decimate a series over, as `[x0, x1, y0, y1]`.
///
/// An axis that follows the data (`auto`) uses the data's extent, so that
/// points arriving outside the view are kept and the view can grow. The other
/// axes use the visible bounds `min` and `max`.
fn decimation_range(
  points: &[PlotPoint],
  sorted: bool,
  auto: egui::Vec2b,
  min: [f64; 2],
  max: [f64; 2],
) -> [f64; 4] {
  let mut range = [min[0], max[0], min[1], max[1]];
  if auto.any()
    && let Some(data) = data_extent(points, sorted)
  {
    if auto.x {
      range[0..2].copy_from_slice(&data[0..2]);
    }
    if auto.y {
      range[2..4].copy_from_slice(&data[2..4]);
    }
  }
  range
}

/// Extent of `points` as `[x0, x1, y0, y1]`, or `None` if there are none.
/// For sorted points the x range is read from the first and last point.
fn data_extent(points: &[PlotPoint], sorted: bool) -> Option<[f64; 4]> {
  let (first, last) = (points.first()?, points.last()?);
  let (mut x0, mut x1) = (f64::INFINITY, f64::NEG_INFINITY);
  let (mut y0, mut y1) = (f64::INFINITY, f64::NEG_INFINITY);
  for p in points {
    y0 = y0.min(p.y);
    y1 = y1.max(p.y);
    if !sorted {
      x0 = x0.min(p.x);
      x1 = x1.max(p.x);
    }
  }
  if sorted {
    x0 = first.x;
    x1 = last.x;
  }
  Some([x0, x1, y0, y1])
}

/// A slider with its label on the line above, the slider filling the rest of the row.
fn slider(ui: &mut egui::Ui, label: &str, value: &mut f32, range: RangeInclusive<f32>) {
  ui.label(label);
  ui.scope(|ui| {
    // The value box after the slider is a DragValue, at least `interact_size.x` wide. One
    // decimal keeps its text ("10.0" at most) within that minimum, so the room left is exact.
    let value_box = ui.spacing().interact_size.x + ui.spacing().item_spacing.x;
    let rail = (ui.available_width() - value_box).max(20.0);
    ui.spacing_mut().slider_width = rail;
    ui.add(egui::Slider::new(value, range).fixed_decimals(1));
  });
}

/// A small button that sets `request` to `open` when clicked. Returns its rect.
fn toggle_button(
  ui: &mut egui::Ui,
  glyph: &str,
  tip: &str,
  request: &mut Option<bool>,
  open: bool,
) -> egui::Rect {
  let response = ui.small_button(glyph).on_hover_text(tip);
  if response.clicked() {
    *request = Some(open);
  }
  response.rect
}

/// Histogram of `series`, the series of `st` mapped to log10 if `log_x`, from `cache` if its key
/// still matches. The points must be contiguous, as [`lock_contiguous`] and [`log_series`] make them.
fn cached_histogram<'a>(
  cache: &'a mut Option<HistCache>,
  rule: BinRule,
  log_x: bool,
  st: &PlotState,
  series: &[Series],
) -> Option<&'a Histogram> {
  let key = (st.version, rule, st.series.len());
  if cache
    .as_ref()
    .is_none_or(|c| c.key != key || c.log_x != log_x)
  {
    let slices: Vec<&[PlotPoint]> = series.iter().map(|s| s.points.as_slices().0).collect();
    *cache = Some(HistCache {
      key,
      log_x,
      hist: hist::histogram(&slices, rule),
    });
  }
  cache.as_ref()?.hist.as_ref()
}

/// The series to draw or bin, with their points mapped to log10 on the axes `logs` (x, y). With
/// no log axis these are the series of `st` themselves. Otherwise they come from `cache`, rebuilt
/// when the data, the axes or the series count change. Also returns where runs of kept points
/// start (empty with no log axis) and the number of points left out.
fn mapped_series<'a>(
  cache: &'a mut Option<LogCache>,
  st: &'a PlotState,
  logs: [bool; 2],
) -> (&'a [Series], &'a [Vec<usize>], usize) {
  if logs == [false; 2] {
    *cache = None;
    return (&st.series, &[], 0);
  }
  let key = (st.version, logs, st.series.len());
  if cache.as_ref().is_none_or(|c| c.key != key) {
    let (series, starts, dropped) = log_series(&st.series, logs);
    *cache = Some(LogCache {
      key,
      series,
      starts,
      dropped,
    });
  }
  // The cache was filled above when it did not match.
  let c = cache.as_ref().expect("the cache was filled above");
  (&c.series, &c.starts, c.dropped)
}

/// Copies of `series` with their points mapped to log10 on the axes `logs` (x, y). A point with
/// a coordinate that has no place on a log axis is left out. Also returns, per series, the
/// indices of the mapped points that start a new run because points were left out just before
/// them (a line must not bridge that gap), and how many points were left out in all.
/// Log is monotonic, so the mapped series keep their order and their `sorted` flag.
fn log_series(series: &[Series], logs: [bool; 2]) -> (Vec<Series>, Vec<Vec<usize>>, usize) {
  let mut dropped = 0;
  let (mapped, starts) = series
    .iter()
    .map(|s| {
      let mut points = VecDeque::with_capacity(s.points.len());
      let mut starts = Vec::new();
      let mut gap = false;
      for p in &s.points {
        let x = if logs[0] { to_log(p.x) } else { Some(p.x) };
        let y = if logs[1] { to_log(p.y) } else { Some(p.y) };
        match x.zip(y) {
          Some((x, y)) => {
            if gap && !points.is_empty() {
              starts.push(points.len());
            }
            gap = false;
            points.push_back(PlotPoint::new(x, y));
          }
          None => {
            dropped += 1;
            gap = true;
          }
        }
      }
      points.make_contiguous();
      let series = Series {
        name: s.name.clone(),
        points,
        sorted: s.sorted,
        ..Default::default()
      };
      (series, starts)
    })
    .unzip();
  (mapped, starts, dropped)
}

/// `points` split at `starts` (ascending indices where a run begins) into runs.
fn runs<'a>(points: &'a [PlotPoint], starts: &[usize]) -> impl Iterator<Item = &'a [PlotPoint]> {
  let bounds: Vec<usize> = std::iter::once(0)
    .chain(
      starts
        .iter()
        .copied()
        .filter(|&i| i > 0 && i < points.len()),
    )
    .chain(std::iter::once(points.len()))
    .collect();
  (0..bounds.len() - 1).map(move |k| &points[bounds[k]..bounds[k + 1]])
}

/// Bars of group `group` of `groups` in each bin of `h`, as (argument, width, height).
///
/// Side by side, the bars of a bin share 90% of its width, one slot per group. Stacked, each
/// bar is centred on its bin, 90% wide. Zero-height bars are kept, so that bars pair by index.
fn bar_layout(
  h: &Histogram,
  layout: HistLayout,
  group: usize,
  groups: usize,
) -> Vec<(f64, f64, f64)> {
  (0..h.bins())
    .map(|b| {
      let height = h.counts[group][b] as f64;
      match layout {
        HistLayout::Side => {
          let bar_w = 0.9 * h.width / groups.max(1) as f64;
          let arg = h.start + b as f64 * h.width + 0.05 * h.width + (group as f64 + 0.5) * bar_w;
          (arg, bar_w, height)
        }
        HistLayout::Stack => (h.center(b), 0.9 * h.width, height),
      }
    })
    .collect()
}

/// Bars of group `group` of `groups` when the counts are on a log axis, as (argument, width,
/// base, height). A count c ≥ 1 rises from zero to log10(c), and a count of 0 draws no bar, so it
/// is left out. Stacked, log heights do not add up: the bar of group g spans log10 of the
/// cumulative counts of the groups before it (zero below the first) up to log10 of those with g.
/// Arguments and widths are those of [`bar_layout`].
fn log_bars(
  h: &Histogram,
  layout: HistLayout,
  group: usize,
  groups: usize,
) -> Vec<(f64, f64, f64, f64)> {
  bar_layout(h, layout, group, groups)
    .into_iter()
    .enumerate()
    .filter_map(|(b, (arg, width, _))| {
      let count = h.counts[group][b];
      if count == 0 {
        return None;
      }
      let (base, top) = match layout {
        HistLayout::Side => (0.0, (count as f64).log10()),
        HistLayout::Stack => {
          let below: u64 = h.counts[..group].iter().map(|c| c[b]).sum();
          let base = if below == 0 {
            0.0
          } else {
            (below as f64).log10()
          };
          (base, ((below + count) as f64).log10())
        }
      };
      Some((arg, width, base, top - base))
    })
    .collect()
}

/// One bar chart per series, in series order. In a stack, each chart sits on the ones before it,
/// unless the counts are on a log axis: then each bar is placed by [`log_bars`].
fn bar_charts(h: &Histogram, layout: HistLayout, series: &[Series], log_y: bool) -> Vec<BarChart> {
  let mut charts: Vec<BarChart> = Vec::with_capacity(series.len());
  for (g, s) in series.iter().enumerate() {
    let color = palette(g);
    // egui_plot's default fill is 20% opaque, too faint to tell stacked bars apart.
    let (fill, stroke) = (color.gamma_multiply(0.6), egui::Stroke::new(1.0, color));
    let bars: Vec<Bar> = if log_y {
      log_bars(h, layout, g, series.len())
        .into_iter()
        .map(|(arg, width, base, height)| {
          Bar::new(arg, height)
            .base_offset(base)
            .width(width)
            .fill(fill)
            .stroke(stroke)
        })
        .collect()
    } else {
      bar_layout(h, layout, g, series.len())
        .into_iter()
        .map(|(arg, width, height)| Bar::new(arg, height).width(width).fill(fill).stroke(stroke))
        .collect()
    };
    let chart = BarChart::new(s.name.as_str(), bars).color(color);
    let chart = match layout {
      HistLayout::Stack if !log_y => {
        let below: Vec<&BarChart> = charts.iter().collect();
        chart.stack_on(&below)
      }
      _ => chart,
    };
    charts.push(chart);
  }
  charts
}

/// Applies a Follow or Fit request to `plot_ui`. Returns the auto-bounds in effect.
fn apply_follow(plot_ui: &mut PlotUi<'_>, request: Option<bool>) -> egui::Vec2b {
  match request {
    Some(on) => {
      plot_ui.set_auto_bounds(on);
      on.into()
    }
    None => plot_ui.auto_bounds(),
  }
}

/// The hint shown in place of the plot while there is no data, centred in `rect`.
fn paint_waiting(ui: &egui::Ui, rect: egui::Rect, name: &str) {
  ui.painter().text(
    rect.center(),
    egui::Align2::CENTER_CENTER,
    format!("waiting for data…  complot {name} -y 1"),
    egui::FontId::proportional(16.0),
    ui.visuals().weak_text_color(),
  );
}

/// Grid marks of a log axis over `bounds`, which are exponents. Whole decades get step 1 and the
/// subdivisions 0.1, so that egui_plot draws the decades stronger.
fn log_grid(input: GridInput) -> Vec<GridMark> {
  log_ticks(input.bounds.0, input.bounds.1, 10)
    .into_iter()
    .map(|value| {
      let decade = (value - value.round()).abs() < 1e-9;
      GridMark {
        value,
        step_size: if decade { 1.0 } else { 0.1 },
      }
    })
    .collect()
}

/// Label of a log axis at `mark`: the value 10^`mark.value`.
fn log_axis_label(mark: GridMark, _range: &RangeInclusive<f64>) -> String {
  log_label(mark.value)
}

/// Axis title, with " (log)" after the label on a log axis. A linear axis without a label stays
/// without one.
fn axis_title(label: &str, log: bool) -> String {
  match (label.is_empty(), log) {
    (_, false) => label.to_string(),
    (true, true) => "(log)".to_string(),
    (false, true) => format!("{label} (log)"),
  }
}

/// `v` as the axis shows it: 10^v on a log axis, else `v`.
fn unlog(v: f64, log: bool) -> f64 {
  if log { 10f64.powf(v) } else { v }
}

/// Id of the copy menu. Both plots use it, so that a click that changes the kind closes the menu.
fn copy_menu_id() -> egui::Id {
  egui::Id::new("complot_copy_menu")
}

/// `v` as the plot draws it: log10 on a log axis, else `v`. `None` where a log axis has no place
/// for `v`.
fn drawn_value(v: f64, log: bool) -> Option<f64> {
  if log { to_log(v) } else { Some(v) }
}

/// The sample of series `name` that the plot draws at `at`, as it was pushed. Both sides use
/// [`drawn_value`], so a drawn point equals its sample's drawn value exactly.
fn sample_at(st: &PlotState, name: &str, at: [f64; 2], logs: [bool; 2]) -> Option<[f64; 2]> {
  let series = st.series.iter().find(|s| s.name == name)?;
  series.points.iter().find_map(|p| {
    let x = drawn_value(p.x, logs[0])?;
    let y = drawn_value(p.y, logs[1])?;
    (x == at[0] && y == at[1]).then_some([p.x, p.y])
  })
}

/// What a right click on a plot freezes for the copy menu: the cursor, and the data point the
/// hover snapped to, if any. A snapped point copies its sample as pushed, found in `st`; the
/// cursor has none, so it is `10^v` on a log axis. `logs` says which axes are logarithmic.
fn copy_snapshot<R>(
  response: &PlotResponse<R>,
  st: &PlotState,
  snapped: Option<(String, [f64; 2])>,
  logs: [bool; 2],
) -> Option<CopyMenu> {
  let at = response.response.hover_pos()?;
  let cursor = response.transform.value_from_position(at);
  let point = snapped.map(|(series, drawn)| {
    let [x, y] = sample_at(st, &series, drawn, logs)
      .unwrap_or([unlog(drawn[0], logs[0]), unlog(drawn[1], logs[1])]);
    (series, [x, y])
  });
  Some(CopyMenu {
    at,
    point,
    cursor: [unlog(cursor.x, logs[0]), unlog(cursor.y, logs[1])],
  })
}

/// Shows the copy menu `menu` over the plot `response`, while it is open. Returns the text of the
/// button clicked, if one was: "x,y" at full precision.
fn copy_menu_items(response: &egui::Response, menu: &CopyMenu) -> Option<String> {
  let mut copied = None;
  egui::Popup::context_menu(response)
    .id(copy_menu_id())
    .show(|ui| {
      if let Some((series, [x, y])) = &menu.point
        && ui
          .button(format!("Copy point  {series}: {}, {}", sig4(*x), sig4(*y)))
          .clicked()
      {
        copied = Some(format!("{x},{y}"));
        ui.close();
      }
      let [x, y] = menu.cursor;
      if ui
        .button(format!("Copy cursor  {}, {}", sig4(x), sig4(y)))
        .clicked()
      {
        copied = Some(format!("{x},{y}"));
        ui.close();
      }
    });
  copied
}

/// Hover text of a point: its series, then its values before the log axes, 10^v on a log axis.
fn hover_text(pos: &HoverPosition<'_>, logs: [bool; 2]) -> String {
  let value = |v: f64, log: bool| sig4(unlog(v, log));
  match pos {
    HoverPosition::NearDataPoint {
      plot_name,
      position,
      ..
    } => format!(
      "{plot_name}\nx = {}\ny = {}",
      value(position.x, logs[0]),
      value(position.y, logs[1])
    ),
    HoverPosition::Elsewhere { position } => format!(
      "x = {}\ny = {}",
      value(position.x, logs[0]),
      value(position.y, logs[1])
    ),
  }
}

/// `v` with at most four significant digits: plain from 1e-4 to below 1e4, else scientific.
fn sig4(v: f64) -> String {
  if !v.is_finite() || v == 0.0 {
    return v.to_string();
  }
  let magnitude = v.abs().log10().floor() as i32;
  if (-4..4).contains(&magnitude) {
    let decimals = (3 - magnitude) as usize;
    trim_fraction(&format!("{v:.decimals$}")).to_string()
  } else {
    let scientific = format!("{v:.3e}");
    match scientific.split_once('e') {
      Some((mantissa, exponent)) => format!("{}e{exponent}", trim_fraction(mantissa)),
      None => scientific,
    }
  }
}

/// Drops the trailing zeros of a decimal fraction, and the point too if nothing follows it.
fn trim_fraction(s: &str) -> &str {
  if s.contains('.') {
    s.trim_end_matches('0').trim_end_matches('.')
  } else {
    s
  }
}

impl PlotApp {
  fn new(name: &str, state: Arc<Mutex<PlotState>>) -> Self {
    let max_points = lock(&state).max_points;
    PlotApp {
      name: name.to_string(),
      state,
      point_radius: 2.0,
      line_width: 1.5,
      legend: true,
      grid: true,
      max_points,
      fit_request: false,
      follow_request: None,
      // egui_plot starts with auto-bounds on.
      follow: true,
      applied_title: None,
      decimated: None,
      settings_open: true,
      bin_rule: BinRule::default(),
      hist_layout: HistLayout::Side,
      hist_cache: None,
      log: [false; 3],
      log_cache: None,
      dropped: 0,
      view3d: View3d::default(),
      copy_menu: None,
      copy_note: None,
    }
  }

  /// Draws the window: the settings panel, then the plot and the note of a copy.
  fn draw(&mut self, ui: &mut egui::Ui) {
    self.side_panel(ui);
    self.plot(ui);
    self.show_copy_note(ui.ctx());
  }

  /// The side panel: the settings while it is open, else a narrow strip with a button.
  fn side_panel(&mut self, ui: &mut egui::Ui) -> PanelRects {
    // The panels borrow the open flag while they animate, so the buttons only record a request.
    let mut open = self.settings_open;
    let mut request = None;
    let mut button = egui::Rect::NOTHING;
    let settings = egui::Panel::left("settings")
      .resizable(true)
      .default_size(SETTINGS_DEFAULT)
      .size_range(SETTINGS_MIN..=SETTINGS_MAX);
    let hidden = egui::Panel::left("settings_hidden")
      .resizable(true)
      .exact_size(SETTINGS_HIDDEN)
      .frame(egui::Frame::side_top_panel(ui.style()).inner_margin(egui::Margin::symmetric(4, 2)));
    let shown = egui::Panel::show_switched(ui, &mut open, hidden, settings, |ui, expanded| {
      button = if expanded {
        self.settings_contents(ui, &mut request)
      } else {
        toggle_button(ui, SHOW_GLYPH, "Show settings", &mut request, true)
      };
      ui.min_rect()
    });
    if let Some(on) = request {
      open = on;
    }
    self.settings_open = open;
    PanelRects {
      outer: shown.response.rect,
      content: shown.inner,
      button,
    }
  }

  /// The expanded settings: a header with the name and a hide button, then the groups
  /// and the status line. Returns the rect of the hide button.
  fn settings_contents(&mut self, ui: &mut egui::Ui, request: &mut Option<bool>) -> egui::Rect {
    let mut button = egui::Rect::NOTHING;
    egui::Sides::new().shrink_left().truncate().show(
      ui,
      |ui| {
        ui.add(egui::Label::new(
          egui::RichText::new(self.name.as_str()).heading(),
        ));
      },
      |ui| button = toggle_button(ui, HIDE_GLYPH, "Hide settings", request, false),
    );
    ui.add_space(4.0);
    // The groups scroll when the window is too short for them; the header stays.
    egui::ScrollArea::vertical()
      .auto_shrink([false, true])
      .show(ui, |ui| self.settings_groups(ui));
    button
  }

  /// The settings groups and the status line. Which groups show depends on the kind of plot.
  fn settings_groups(&mut self, ui: &mut egui::Ui) {
    let kind = self.kind_group(ui);
    self.scale_group(ui, kind);
    let bins = if kind == Kind::Hist {
      self.hist_bins()
    } else {
      0
    };
    match kind {
      Kind::Xy => {
        self.all_series_group(ui);
        self.series_group(ui);
      }
      Kind::Hist => self.histogram_group(ui, bins),
      Kind::ThreeD => {
        self.all_series_group(ui);
        self.series_group(ui);
        self.three_d_group(ui);
      }
    }
    ui.checkbox(&mut self.legend, "Legend");
    if kind != Kind::ThreeD {
      ui.checkbox(&mut self.grid, "Grid");
      self.view_group(ui);
    }

    ui.group(|ui| {
      ui.label(egui::RichText::new("Buffer").strong());
      ui.label("max points / series");
      let response = ui.add(
        egui::DragValue::new(&mut self.max_points)
          .range(0..=usize::MAX)
          .custom_formatter(|v, _| {
            if v == 0.0 {
              "unlimited".to_string()
            } else {
              format!("{v:.0}")
            }
          }),
      );
      if response.changed() {
        lock(&self.state).set_max_points(self.max_points);
      }
    });

    ui.group(|ui| {
      ui.label(egui::RichText::new("Data").strong());
      ui.horizontal_wrapped(|ui| {
        if ui.button("Clear").clicked() {
          lock(&self.state).clear();
        }
        if ui.button("Copy CSV").clicked() {
          let csv = lock(&self.state).to_csv();
          ui.ctx().copy_text(csv);
        }
      });
    });

    let (series, points, skipped) = {
      let st = lock(&self.state);
      (st.series.len(), st.total_points(), st.skipped)
    };
    let mut status = if skipped > 0 {
      format!("{series} series · {points} points · {skipped} skipped")
    } else {
      format!("{series} series · {points} points")
    };
    if let Some((drawn, total)) = self.decimated {
      status = format!("{status} · drawing {drawn} of {total}");
    }
    if kind == Kind::Hist {
      status = format!("{status} · {bins} bins");
    }
    if self.dropped > 0 {
      status = format!("{status} · {} ≤ 0 hidden", self.dropped);
    }
    ui.label(status);
  }

  /// The "Kind" group: what the plot shows. 3D needs z values in every series. Returns the
  /// kind after this frame's choice.
  fn kind_group(&mut self, ui: &mut egui::Ui) -> Kind {
    let (kind, all_z) = {
      let st = lock(&self.state);
      (st.kind, st.all_have_z())
    };
    let mut chosen = kind;
    ui.group(|ui| {
      ui.label(egui::RichText::new("Kind").strong());
      ui.radio_value(&mut chosen, Kind::Xy, "x–y");
      ui.radio_value(&mut chosen, Kind::Hist, "Histogram");
      let three_d = ui
        .add_enabled(all_z, egui::RadioButton::new(chosen == Kind::ThreeD, "3D"))
        .on_disabled_hover_text("every series needs z values");
      if three_d.clicked() && all_z {
        chosen = Kind::ThreeD;
      }
    });
    if chosen != kind {
      let mut st = lock(&self.state);
      st.kind = chosen;
      st.version += 1;
    }
    chosen
  }

  /// The "Scale" group: which axes are logarithmic, and z in 3D only. Toggling an axis refits the
  /// view, whose bounds are in the other scale.
  fn scale_group(&mut self, ui: &mut egui::Ui, kind: Kind) {
    let before = self.log;
    ui.group(|ui| {
      ui.label(egui::RichText::new("Scale").strong());
      if kind == Kind::Hist {
        ui.checkbox(&mut self.log[0], "log x (values)");
        ui.checkbox(&mut self.log[1], "log y (counts)");
      } else {
        ui.checkbox(&mut self.log[0], "log x");
        ui.checkbox(&mut self.log[1], "log y");
      }
      if kind == Kind::ThreeD {
        ui.checkbox(&mut self.log[2], "log z");
      }
    });
    if self.log != before {
      self.fit_request = true;
    }
  }

  /// The "All series" group: the geometry of every series without its own, and the marker
  /// and line sizes where they apply.
  fn all_series_group(&mut self, ui: &mut egui::Ui) {
    let (default, points, lines) = {
      let st = lock(&self.state);
      (
        st.default_geom,
        any_drawn(&st, has_points),
        any_drawn(&st, has_lines),
      )
    };
    let mut chosen = None;
    ui.group(|ui| {
      ui.label(egui::RichText::new("All series").strong());
      for geom in GEOMS {
        if ui.radio(default == geom, geom_name(geom)).clicked() {
          chosen = Some(geom);
        }
      }
      ui.add_enabled_ui(points, |ui| {
        slider(ui, "point size", &mut self.point_radius, 0.5..=10.0);
      });
      ui.add_enabled_ui(lines, |ui| {
        slider(ui, "line width", &mut self.line_width, 0.5..=8.0);
      });
    });
    // Choosing one applies it to every series, so the series' own choices are dropped.
    if let Some(geom) = chosen {
      let mut st = lock(&self.state);
      st.default_geom = geom;
      st.geoms.clear();
      st.version += 1;
    }
  }

  /// The "Series" group: a row per series, with its colour, its name and its geometry.
  fn series_group(&mut self, ui: &mut egui::Ui) {
    let rows: Vec<(String, Geom)> = {
      let st = lock(&self.state);
      st.series
        .iter()
        .map(|s| (s.name.clone(), st.geom_of(&s.name)))
        .collect()
    };
    ui.group(|ui| {
      ui.label(egui::RichText::new("Series").strong());
      if rows.is_empty() {
        ui.label(egui::RichText::new("none yet").weak());
      }
      for (i, (name, geom)) in rows.iter().enumerate() {
        let mut chosen = *geom;
        egui::Sides::new().shrink_left().truncate().show(
          ui,
          |ui| {
            let (swatch, _) =
              ui.allocate_exact_size(egui::Vec2::splat(SWATCH), egui::Sense::hover());
            ui.painter()
              .rect_filled(swatch, egui::CornerRadius::same(2), palette(i));
            ui.add(egui::Label::new(name.as_str()).truncate());
          },
          |ui| {
            egui::ComboBox::from_id_salt(("series geometry", name))
              .width(GEOM_BOX)
              .selected_text(geom_name(chosen))
              .show_ui(ui, |ui| {
                for geom in GEOMS {
                  ui.selectable_value(&mut chosen, geom, geom_name(geom));
                }
              });
          },
        );
        if chosen != *geom {
          let mut st = lock(&self.state);
          st.geoms.insert(name.clone(), chosen);
          st.version += 1;
        }
      }
    });
  }

  /// The "Histogram" group: the bin rule, the bar layout and the number of bins.
  fn histogram_group(&mut self, ui: &mut egui::Ui, bins: usize) {
    ui.group(|ui| {
      ui.label(egui::RichText::new("Histogram").strong());
      ui.label(egui::RichText::new("Bins").small().weak());
      ui.radio_value(&mut self.bin_rule, BinRule::Sturges, "Sturges");
      ui.radio_value(&mut self.bin_rule, BinRule::Scott, "Scott");
      ui.label(egui::RichText::new("Bars").small().weak());
      ui.radio_value(&mut self.hist_layout, HistLayout::Side, "side by side");
      ui.radio_value(&mut self.hist_layout, HistLayout::Stack, "stacked");
      ui.label(format!("{bins} bins"));
    });
  }

  /// The "3D" group: the projection and the camera reset, with the mouse controls as a hint.
  fn three_d_group(&mut self, ui: &mut egui::Ui) {
    ui.group(|ui| {
      ui.label(egui::RichText::new("3D").strong());
      ui.checkbox(&mut self.view3d.perspective, "Perspective");
      if ui.button("Reset view").clicked() {
        self.view3d.reset_camera();
      }
      ui.label(
        egui::RichText::new("drag: rotate · scroll: zoom · double-click: reset")
          .small()
          .weak(),
      );
    });
  }

  /// The "View" group: follow the data or fit the axes to it, and how to pan and zoom.
  fn view_group(&mut self, ui: &mut egui::Ui) {
    ui.group(|ui| {
      ui.label(egui::RichText::new("View").strong());
      let mut follow = self.follow;
      if ui.checkbox(&mut follow, "Follow").changed() {
        self.follow = follow;
        self.follow_request = Some(follow);
      }
      if ui.button("Fit").clicked() {
        self.fit_request = true;
      }
      ui.label(
        egui::RichText::new(
          "drag/scroll: pan · ⌘/Ctrl+scroll: zoom · right-drag: box zoom · double-click: fit",
        )
        .small()
        .weak(),
      );
    });
  }

  /// Number of bins of the histogram of the data, 0 with no values. Refreshes the caches.
  fn hist_bins(&mut self) -> usize {
    let state = Arc::clone(&self.state);
    let st = lock_contiguous(&state);
    // The values are the y of the data, so the mapping of the histogram is on y.
    let (series, _, _) = mapped_series(&mut self.log_cache, &st, [false, self.log[0]]);
    cached_histogram(
      &mut self.hist_cache,
      self.bin_rule,
      self.log[0],
      &st,
      series,
    )
    .map_or(0, Histogram::bins)
  }

  /// The central view, of the plot's kind.
  fn plot(&mut self, ui: &mut egui::Ui) {
    // One lock for the whole frame; network threads hold it only briefly.
    let state = Arc::clone(&self.state);
    let st = lock_contiguous(&state);
    // A request from the panel is applied inside the plot, which then reports the follow state.
    let fit = std::mem::take(&mut self.fit_request);
    let toggle = self.follow_request.take();
    let request = if fit { Some(true) } else { toggle };
    match st.kind {
      Kind::Xy => self.xy_plot(ui, &st, request),
      Kind::Hist => self.hist_plot(ui, &st, request),
      Kind::ThreeD => self.three_d_plot(ui, &st),
    }
  }

  /// The x–y plot, each series with its own geometry. On a log axis the values are drawn in log10.
  fn xy_plot(&mut self, ui: &mut egui::Ui, st: &PlotState, request: Option<bool>) {
    let (radius, width) = (self.point_radius, self.line_width);
    let (legend, grid) = (self.legend, self.grid);
    let [log_x, log_y, _] = self.log;
    let (series, starts, dropped) = mapped_series(&mut self.log_cache, st, [log_x, log_y]);
    let name = &self.name;
    let total = st.total_points();
    let mut follow: Option<bool> = None;
    let mut decimated: Option<(usize, usize)> = None;
    let mut plot_response = None;
    let mut frozen = None;
    // The copy menu shows the values that the hover label would, so the label waits for it.
    let menu_open = egui::Popup::is_id_open(ui.ctx(), copy_menu_id());

    egui::CentralPanel::default().show(ui, |ui| {
      // The data point the hover snaps to in this frame: the formatter runs only while hovering.
      let snapped = RefCell::new(None);
      let snapped_ref = &snapped;
      let mut plot = Plot::new(XY_PLOT_ID)
        .id(egui::Id::new(XY_PLOT_ID))
        .show_grid(grid)
        .label_formatter(move |pos| {
          *snapped_ref.borrow_mut() = match pos {
            HoverPosition::NearDataPoint {
              plot_name,
              position,
              ..
            } => Some((plot_name.to_string(), [position.x, position.y])),
            HoverPosition::Elsewhere { .. } => None,
          };
          (!menu_open).then(|| hover_text(pos, [log_x, log_y]))
        });
      if legend {
        plot = plot.legend(Legend::default());
      }
      if log_x {
        plot = plot
          .x_grid_spacer(log_grid)
          .x_axis_formatter(log_axis_label);
      }
      if log_y {
        plot = plot
          .y_grid_spacer(log_grid)
          .y_axis_formatter(log_axis_label);
      }
      let x_title = axis_title(&st.x_label, log_x);
      if !x_title.is_empty() {
        plot = plot.x_axis_label(x_title);
      }
      let y_title = axis_title(&st.y_label, log_y);
      if !y_title.is_empty() {
        plot = plot.y_axis_label(y_title);
      }

      let response = plot.show(ui, |plot_ui| {
        // A request from the panel is applied first, so this frame already
        // decimates for the new view.
        let auto = apply_follow(plot_ui, request);
        let view = plot_ui.plot_bounds();
        let (min, max) = (view.min(), view.max());
        // One line bucket per physical pixel column; one point per cell of
        // `radius` points, about the area a marker covers.
        let ppp = plot_ui.ctx().pixels_per_point();
        let frame = plot_ui.transform().frame();
        let columns = (frame.width() * ppp).round().max(1.0) as usize;
        let cell = radius.max(1.0);
        let cols = (frame.width() / cell).round().max(1.0) as usize;
        let rows = (frame.height() / cell).round().max(1.0) as usize;

        // A large unsorted series is drawn as points only: its line can only
        // be decimated in x order, and in full it is an unreadable tangle.
        let big = |s: &Series| s.points.len() > LOD_MIN;
        let as_line = |s: &Series| has_lines(st.geom_of(&s.name)) && (s.sorted || !big(s));
        let as_points = |s: &Series| has_points(st.geom_of(&s.name)) || !as_line(s);

        // Per series, the marks drawn for it (line points or markers, whichever more).
        let mut marks = vec![0; series.len()];
        let mut any_decimated = false;
        // Lines first, so that the points of every series are drawn on top.
        // A line and the points of a series share a name and an id (the
        // default), so the legend shows one entry for both.
        // On a log axis, points with no place on it split a line into runs, drawn as separate
        // lines (same name, so still one legend entry) so that no line bridges the gap.
        for (i, s) in series.iter().enumerate().filter(|(_, s)| as_line(s)) {
          let pts = s.points.as_slices().0;
          let range = big(s).then(|| decimation_range(pts, s.sorted, auto, min, max));
          for run in runs(pts, starts.get(i).map_or(&[], Vec::as_slice)) {
            let line = match range {
              Some([x0, x1, _, _]) => {
                let kept = decimate_line(run, x0, x1, columns);
                any_decimated = true;
                marks[i] += kept.len();
                Line::new(s.name.as_str(), PlotPoints::Owned(kept))
              }
              None => {
                marks[i] += run.len();
                Line::new(s.name.as_str(), PlotPoints::Borrowed(run))
              }
            };
            plot_ui.line(line.color(palette(i)).width(width));
          }
        }
        for (i, s) in series.iter().enumerate().filter(|(_, s)| as_points(s)) {
          let pts = s.points.as_slices().0;
          let points = if big(s) {
            let [x0, x1, y0, y1] = decimation_range(pts, s.sorted, auto, min, max);
            let kept = decimate_points(pts, x0, x1, y0, y1, cols, rows);
            any_decimated = true;
            marks[i] = marks[i].max(kept.len());
            PlotPoints::Owned(kept)
          } else {
            marks[i] = marks[i].max(pts.len());
            PlotPoints::Borrowed(pts)
          };
          let points = Points::new(s.name.as_str(), points)
            .color(palette(i))
            .radius(radius);
          plot_ui.points(points);
        }
        let drawn: usize = marks.iter().sum();
        follow = Some(auto.any());
        decimated = any_decimated.then_some((drawn, total));
      });

      if total == 0 {
        paint_waiting(ui, response.response.rect, name);
      }
      if response.response.secondary_clicked() {
        frozen = copy_snapshot(&response, st, snapped.take(), [log_x, log_y]);
      }
      plot_response = Some(response.response);
    });

    if let Some(response) = &plot_response {
      self.update_copy_menu(response, frozen);
    }
    if let Some(on) = follow {
      self.follow = on;
    }
    self.set_decimated(ui, decimated);
    self.set_dropped(ui, dropped);
  }

  /// The histogram: one bar chart per series, over bins shared by all of them. Log values are
  /// binned in log10, and log counts are drawn as log10 heights.
  fn hist_plot(&mut self, ui: &mut egui::Ui, st: &PlotState, request: Option<bool>) {
    let [log_x, log_y, _] = self.log;
    let layout = self.hist_layout;
    // The values are the y of the data, so the mapping of the histogram is on y.
    let (series, _, dropped) = mapped_series(&mut self.log_cache, st, [false, log_x]);
    let charts = match cached_histogram(&mut self.hist_cache, self.bin_rule, log_x, st, series) {
      Some(h) => bar_charts(h, layout, &st.series, log_y),
      None => Vec::new(),
    };
    let (legend, grid) = (self.legend, self.grid);
    let name = &self.name;
    let total = st.total_points();
    let x_label = if st.y_label.is_empty() {
      "value"
    } else {
      st.y_label.as_str()
    };
    let x_title = axis_title(x_label, log_x);
    let y_title = axis_title("count", log_y);
    let mut follow: Option<bool> = None;
    let mut plot_response = None;
    let mut frozen = None;
    let menu_open = egui::Popup::is_id_open(ui.ctx(), copy_menu_id());

    egui::CentralPanel::default().show(ui, |ui| {
      let mut plot = Plot::new(HIST_PLOT_ID)
        .id(egui::Id::new(HIST_PLOT_ID))
        .show_grid(grid)
        .x_axis_label(x_title)
        .y_axis_label(y_title)
        .label_formatter(move |pos| (!menu_open).then(|| hover_text(pos, [log_x, log_y])));
      if log_x {
        plot = plot
          .x_grid_spacer(log_grid)
          .x_axis_formatter(log_axis_label);
      }
      if log_y {
        plot = plot
          .y_grid_spacer(log_grid)
          .y_axis_formatter(log_axis_label);
      }
      if legend {
        plot = plot.legend(Legend::default());
      }
      let response = plot.show(ui, |plot_ui| {
        follow = Some(apply_follow(plot_ui, request).any());
        for chart in charts {
          plot_ui.bar_chart(chart);
        }
      });

      if total == 0 {
        paint_waiting(ui, response.response.rect, name);
      }
      if response.response.secondary_clicked() {
        frozen = copy_snapshot(&response, st, None, [log_x, log_y]);
      }
      plot_response = Some(response.response);
    });

    if let Some(response) = &plot_response {
      self.update_copy_menu(response, frozen);
    }
    if let Some(on) = follow {
      self.follow = on;
    }
    self.set_decimated(ui, None);
    self.set_dropped(ui, dropped);
  }

  /// The 3D view of the series that have z values. Each series is drawn with its geometry.
  fn three_d_plot(&mut self, ui: &mut egui::Ui, st: &PlotState) {
    // The copy menu is for the x–y and histogram plots, so it closes here.
    egui::Popup::close_id(ui.ctx(), copy_menu_id());
    self.view3d.point_radius = self.point_radius;
    self.view3d.line_width = self.line_width;
    self.view3d.legend = self.legend;
    self.view3d.log = self.log;
    let series: Vec<Series3d<'_>> = st
      .series
      .iter()
      .enumerate()
      .filter_map(|(i, s)| {
        let z = s.z.as_ref()?.as_slices().0;
        let geom = st.geom_of(&s.name);
        Some(Series3d {
          name: s.name.as_str(),
          xy: s.points.as_slices().0,
          z,
          color: palette(i),
          lines: has_lines(geom),
          points: has_points(geom),
        })
      })
      .collect();
    let labels = [
      st.x_label.as_str(),
      st.y_label.as_str(),
      st.z_label.as_str(),
    ];
    let empty = st.total_points() == 0;
    let mut decimated: Option<(usize, usize)> = None;
    let mut dropped = 0;

    egui::CentralPanel::default().show(ui, |ui| {
      let drawn = self.view3d.show(ui, &series, labels);
      if drawn.marks < drawn.total {
        decimated = Some((drawn.marks, drawn.total));
      }
      dropped = drawn.dropped;
      if empty {
        paint_waiting(ui, drawn.response.rect, &self.name);
      }
    });
    self.set_decimated(ui, decimated);
    self.set_dropped(ui, dropped);
  }

  /// Stores the decimation status for the settings of the next frame. The settings of this frame
  /// are already drawn, so a changed status asks for another frame.
  fn set_decimated(&mut self, ui: &egui::Ui, decimated: Option<(usize, usize)>) {
    if self.decimated != decimated {
      ui.ctx().request_repaint();
    }
    self.decimated = decimated;
  }

  /// Stores the points left out for a log axis, for the status line of the next frame. As with
  /// [`Self::set_decimated`], a changed count asks for another frame.
  fn set_dropped(&mut self, ui: &egui::Ui, dropped: usize) {
    if self.dropped != dropped {
      ui.ctx().request_repaint();
    }
    self.dropped = dropped;
  }

  /// Freezes the copy menu on the right click that gave `frozen`, then shows it over the plot
  /// `response` while it is open. A copy puts its text on the clipboard and starts the note.
  fn update_copy_menu(&mut self, response: &egui::Response, frozen: Option<CopyMenu>) {
    if let Some(menu) = frozen {
      self.copy_menu = Some(menu);
    }
    let Some(menu) = &self.copy_menu else {
      return;
    };
    if let Some(text) = copy_menu_items(response, menu) {
      self.copy_note = Some(CopyNote {
        at: menu.at,
        text: format!("Copied {text}"),
        since: response.ctx.input(|i| i.time),
      });
      response.ctx.copy_text(text);
    }
  }

  /// Shows the note of the last copy by the menu, until [`COPY_NOTE_SECS`] have passed. While it
  /// shows, a repaint is requested for the moment it goes.
  fn show_copy_note(&mut self, ctx: &egui::Context) {
    let Some(note) = &self.copy_note else {
      return;
    };
    let left = COPY_NOTE_SECS - (ctx.input(|i| i.time) - note.since);
    if left <= 0.0 {
      self.copy_note = None;
      return;
    }
    ctx.request_repaint_after(Duration::from_secs_f64(left));
    let style = ctx.global_style();
    egui::Area::new(egui::Id::new("complot_copy_note"))
      .order(egui::Order::Tooltip)
      .pivot(egui::Align2::LEFT_BOTTOM)
      .fixed_pos(note.at + egui::vec2(4.0, -4.0))
      .interactable(false)
      .show(ctx, |ui| {
        egui::Frame::popup(&style).show(ui, |ui| ui.label(note.text.as_str()));
      });
  }
}

impl eframe::App for PlotApp {
  fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
    let st = lock(&self.state);
    if st.close_requested {
      ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
    if let Some(title) = &st.title
      && self.applied_title.as_ref() != Some(title)
    {
      ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("complot — {title}")));
      self.applied_title = Some(title.clone());
    }
  }

  fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
    self.draw(ui);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::model::PlotType;
  use egui_plot::{PlotMemory, PlotTransform};

  /// Time between frames, as at 60 Hz. Animations run on this clock.
  const FRAME_TIME: f64 = 1.0 / 60.0;

  /// Size of the screen the panel is drawn in.
  const SCREEN: egui::Vec2 = egui::vec2(900.0, 560.0);

  /// A screen tall enough for the status line, which lies below the settings groups at the
  /// default height.
  const TALL: egui::Vec2 = egui::vec2(900.0, 760.0);

  /// Too long for the header, which must be shortened rather than overflow.
  const LONG_NAME: &str = "a_plot_name_much_too_long_for_the_settings_header";

  /// Tolerance of the layout checks.
  const EPS: f64 = 1e-9;

  /// A context and a plot with some data, so that the status line has content.
  fn setup(name: &str) -> (egui::Context, PlotApp) {
    let state = Arc::new(Mutex::new(PlotState::new()));
    {
      let mut st = lock(&state);
      for i in 0..50 {
        st.push_point("sine", None, (i as f64 * 0.1).sin(), None)
          .unwrap();
      }
    }
    (egui::Context::default(), PlotApp::new(name, state))
  }

  /// A context and a plot named `t`, with `fill` applied to its state first.
  fn app_with(fill: impl FnOnce(&mut PlotState)) -> (egui::Context, PlotApp) {
    let state = Arc::new(Mutex::new(PlotState::new()));
    fill(&mut lock(&state));
    (egui::Context::default(), PlotApp::new("t", state))
  }

  /// Raw input for one frame, on a screen of `screen` points, with `events`.
  fn raw_input(
    ctx: &egui::Context,
    screen: egui::Vec2,
    events: Vec<egui::Event>,
  ) -> egui::RawInput {
    egui::RawInput {
      screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, screen)),
      time: Some(ctx.input(|i| i.time) + FRAME_TIME),
      predicted_dt: FRAME_TIME as f32,
      events,
      ..Default::default()
    }
  }

  /// Runs one frame that draws only the settings panel, with `events` as input. Returns the
  /// rects of the panel and the output of the frame.
  fn panel_frame(
    ctx: &egui::Context,
    app: &mut PlotApp,
    events: Vec<egui::Event>,
    screen: egui::Vec2,
  ) -> (PanelRects, egui::FullOutput) {
    let mut rects = None;
    let mut output = ctx.run_ui(raw_input(ctx, screen, events), |ui| {
      rects = Some(app.side_panel(ui));
    });
    // No renderer here, so the texture uploads are dropped unused.
    output.textures_delta.clear();
    (rects.expect("the frame drew the panel"), output)
  }

  /// Runs one frame that draws only the settings panel, with `events` as input.
  fn frame(ctx: &egui::Context, app: &mut PlotApp, events: Vec<egui::Event>) -> PanelRects {
    frame_in(ctx, app, events, SCREEN)
  }

  /// Like [`frame`], on a screen of the given size.
  fn frame_in(
    ctx: &egui::Context,
    app: &mut PlotApp,
    events: Vec<egui::Event>,
    screen: egui::Vec2,
  ) -> PanelRects {
    panel_frame(ctx, app, events, screen).0
  }

  /// Runs `frames` frames without input, so that layout and animations settle.
  fn settle(ctx: &egui::Context, app: &mut PlotApp, frames: usize) -> PanelRects {
    let mut rects = frame(ctx, app, Vec::new());
    for _ in 1..frames {
      rects = frame(ctx, app, Vec::new());
    }
    rects
  }

  /// Runs five frames without input and returns the output of the last, for its layout.
  fn settled_output(ctx: &egui::Context, app: &mut PlotApp) -> egui::FullOutput {
    let mut output = panel_frame(ctx, app, Vec::new(), SCREEN).1;
    for _ in 1..5 {
      output = panel_frame(ctx, app, Vec::new(), SCREEN).1;
    }
    output
  }

  /// Runs one frame of the whole window, plot included, with `events` as input.
  fn whole_frame(
    ctx: &egui::Context,
    app: &mut PlotApp,
    events: Vec<egui::Event>,
  ) -> egui::FullOutput {
    whole_frame_in(ctx, app, events, SCREEN)
  }

  /// Like [`whole_frame`], on a screen of the given size.
  fn whole_frame_in(
    ctx: &egui::Context,
    app: &mut PlotApp,
    events: Vec<egui::Event>,
    screen: egui::Vec2,
  ) -> egui::FullOutput {
    let mut output = ctx.run_ui(raw_input(ctx, screen, events), |ui| app.draw(ui));
    // No renderer here, so the texture uploads are dropped unused.
    output.textures_delta.clear();
    output
  }

  /// Every text of `output` with its bounding rect, in paint order.
  fn texts(output: &egui::FullOutput) -> Vec<(String, egui::Rect)> {
    fn collect(shape: &egui::Shape, out: &mut Vec<(String, egui::Rect)>) {
      match shape {
        egui::Shape::Text(t) => out.push((t.galley.text().to_string(), t.visual_bounding_rect())),
        egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| collect(s, out)),
        _ => {}
      }
    }
    let mut out = Vec::new();
    for clipped in &output.shapes {
      collect(&clipped.shape, &mut out);
    }
    out
  }

  /// Centre of the first text of `output` that reads `text`.
  fn text_at(output: &egui::FullOutput, text: &str) -> egui::Pos2 {
    texts(output)
      .into_iter()
      .find(|(t, _)| t == text)
      .unwrap_or_else(|| panic!("no text '{text}' on screen"))
      .1
      .center()
  }

  /// The texts of `output` that start with `prefix`.
  fn texts_starting(output: &egui::FullOutput, prefix: &str) -> Vec<String> {
    texts(output)
      .into_iter()
      .map(|(t, _)| t)
      .filter(|t| t.starts_with(prefix))
      .collect()
  }

  /// Centre of the first text of `output` that starts with `prefix`.
  fn text_starting_at(output: &egui::FullOutput, prefix: &str) -> egui::Pos2 {
    texts(output)
      .into_iter()
      .find(|(t, _)| t.starts_with(prefix))
      .unwrap_or_else(|| panic!("no text starting '{prefix}' on screen"))
      .1
      .center()
  }

  /// The texts put on the clipboard by `output`.
  fn copied(output: &egui::FullOutput) -> Vec<String> {
    output
      .platform_output
      .commands
      .iter()
      .filter_map(|c| match c {
        egui::OutputCommand::CopyText(text) => Some(text.clone()),
        _ => None,
      })
      .collect()
  }

  /// The two numbers of a copied "x,y".
  fn parse_pair(text: &str) -> (f64, f64) {
    let (x, y) = text
      .split_once(',')
      .unwrap_or_else(|| panic!("'{text}' is not x,y"));
    (
      x.parse()
        .unwrap_or_else(|_| panic!("'{x}' is not a number")),
      y.parse()
        .unwrap_or_else(|_| panic!("'{y}' is not a number")),
    )
  }

  /// The transform of the plot `id`, as its last frame drew it.
  fn plot_transform(ctx: &egui::Context, id: &str) -> PlotTransform {
    PlotMemory::load(ctx, egui::Id::new(id))
      .expect("the plot was drawn")
      .transform()
  }

  /// Runs `frames` frames of the whole window without input, and returns the output of the last.
  fn whole_settled(ctx: &egui::Context, app: &mut PlotApp, frames: usize) -> egui::FullOutput {
    let mut output = whole_frame(ctx, app, Vec::new());
    for _ in 1..frames {
      output = whole_frame(ctx, app, Vec::new());
    }
    output
  }

  /// Clicks `button` at `pos` on the whole window: hovers there, then presses and releases.
  /// Returns the output of the frame of the release, where the click registers.
  fn click_in(
    ctx: &egui::Context,
    app: &mut PlotApp,
    pos: egui::Pos2,
    button: egui::PointerButton,
  ) -> egui::FullOutput {
    whole_frame(ctx, app, vec![pointer_moved(pos)]);
    whole_frame(
      ctx,
      app,
      vec![pointer_moved(pos), button_event(button, pos, true)],
    );
    whole_frame(ctx, app, vec![button_event(button, pos, false)])
  }

  /// Runs one frame of the whole window at 2 pixels per point, with `events`. Its textures go to
  /// `renderer`, so that a later frame can be rendered; rendering is slow, so frames are not.
  fn frame_at_2x(
    ctx: &egui::Context,
    renderer: &mut crate::snapshot::Renderer,
    app: &mut PlotApp,
    events: Vec<egui::Event>,
  ) -> egui::FullOutput {
    let viewport = egui::ViewportInfo {
      native_pixels_per_point: Some(2.0),
      ..Default::default()
    };
    let input = egui::RawInput {
      viewports: std::iter::once((egui::ViewportId::ROOT, viewport)).collect(),
      screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SCREEN)),
      time: Some(ctx.input(|i| i.time) + FRAME_TIME),
      predicted_dt: FRAME_TIME as f32,
      events,
      ..Default::default()
    };
    let mut output = ctx.run_ui(input, |ui| app.draw(ui));
    renderer.update(&output.textures_delta);
    output.textures_delta.clear();
    output
  }

  /// A context and an x–y plot of series "a", with (0.1, 0.987) among its points, drawn for a
  /// few frames so that its transform is known.
  fn snapped_app() -> (egui::Context, PlotApp) {
    let (ctx, mut app) = app_with(|st| {
      for (x, y) in [(0.0, 0.0), (0.1, 0.987), (0.5, 0.3), (1.0, 1.0)] {
        st.push_point("a", Some(x), y, None).unwrap();
      }
    });
    whole_settled(&ctx, &mut app, 3);
    (ctx, app)
  }

  fn pointer_moved(pos: egui::Pos2) -> egui::Event {
    egui::Event::PointerMoved(pos)
  }

  /// A press or a release of `button` at `pos`.
  fn button_event(button: egui::PointerButton, pos: egui::Pos2, pressed: bool) -> egui::Event {
    egui::Event::PointerButton {
      pos,
      button,
      pressed,
      modifiers: egui::Modifiers::NONE,
    }
  }

  /// A press or a release of the primary button at `pos`.
  fn pointer_button(pos: egui::Pos2, pressed: bool) -> egui::Event {
    button_event(egui::PointerButton::Primary, pos, pressed)
  }

  /// Clicks at `pos`. The pointer hovers there first, so the widget under it is known.
  fn click(ctx: &egui::Context, app: &mut PlotApp, pos: egui::Pos2) {
    frame(ctx, app, vec![pointer_moved(pos)]);
    frame(
      ctx,
      app,
      vec![pointer_moved(pos), pointer_button(pos, true)],
    );
    frame(ctx, app, vec![pointer_button(pos, false)]);
  }

  /// Drags the right edge of the panel shown by `from` to `x`, in steps, then releases it.
  fn drag_edge(ctx: &egui::Context, app: &mut PlotApp, from: PanelRects, x: f32) -> PanelRects {
    let y = from.outer.center().y;
    let start = egui::pos2(from.outer.max.x, y);
    let end = egui::pos2(x, y);
    frame(ctx, app, vec![pointer_moved(start)]);
    frame(
      ctx,
      app,
      vec![pointer_moved(start), pointer_button(start, true)],
    );
    const STEPS: usize = 10;
    for i in 1..=STEPS {
      let t = i as f32 / STEPS as f32;
      let pos = egui::pos2(start.x + (end.x - start.x) * t, y);
      frame(ctx, app, vec![pointer_moved(pos)]);
    }
    frame(ctx, app, vec![pointer_button(end, false)]);
    settle(ctx, app, 30)
  }

  /// Asserts that the content and the button lie inside the panel, up to half a pixel.
  fn assert_fits(r: PanelRects) {
    let outer = r.outer.expand(0.5);
    assert!(
      outer.contains_rect(r.content),
      "content {:?} overflows panel {:?}",
      r.content,
      r.outer
    );
    assert!(
      outer.contains_rect(r.button),
      "button {:?} overflows panel {:?}",
      r.button,
      r.outer
    );
  }

  /// Asserts that the panel is `width` wide, up to a pixel.
  fn assert_width(r: PanelRects, width: f32) {
    assert!(
      (r.outer.width() - width).abs() <= 1.0,
      "panel is {} wide, expected {width}",
      r.outer.width()
    );
  }

  /// Widest values: the largest slider values, the largest buffer limit and every axis on a
  /// log scale, so that the Scale group is drawn in full.
  fn widest(app: &mut PlotApp) {
    app.point_radius = 10.0;
    app.line_width = 8.0;
    app.max_points = usize::MAX;
    app.log = [true; 3];
  }

  /// Asserts that bars match `want`, each as (argument, width, base, height), up to `EPS`.
  fn assert_bars(got: &[(f64, f64, f64, f64)], want: &[(f64, f64, f64, f64)]) {
    assert_eq!(got.len(), want.len(), "{got:?} is not {want:?}");
    for (g, w) in got.iter().zip(want) {
      let close = [g.0 - w.0, g.1 - w.1, g.2 - w.2, g.3 - w.3]
        .iter()
        .all(|d| d.abs() < EPS);
      assert!(close, "{g:?} is not {w:?}");
    }
  }

  /// Points of the Lorenz attractor as (x, y, z), from RK4 steps of 0.01.
  fn lorenz(n: usize) -> Vec<(f64, f64, f64)> {
    let rhs = |[x, y, z]: [f64; 3]| [10.0 * (y - x), x * (28.0 - z) - y, x * y - 8.0 / 3.0 * z];
    let advance =
      |s: [f64; 3], k: [f64; 3], h: f64| [s[0] + h * k[0], s[1] + h * k[1], s[2] + h * k[2]];
    let h = 0.01;
    let mut s = [1.0, 1.0, 1.0];
    (0..n)
      .map(|_| {
        let k1 = rhs(s);
        let k2 = rhs(advance(s, k1, h / 2.0));
        let k3 = rhs(advance(s, k2, h / 2.0));
        let k4 = rhs(advance(s, k3, h));
        for (i, v) in s.iter_mut().enumerate() {
          *v += h / 6.0 * (k1[i] + 2.0 * k2[i] + 2.0 * k3[i] + k4[i]);
        }
        (s[0], s[1], s[2])
      })
      .collect()
  }

  #[test]
  fn fits_in_a_window_of_minimum_height() {
    let (ctx, mut app) = setup(LONG_NAME);
    let short = egui::vec2(SCREEN.x, 320.0);
    let mut r = frame_in(&ctx, &mut app, Vec::new(), short);
    for _ in 0..5 {
      r = frame_in(&ctx, &mut app, Vec::new(), short);
    }
    assert!(r.outer.height() <= 320.5);
    assert_fits(r);
  }

  #[test]
  fn settings_open_by_default() {
    assert!(setup("t").1.settings_open);
  }

  #[test]
  fn fits_at_default_width() {
    let (ctx, mut app) = setup("t");
    let r = settle(&ctx, &mut app, 5);
    assert_width(r, SETTINGS_DEFAULT);
    assert_fits(r);
  }

  #[test]
  fn drag_below_minimum_stops_at_minimum() {
    let (ctx, mut app) = setup("t");
    let r = settle(&ctx, &mut app, 5);
    let r = drag_edge(&ctx, &mut app, r, 120.0);
    assert_width(r, SETTINGS_MIN);
    assert!(
      app.settings_open,
      "dragging below the minimum must not hide the panel"
    );
    assert_fits(r);
  }

  #[test]
  fn drag_above_minimum_follows_pointer() {
    let (ctx, mut app) = setup("t");
    let r = settle(&ctx, &mut app, 5);
    let r = drag_edge(&ctx, &mut app, r, 240.0);
    assert_width(r, 240.0);
    assert!(app.settings_open);
    assert_fits(r);
  }

  #[test]
  fn widest_values_fit_at_minimum() {
    let (ctx, mut app) = setup("t");
    widest(&mut app);
    let r = settle(&ctx, &mut app, 5);
    let r = drag_edge(&ctx, &mut app, r, SETTINGS_MIN);
    assert_width(r, SETTINGS_MIN);
    assert_fits(r);
  }

  #[test]
  fn fits_at_every_allowed_width() {
    let (ctx, mut app) = setup(LONG_NAME);
    widest(&mut app);
    let mut r = settle(&ctx, &mut app, 5);
    let mut width = SETTINGS_MIN;
    while width <= SETTINGS_MAX {
      r = drag_edge(&ctx, &mut app, r, width);
      assert_width(r, width);
      assert!(app.settings_open);
      assert_fits(r);
      width += 10.0;
    }
  }

  #[test]
  fn panel_fits_in_every_kind_with_a_long_series_name() {
    let kinds = [
      (PlotType::Both, Kind::Xy),
      (PlotType::Hist, Kind::Hist),
      (PlotType::Points3d, Kind::ThreeD),
    ];
    for (plot_type, kind) in kinds {
      let (ctx, mut app) = app_with(|st| {
        for i in 0..20 {
          let t = i as f64;
          st.push_point(LONG_NAME, None, t.sin(), Some(t.cos()))
            .unwrap();
          st.push_point("short", None, t, Some(-t)).unwrap();
        }
        // The widest geometry label.
        st.geoms.insert(LONG_NAME.to_string(), Geom::Points);
        st.set_type(plot_type, None).unwrap();
      });
      assert_eq!(lock(&app.state).kind, kind);
      widest(&mut app);
      let r = settle(&ctx, &mut app, 5);
      let r = drag_edge(&ctx, &mut app, r, SETTINGS_MIN);
      assert_width(r, SETTINGS_MIN);
      assert_fits(r);
    }
  }

  #[test]
  fn hide_and_show_buttons() {
    let (ctx, mut app) = setup("t");
    let shown = settle(&ctx, &mut app, 5);
    click(&ctx, &mut app, shown.button.center());
    let hidden = settle(&ctx, &mut app, 30);
    assert!(!app.settings_open);
    assert!(
      hidden.outer.width() < 40.0,
      "hidden panel is {} wide",
      hidden.outer.width()
    );
    assert_fits(hidden);

    click(&ctx, &mut app, hidden.button.center());
    let back = settle(&ctx, &mut app, 30);
    assert!(app.settings_open);
    assert_width(back, SETTINGS_DEFAULT);
    assert_fits(back);
  }

  #[test]
  fn clicking_histogram_sets_the_kind() {
    let (ctx, mut app) = setup("t");
    let out = settled_output(&ctx, &mut app);
    let version = lock(&app.state).version;
    click(&ctx, &mut app, text_at(&out, "Histogram"));
    let st = lock(&app.state);
    assert_eq!(st.kind, Kind::Hist);
    assert!(st.version > version, "a kind change bumps the version");
  }

  #[test]
  fn clicking_3d_sets_the_kind_when_every_series_has_z() {
    // A control for the test above: the same click works once every series has z.
    let (ctx, mut app) = app_with(|st| {
      st.push_point("a", None, 1.0, Some(2.0)).unwrap();
      st.push_point("b", None, 2.0, Some(3.0)).unwrap();
    });
    let out = settled_output(&ctx, &mut app);
    click(&ctx, &mut app, text_at(&out, "3D"));
    assert_eq!(lock(&app.state).kind, Kind::ThreeD);
  }

  #[test]
  fn clicking_3d_leaves_the_kind_unchanged_while_a_series_lacks_z() {
    // `setup` has a series without z.
    let (ctx, mut app) = setup("t");
    let out = settled_output(&ctx, &mut app);
    click(&ctx, &mut app, text_at(&out, "3D"));
    assert_eq!(lock(&app.state).kind, Kind::Xy);
  }

  #[test]
  fn all_series_lines_sets_the_default_and_clears_the_series_geometries() {
    let (ctx, mut app) = setup("t");
    lock(&app.state)
      .geoms
      .insert("sine".to_string(), Geom::Points);
    let version = lock(&app.state).version;
    let out = settled_output(&ctx, &mut app);
    click(&ctx, &mut app, text_at(&out, "Lines"));
    let st = lock(&app.state);
    assert_eq!(st.default_geom, Geom::Lines);
    assert!(
      st.geoms.is_empty(),
      "series geometries left: {:?}",
      st.geoms
    );
    assert!(st.version > version);
  }

  #[test]
  fn xy_frames_draw_each_series_with_its_own_geometry() {
    let (ctx, mut app) = app_with(|st| {
      // Long and sorted, so that its line is decimated; long and unsorted, so that it is
      // drawn as points only.
      for i in 0..6000 {
        let t = i as f64;
        st.push_point("sorted", None, (t * 0.01).sin(), None)
          .unwrap();
        st.push_point("unsorted", Some(-t), t.cos(), None).unwrap();
      }
      for i in 0..200 {
        st.push_point("few", None, i as f64, None).unwrap();
      }
      st.geoms.insert("sorted".to_string(), Geom::Lines);
      st.geoms.insert("few".to_string(), Geom::Points);
    });
    for _ in 0..3 {
      whole_frame(&ctx, &mut app, Vec::new());
    }
    assert!(app.decimated.is_some(), "the long series are decimated");
  }

  #[test]
  fn empty_plot_draws_in_every_kind() {
    for plot_type in [PlotType::Both, PlotType::Hist, PlotType::Points3d] {
      let (ctx, mut app) = app_with(|st| st.set_type(plot_type, None).unwrap());
      for _ in 0..3 {
        whole_frame(&ctx, &mut app, Vec::new());
      }
    }
  }

  #[test]
  fn histogram_frames_in_both_layouts_cache_the_bins() {
    let (ctx, mut app) = app_with(|st| {
      for i in 0..50 {
        st.push_point("a", None, i as f64, None).unwrap();
        st.push_point("b", None, 2.0 * i as f64, None).unwrap();
      }
      st.set_type(PlotType::Hist, None).unwrap();
    });
    for layout in [HistLayout::Side, HistLayout::Stack] {
      app.hist_layout = layout;
      let mut out = whole_frame_in(&ctx, &mut app, Vec::new(), TALL);
      for _ in 0..2 {
        out = whole_frame_in(&ctx, &mut app, Vec::new(), TALL);
      }
      // 100 values: Sturges gives 8 bins, which the status line and the group both show.
      let cache = app.hist_cache.as_ref().expect("the histogram is cached");
      assert_eq!(cache.hist.as_ref().map(Histogram::bins), Some(8));
      assert_eq!(cache.key, (lock(&app.state).version, BinRule::Sturges, 2));
      assert!(
        texts(&out).iter().any(|(t, _)| t.ends_with("· 8 bins")),
        "the status line does not show 8 bins"
      );
    }
  }

  #[test]
  fn three_d_frames_report_the_drawn_counts() {
    let (ctx, mut app) = app_with(|st| {
      for (x, y, z) in lorenz(5000) {
        st.push_point("lorenz", Some(x), y, Some(z)).unwrap();
      }
      for i in 0..20_000 {
        let t = i as f64;
        st.push_point(
          "cloud",
          Some((t * 0.618_034).fract()),
          (t * 0.381_966).fract(),
          Some((t * 0.569_840).fract()),
        )
        .unwrap();
      }
      st.set_type(PlotType::Lines3d, Some("lorenz")).unwrap();
      st.set_type(PlotType::Points3d, Some("cloud")).unwrap();
    });
    for _ in 0..3 {
      whole_frame(&ctx, &mut app, Vec::new());
    }
    let (marks, total) = app.decimated.expect("the cloud is decimated");
    assert_eq!(total, 25_000);
    assert!(marks < total, "{marks} marks for {total} points");
  }

  #[test]
  fn side_bars_lie_inside_their_bin_and_do_not_overlap() {
    let counts = vec![vec![3, 0, 5, 1], vec![2, 4, 0, 1], vec![0, 0, 7, 9]];
    let h = Histogram {
      start: -2.0,
      width: 0.5,
      counts: counts.clone(),
    };
    let mut bins: Vec<Vec<(f64, f64)>> = vec![Vec::new(); h.bins()];
    for (g, group) in counts.iter().enumerate() {
      let bars = bar_layout(&h, HistLayout::Side, g, counts.len());
      assert_eq!(bars.len(), h.bins());
      for (b, &(arg, width, height)) in bars.iter().enumerate() {
        assert_eq!(height, group[b] as f64, "height of group {g} in bin {b}");
        assert!((width - 0.9 * h.width / counts.len() as f64).abs() < EPS);
        let lo = h.start + b as f64 * h.width;
        let hi = lo + h.width;
        let (left, right) = (arg - width / 2.0, arg + width / 2.0);
        assert!(
          left >= lo - EPS && right <= hi + EPS,
          "group {g} in bin {b} spans {left}..{right}, outside {lo}..{hi}"
        );
        bins[b].push((left, right));
      }
    }
    for bar_edges in &bins {
      for (i, a) in bar_edges.iter().enumerate() {
        for b in &bar_edges[i + 1..] {
          assert!(
            a.1 <= b.0 + EPS || b.1 <= a.0 + EPS,
            "bars {a:?} and {b:?} overlap"
          );
        }
      }
    }
  }

  #[test]
  fn stacked_bars_are_centred_with_the_counts_as_heights() {
    let counts = vec![vec![3, 0, 5], vec![2, 4, 0]];
    let h = Histogram {
      start: 1.0,
      width: 2.0,
      counts: counts.clone(),
    };
    for (g, group) in counts.iter().enumerate() {
      let bars = bar_layout(&h, HistLayout::Stack, g, counts.len());
      assert_eq!(bars.len(), h.bins());
      for (b, &(arg, width, height)) in bars.iter().enumerate() {
        assert!(
          (arg - h.center(b)).abs() < EPS,
          "bar {g} of bin {b} is off centre"
        );
        assert!((width - 0.9 * h.width).abs() < EPS);
        assert_eq!(height, group[b] as f64);
      }
    }
  }

  #[test]
  fn log_series_drops_points_without_a_log_place() {
    let mut st = PlotState::new();
    // Not sorted in x: the zero comes after the 1.
    for (x, y) in [
      (1.0, 10.0),
      (0.0, 5.0),
      (10.0, -1.0),
      (100.0, 1000.0),
      (1000.0, 0.5),
    ] {
      st.push_point("a", Some(x), y, None).unwrap();
    }
    // Sorted in x, with the same values but no zero.
    for (x, y) in [(1.0, 10.0), (10.0, -1.0), (100.0, 100.0), (1000.0, 0.5)] {
      st.push_point("b", Some(x), y, None).unwrap();
    }
    let (mapped, starts, dropped) = log_series(&st.series, [true, true]);
    assert_eq!(dropped, 3);
    assert_eq!(mapped[0].sorted, st.series[0].sorted);
    assert_eq!(mapped[1].sorted, st.series[1].sorted);
    let points = |s: &Series| s.points.iter().map(|p| (p.x, p.y)).collect::<Vec<_>>();
    let close = |got: &[(f64, f64)], want: &[(f64, f64)]| {
      got.len() == want.len()
        && got
          .iter()
          .zip(want)
          .all(|(g, w)| (g.0 - w.0).abs() < EPS && (g.1 - w.1).abs() < EPS)
    };
    let a = points(&mapped[0]);
    assert!(
      close(&a, &[(0.0, 1.0), (2.0, 3.0), (3.0, 0.5f64.log10())]),
      "{a:?}"
    );
    let b = points(&mapped[1]);
    assert!(
      close(&b, &[(0.0, 1.0), (2.0, 2.0), (3.0, 0.5f64.log10())]),
      "{b:?}"
    );
    assert!(
      b.windows(2).all(|w| w[0].0 <= w[1].0),
      "{b:?} is not sorted"
    );
    // In b the second point was left out, so a new run starts at the second mapped point.
    assert_eq!(starts[1], vec![1]);
  }

  #[test]
  fn runs_split_at_the_starts() {
    let pts: Vec<PlotPoint> = (0..6).map(|i| PlotPoint::new(i as f64, 1.0)).collect();
    let lens = |starts: &[usize]| {
      runs(&pts, starts)
        .map(<[PlotPoint]>::len)
        .collect::<Vec<_>>()
    };
    assert_eq!(lens(&[]), [6]);
    assert_eq!(lens(&[2, 5]), [2, 3, 1]);
    // Out-of-range or leading starts don't make empty runs.
    assert_eq!(lens(&[0, 6, 9]), [6]);
    // Leading and trailing gaps make no run start: [_, a, b, _, c] keeps [a, b] and [c].
    let mut st = PlotState::new();
    for (x, y) in [
      (1.0, -1.0),
      (2.0, 1.0),
      (3.0, 2.0),
      (4.0, 0.0),
      (5.0, 3.0),
      (6.0, -2.0),
    ] {
      st.push_point("s", Some(x), y, None).unwrap();
    }
    let (mapped, starts, dropped) = log_series(&st.series, [false, true]);
    assert_eq!((dropped, starts[0].clone()), (3, vec![2]));
    let lens: Vec<usize> = runs(mapped[0].points.as_slices().0, &starts[0])
      .map(<[PlotPoint]>::len)
      .collect();
    assert_eq!(lens, [2, 1]);
  }

  #[test]
  fn mapped_series_is_reused_until_the_data_or_the_axes_change() {
    let mut st = PlotState::new();
    st.push_point("a", Some(1.0), 1.0, None).unwrap();
    st.push_point("a", Some(0.0), 1.0, None).unwrap();
    let mut cache = None;
    let (_, _, dropped) = mapped_series(&mut cache, &st, [true, false]);
    assert_eq!(dropped, 1);
    // Marks the cache. Nothing has changed, so the next call must return it as it is.
    cache.as_mut().expect("the cache is kept").dropped = 99;
    let (_, _, dropped) = mapped_series(&mut cache, &st, [true, false]);
    assert_eq!(dropped, 99, "rebuilt although nothing changed");
    // A new point changes the version, so the cache is rebuilt.
    st.push_point("a", Some(4.0), 1.0, None).unwrap();
    let (series, _, dropped) = mapped_series(&mut cache, &st, [true, false]);
    assert_eq!(dropped, 1);
    assert_eq!(series[0].points.len(), 2);
    // Other log axes: rebuilt too.
    cache.as_mut().expect("the cache is kept").dropped = 99;
    let (_, _, dropped) = mapped_series(&mut cache, &st, [true, true]);
    assert_eq!(dropped, 1);
    // No log axis: the series as they are, and the cache is released.
    let (series, _, dropped) = mapped_series(&mut cache, &st, [false, false]);
    assert_eq!((dropped, series[0].points.len()), (0, 3));
    assert!(cache.is_none());
  }

  #[test]
  fn log_grid_draws_decades_stronger_than_subdivisions() {
    let grid = |lo: f64, hi: f64| {
      log_grid(GridInput {
        bounds: (lo, hi),
        base_step_size: 1.0,
      })
    };
    // About three decades: whole decades only, all with the strong step.
    let wide = grid(-0.3, 3.2);
    let values: Vec<f64> = wide.iter().map(|m| m.value).collect();
    assert_eq!(values, [0.0, 1.0, 2.0, 3.0]);
    assert!(wide.iter().all(|m| m.step_size == 1.0));
    // 1 to 100: 1, 2, 5, 10, 20 and 50, of which 1 and 10 are decades.
    let narrow = grid(0.0, 1.99);
    assert_eq!(narrow.len(), 6);
    let decades: Vec<f64> = narrow
      .iter()
      .filter(|m| m.step_size == 1.0)
      .map(|m| m.value)
      .collect();
    assert_eq!(decades, [0.0, 1.0]);
    assert_eq!(narrow.iter().filter(|m| m.step_size < 1.0).count(), 4);
  }

  #[test]
  fn log_bars_stack_on_cumulative_counts() {
    // Group 0 is empty in bin 0 and group 1 in bin 1. Bin 2 holds 4 values in each group.
    let h = Histogram {
      start: 0.0,
      width: 1.0,
      counts: vec![vec![0, 1, 4, 0], vec![5, 0, 4, 3]],
    };
    // Group 0 rises from zero to log 1 in bin 1 and to log 4 in bin 2. Bin 0 is left out.
    assert_bars(
      &log_bars(&h, HistLayout::Stack, 0, 2),
      &[(1.5, 0.9, 0.0, 0.0), (2.5, 0.9, 0.0, 4f64.log10())],
    );
    // Group 1 rises from zero to log 5 in bin 0. In bin 2 it sits on log 4 and reaches log 8,
    // and in bin 3 it rises from zero to log 3. Bin 1 is left out.
    assert_bars(
      &log_bars(&h, HistLayout::Stack, 1, 2),
      &[
        (0.5, 0.9, 0.0, 5f64.log10()),
        (2.5, 0.9, 4f64.log10(), 2f64.log10()),
        (3.5, 0.9, 0.0, 3f64.log10()),
      ],
    );
  }

  #[test]
  fn log_bars_side_rise_to_the_log_of_each_count() {
    let h = Histogram {
      start: 0.0,
      width: 1.0,
      counts: vec![vec![0, 1, 10], vec![3, 0, 100]],
    };
    // Two groups: bars 0.45 wide, the first at 0.05 + 0.5 * 0.45 into the bin, the second at
    // 0.05 + 1.5 * 0.45.
    assert_bars(
      &log_bars(&h, HistLayout::Side, 0, 2),
      &[(1.275, 0.45, 0.0, 0.0), (2.275, 0.45, 0.0, 1.0)],
    );
    assert_bars(
      &log_bars(&h, HistLayout::Side, 1, 2),
      &[(0.725, 0.45, 0.0, 3f64.log10()), (2.725, 0.45, 0.0, 2.0)],
    );
  }

  #[test]
  fn log_frames_run_in_every_kind() {
    for plot_type in [PlotType::Both, PlotType::Hist, PlotType::Points3d] {
      let (ctx, mut app) = app_with(|st| {
        for i in 0..40 {
          let t = i as f64;
          // Values <= 0 on every axis; z is there for the 3D plot.
          st.push_point("a", Some(t - 5.0), t - 3.0, Some(t - 7.0))
            .unwrap();
        }
        st.set_type(plot_type, None).unwrap();
      });
      app.log = [true; 3];
      for _ in 0..3 {
        whole_frame(&ctx, &mut app, Vec::new());
      }
    }
  }

  #[test]
  fn log_frames_count_the_points_left_out_in_the_status() {
    // Runs a few frames, so that the status shows the points left out by the last one. The
    // frames are tall enough for the status line to be drawn.
    let run = |ctx: &egui::Context, app: &mut PlotApp| {
      let mut out = whole_frame_in(ctx, app, Vec::new(), TALL);
      for _ in 0..2 {
        out = whole_frame_in(ctx, app, Vec::new(), TALL);
      }
      out
    };
    let shows =
      |out: &egui::FullOutput, text: &str| texts(out).iter().any(|(t, _)| t.ends_with(text));

    // x–y: (10, 0) has no log y and (-5, 100) no log x, so two points are left out.
    let (ctx, mut app) = app_with(|st| {
      for (x, y) in [
        (1.0, 1.0),
        (10.0, 0.0),
        (100.0, 10.0),
        (-5.0, 100.0),
        (1000.0, 1000.0),
      ] {
        st.push_point("a", Some(x), y, None).unwrap();
      }
    });
    app.log = [true, true, false];
    let out = run(&ctx, &mut app);
    assert!(shows(&out, "· 2 ≤ 0 hidden"), "{:?}", texts(&out));
    app.log = [false; 3];
    let out = run(&ctx, &mut app);
    assert!(!shows(&out, "hidden"), "{:?}", texts(&out));

    // Histogram: the values are y, so log x leaves out the 0 and the -2, while log counts
    // leave out nothing.
    let (ctx, mut app) = app_with(|st| {
      for y in [1.0, 0.0, 5.0, -2.0, 3.0, 4.0] {
        st.push_point("a", None, y, None).unwrap();
      }
      st.set_type(PlotType::Hist, None).unwrap();
    });
    app.log = [true, false, false];
    let out = run(&ctx, &mut app);
    assert!(shows(&out, "· 2 ≤ 0 hidden"), "{:?}", texts(&out));
    app.log = [false, true, false];
    let out = run(&ctx, &mut app);
    assert!(!shows(&out, "hidden"), "{:?}", texts(&out));
  }

  #[test]
  fn sig4_keeps_four_significant_digits() {
    let cases = [
      (1234.5678, "1235"),
      (0.00123456, "0.001235"),
      (12.5, "12.5"),
      (1.0, "1"),
      (-2.5, "-2.5"),
      (123456.0, "1.235e5"),
      (1e7, "1e7"),
      (0.00001234, "1.234e-5"),
      (0.0, "0"),
    ];
    for (v, text) in cases {
      assert_eq!(sig4(v), text);
    }
  }

  #[test]
  fn hover_shows_the_values_before_the_log() {
    let at = |x: f64, y: f64| HoverPosition::NearDataPoint {
      plot_name: "a",
      position: PlotPoint::new(x, y),
      index: 0,
    };
    assert_eq!(
      hover_text(&at(2.0, -1.0), [true, true]),
      "a\nx = 100\ny = 0.1"
    );
    assert_eq!(
      hover_text(&at(2.5, 1234.5678), [false, false]),
      "a\nx = 2.5\ny = 1235"
    );
  }

  #[test]
  fn axis_titles_mark_the_log_axes() {
    assert_eq!(axis_title("time", false), "time");
    assert_eq!(axis_title("time", true), "time (log)");
    assert_eq!(axis_title("", false), "");
    assert_eq!(axis_title("", true), "(log)");
  }

  #[test]
  fn glyphs_are_in_default_fonts() {
    let (ctx, _) = setup("t");
    // Fonts are loaded by the first frame.
    let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
    output.textures_delta.clear();
    let font = egui::FontId::proportional(14.0);
    for glyph in [HIDE_GLYPH, SHOW_GLYPH, "⌘", "·", "–", "…"] {
      assert!(
        ctx.fonts_mut(|f| f.has_glyphs(&font, glyph)),
        "no glyph for {glyph}"
      );
    }
  }

  #[test]
  fn copy_menu_offers_the_point_and_the_cursor_when_snapped() {
    let (ctx, mut app) = snapped_app();
    let at = plot_transform(&ctx, XY_PLOT_ID).position_from_point(&PlotPoint::new(0.1, 0.987));
    let released = click_in(&ctx, &mut app, at, egui::PointerButton::Secondary);
    assert!(copied(&released).is_empty(), "a right click copies nothing");
    let out = whole_settled(&ctx, &mut app, 2);
    assert_eq!(
      texts_starting(&out, "Copy point"),
      ["Copy point  a: 0.1, 0.987"]
    );
    assert_eq!(texts_starting(&out, "Copy cursor").len(), 1);
    // The pointer is still over the plot, but the menu replaces the hover label.
    assert!(!texts(&out).iter().any(|(t, _)| t.contains("x = ")));

    let out = click_in(
      &ctx,
      &mut app,
      text_starting_at(&out, "Copy point"),
      egui::PointerButton::Primary,
    );
    assert_eq!(copied(&out), ["0.1,0.987"]);
    let out = whole_settled(&ctx, &mut app, 2);
    assert!(
      texts_starting(&out, "Copy point").is_empty()
        && texts_starting(&out, "Copy cursor").is_empty(),
      "the menu closes after a copy"
    );
  }

  #[test]
  fn copy_menu_copies_the_cursor_away_from_data() {
    let (ctx, mut app) = app_with(|st| {
      for (x, y) in [(0.0, 0.0), (1.0, 1.0)] {
        st.push_point("a", Some(x), y, None).unwrap();
      }
    });
    whole_settled(&ctx, &mut app, 3);
    let plot = plot_transform(&ctx, XY_PLOT_ID);
    // Well off the segment from (0, 0) to (1, 1), which the hover snaps to along its length.
    let at = plot.position_from_point(&PlotPoint::new(0.5, 0.2));
    click_in(&ctx, &mut app, at, egui::PointerButton::Secondary);
    let out = whole_settled(&ctx, &mut app, 2);
    assert!(texts_starting(&out, "Copy point").is_empty());
    assert_eq!(texts_starting(&out, "Copy cursor").len(), 1);

    let out = click_in(
      &ctx,
      &mut app,
      text_starting_at(&out, "Copy cursor"),
      egui::PointerButton::Primary,
    );
    let text = copied(&out);
    assert_eq!(text.len(), 1, "one copy: {text:?}");
    let (x, y) = parse_pair(&text[0]);
    let bounds = plot.bounds();
    let [min, max] = [bounds.min(), bounds.max()];
    assert!(
      (min[0]..=max[0]).contains(&x) && (min[1]..=max[1]).contains(&y),
      "{x},{y} is outside the plot {min:?}..{max:?}"
    );
    assert!(
      (x - 0.5).abs() < 0.01 && (y - 0.2).abs() < 0.01,
      "{x},{y} is not the cursor"
    );
  }

  #[test]
  fn copy_menu_copies_the_original_values_on_a_log_axis() {
    let (ctx, mut app) = app_with(|st| {
      for (x, y) in [(0.1, 1000.0), (0.5, 10.0), (1.0, 0.1)] {
        st.push_point("a", Some(x), y, None).unwrap();
      }
    });
    app.log = [false, true, false];
    whole_settled(&ctx, &mut app, 3);
    // On the log axis the point 1000 lies at log10(1000) = 3.
    let at = plot_transform(&ctx, XY_PLOT_ID).position_from_point(&PlotPoint::new(0.1, 3.0));
    click_in(&ctx, &mut app, at, egui::PointerButton::Secondary);
    let out = whole_settled(&ctx, &mut app, 2);
    assert_eq!(
      texts_starting(&out, "Copy point"),
      ["Copy point  a: 0.1, 1000"]
    );

    let out = click_in(
      &ctx,
      &mut app,
      text_starting_at(&out, "Copy point"),
      egui::PointerButton::Primary,
    );
    assert_eq!(copied(&out), ["0.1,1000"]);
  }

  #[test]
  fn histogram_copy_menu_offers_only_the_cursor() {
    let (ctx, mut app) = app_with(|st| {
      for i in 0..50 {
        st.push_point("a", None, i as f64, None).unwrap();
      }
      st.set_type(PlotType::Hist, None).unwrap();
    });
    whole_settled(&ctx, &mut app, 3);
    let at = plot_transform(&ctx, HIST_PLOT_ID).frame().center();
    click_in(&ctx, &mut app, at, egui::PointerButton::Secondary);
    let out = whole_settled(&ctx, &mut app, 2);
    assert!(texts_starting(&out, "Copy point").is_empty());
    assert_eq!(texts_starting(&out, "Copy cursor").len(), 1);

    let out = click_in(
      &ctx,
      &mut app,
      text_starting_at(&out, "Copy cursor"),
      egui::PointerButton::Primary,
    );
    let text = copied(&out);
    assert_eq!(text.len(), 1, "one copy: {text:?}");
    parse_pair(&text[0]);
  }

  #[test]
  fn three_d_right_click_opens_no_menu() {
    let (ctx, mut app) = app_with(|st| {
      for i in 0..20 {
        let t = i as f64;
        st.push_point("a", Some(t), t.sin(), Some(t.cos())).unwrap();
      }
      st.set_type(PlotType::Points3d, None).unwrap();
    });
    whole_settled(&ctx, &mut app, 3);
    let released = click_in(
      &ctx,
      &mut app,
      egui::pos2(555.0, 280.0),
      egui::PointerButton::Secondary,
    );
    let out = whole_settled(&ctx, &mut app, 2);
    assert!(copied(&released).is_empty());
    assert!(
      texts_starting(&out, "Copy point").is_empty()
        && texts_starting(&out, "Copy cursor").is_empty(),
      "{:?}",
      texts(&out)
    );
  }

  #[test]
  fn right_drag_box_zooms_and_opens_no_menu() {
    let (ctx, mut app) = snapped_app();
    let plot = plot_transform(&ctx, XY_PLOT_ID);
    let start = plot.position_from_point(&PlotPoint::new(0.2, 0.2));
    let end = plot.position_from_point(&PlotPoint::new(0.6, 0.6));
    let before = plot.bounds().width();

    whole_frame(&ctx, &mut app, vec![pointer_moved(start)]);
    whole_frame(
      &ctx,
      &mut app,
      vec![
        pointer_moved(start),
        button_event(egui::PointerButton::Secondary, start, true),
      ],
    );
    const STEPS: usize = 10;
    for i in 1..=STEPS {
      let pos = start + (end - start) * (i as f32 / STEPS as f32);
      whole_frame(&ctx, &mut app, vec![pointer_moved(pos)]);
    }
    let released = whole_frame(
      &ctx,
      &mut app,
      vec![button_event(egui::PointerButton::Secondary, end, false)],
    );
    let out = whole_settled(&ctx, &mut app, 2);

    assert!(copied(&released).is_empty());
    assert!(
      texts_starting(&out, "Copy point").is_empty()
        && texts_starting(&out, "Copy cursor").is_empty(),
      "a drag opens no menu"
    );
    assert!(!app.follow, "the box zoom turns follow off");
    let zoomed = plot_transform(&ctx, XY_PLOT_ID).bounds().width();
    assert!(
      zoomed < 0.9 * before,
      "width {zoomed} is not zoomed in on {before}"
    );
  }

  #[test]
  fn copy_menu_keeps_the_values_of_its_click() {
    let (ctx, mut app) = snapped_app();
    let plot = plot_transform(&ctx, XY_PLOT_ID);
    let at = plot.position_from_point(&PlotPoint::new(0.1, 0.987));
    click_in(&ctx, &mut app, at, egui::PointerButton::Secondary);
    let open = whole_settled(&ctx, &mut app, 2);
    let cursor = texts_starting(&open, "Copy cursor");
    assert_eq!(cursor.len(), 1);

    // Moved away, over the plot where no data lies: the hover snaps elsewhere, the menu does not.
    let away = plot.position_from_point(&PlotPoint::new(0.9, 0.1));
    whole_frame(&ctx, &mut app, vec![pointer_moved(away)]);
    let moved = whole_settled(&ctx, &mut app, 3);
    assert_eq!(texts_starting(&moved, "Copy cursor"), cursor);
    assert_eq!(
      texts_starting(&moved, "Copy point"),
      ["Copy point  a: 0.1, 0.987"]
    );

    let out = click_in(
      &ctx,
      &mut app,
      text_starting_at(&moved, "Copy point"),
      egui::PointerButton::Primary,
    );
    assert_eq!(copied(&out), ["0.1,0.987"]);
  }

  #[test]
  fn copy_note_shows_for_1_2_seconds() {
    let (ctx, mut app) = snapped_app();
    let at = plot_transform(&ctx, XY_PLOT_ID).position_from_point(&PlotPoint::new(0.1, 0.987));
    click_in(&ctx, &mut app, at, egui::PointerButton::Secondary);
    let open = whole_settled(&ctx, &mut app, 2);
    let released = click_in(
      &ctx,
      &mut app,
      text_starting_at(&open, "Copy point"),
      egui::PointerButton::Primary,
    );
    assert_eq!(copied(&released), ["0.1,0.987"]);
    let shows = |out: &egui::FullOutput| texts(out).iter().any(|(t, _)| t == "Copied 0.1,0.987");
    // The note's area is laid out invisibly on its first frame, so it shows from the next one.
    assert!(shows(&whole_settled(&ctx, &mut app, 1)));
    // Half a second on, the note is still there; 1.5 s on, it is gone.
    assert!(shows(&whole_settled(&ctx, &mut app, 30)));
    assert!(
      !shows(&whole_settled(&ctx, &mut app, 60)),
      "the note outlives 1.2 s"
    );
  }

  #[test]
  fn copy_menu_copies_the_pushed_values_on_every_axis_scale() {
    // 10^log10(12.5) is 12.500000000000002, so copying through the log axis would be wrong.
    let samples = [
      ((12.5, 3.7), "12.5,3.7"),
      ((3.7, 0.3), "3.7,0.3"),
      ((0.3, 12.5), "0.3,12.5"),
    ];
    let drawn = |v: f64, log: bool| {
      if log {
        to_log(v).expect("a positive value")
      } else {
        v
      }
    };
    for logs in [[false, false], [true, false], [false, true], [true, true]] {
      let (ctx, mut app) = app_with(|st| {
        for ((x, y), _) in samples {
          st.push_point("a", Some(x), y, None).unwrap();
        }
      });
      app.log = [logs[0], logs[1], false];
      whole_settled(&ctx, &mut app, 3);
      for ((x, y), text) in samples {
        let plot = plot_transform(&ctx, XY_PLOT_ID);
        let at = plot.position_from_point(&PlotPoint::new(drawn(x, logs[0]), drawn(y, logs[1])));
        click_in(&ctx, &mut app, at, egui::PointerButton::Secondary);
        let open = whole_settled(&ctx, &mut app, 2);
        assert_eq!(
          texts_starting(&open, "Copy point").len(),
          1,
          "no point snapped at {text} on {logs:?}"
        );
        let out = click_in(
          &ctx,
          &mut app,
          text_starting_at(&open, "Copy point"),
          egui::PointerButton::Primary,
        );
        assert_eq!(copied(&out), [text], "on {logs:?}");
      }
    }
  }

  #[test]
  fn copy_menu_finds_the_sample_of_a_decimated_log_series() {
    // Longer than LOD_MIN and sorted, so its line and its points are decimated to fit the screen.
    let samples: Vec<(f64, f64)> = (0..6000)
      .map(|i| (1.0 + i as f64 * 0.37, 0.3 + i as f64 * 0.11))
      .collect();
    let (ctx, mut app) = app_with(|st| {
      for &(x, y) in &samples {
        st.push_point("a", Some(x), y, None).unwrap();
      }
    });
    app.log = [true, true, false];
    whole_settled(&ctx, &mut app, 3);
    let (x, y) = samples[3001];
    let plot = plot_transform(&ctx, XY_PLOT_ID);
    let at = plot.position_from_point(&PlotPoint::new(
      to_log(x).expect("positive"),
      to_log(y).expect("positive"),
    ));
    click_in(&ctx, &mut app, at, egui::PointerButton::Secondary);
    let open = whole_settled(&ctx, &mut app, 2);
    assert_eq!(texts_starting(&open, "Copy point").len(), 1);
    let out = click_in(
      &ctx,
      &mut app,
      text_starting_at(&open, "Copy point"),
      egui::PointerButton::Primary,
    );
    // The point snapped to is a kept sample, which must be copied as pushed. Many of these
    // values come back inexact through log10, so a copy through 10^v would not match any sample.
    let text = copied(&out);
    assert_eq!(text.len(), 1, "one copy: {text:?}");
    assert!(
      samples.iter().any(|(x, y)| format!("{x},{y}") == text[0]),
      "{} is not a sample",
      text[0]
    );
  }

  /// The copy menu open over a snapped point, rendered at 2 pixels per point for a look. Writes
  /// `copy-menu.png` into the directory named by `COMPLOT_SNAPSHOT_DIR`, if it is set.
  #[test]
  #[ignore = "writes PNG files: set COMPLOT_SNAPSHOT_DIR and run with --ignored"]
  fn copy_menu_snapshot() {
    let Some(dir) = std::env::var_os("COMPLOT_SNAPSHOT_DIR").map(std::path::PathBuf::from) else {
      return;
    };
    let (ctx, mut app) = app_with(|st| {
      for i in 0..300 {
        let t = i as f64 * 0.05;
        st.push_point("sine", Some(t), t.sin(), None).unwrap();
        st.push_point("cosine", Some(t), t.cos(), None).unwrap();
      }
    });
    let mut renderer = crate::snapshot::Renderer::default();
    for _ in 0..3 {
      frame_at_2x(&ctx, &mut renderer, &mut app, Vec::new());
    }
    let t: f64 = 31.0 * 0.05;
    let at = plot_transform(&ctx, XY_PLOT_ID).position_from_point(&PlotPoint::new(t, t.sin()));
    let secondary = egui::PointerButton::Secondary;
    frame_at_2x(&ctx, &mut renderer, &mut app, vec![pointer_moved(at)]);
    frame_at_2x(
      &ctx,
      &mut renderer,
      &mut app,
      vec![pointer_moved(at), button_event(secondary, at, true)],
    );
    frame_at_2x(
      &ctx,
      &mut renderer,
      &mut app,
      vec![button_event(secondary, at, false)],
    );
    // The menu fades in over its first frames, so it is drawn fully only after some more.
    let mut last = None;
    for _ in 0..30 {
      last = Some(frame_at_2x(&ctx, &mut renderer, &mut app, Vec::new()));
    }
    renderer
      .render(&ctx, last.expect("frames ran"))
      .write_png(&dir.join("copy-menu.png"))
      .expect("the PNG is written");
  }

  #[test]
  #[ignore = "writes PNG files: set COMPLOT_SNAPSHOT_DIR and run with --ignored"]
  fn snapshots() {
    let Some(dir) = std::env::var_os("COMPLOT_SNAPSHOT_DIR").map(std::path::PathBuf::from) else {
      return;
    };
    let size = egui::vec2(900.0, 560.0);
    let shot = |file: &str, app: &mut PlotApp| {
      let image = crate::snapshot::snapshot(size, 2.0, 20, |ui| app.draw(ui));
      image
        .write_png(&dir.join(file))
        .expect("the PNG is written");
    };

    let (_, mut xy) = app_with(|st| {
      for i in 0..300 {
        let t = i as f64;
        st.push_point("sine", None, (t * 0.05).sin(), None).unwrap();
        st.push_point("cosine", None, (t * 0.05).cos(), None)
          .unwrap();
      }
    });
    shot("xy.png", &mut xy);

    let hist_data = |st: &mut PlotState| {
      for i in 0..400 {
        let t = i as f64;
        st.push_point("a", None, 3.0 * (t * 0.37).sin() + 1.0, None)
          .unwrap();
        st.push_point("b", None, 2.0 * (t * 0.23).cos() + 4.0, None)
          .unwrap();
        st.push_point("c", None, (t * 0.11).sin() + 2.0, None)
          .unwrap();
      }
      st.set_type(PlotType::Hist, None).unwrap();
    };
    let (_, mut hist_side) = app_with(hist_data);
    shot("hist_side.png", &mut hist_side);
    let (_, mut hist_stack) = app_with(hist_data);
    hist_stack.hist_layout = HistLayout::Stack;
    shot("hist_stack.png", &mut hist_stack);

    let (_, mut three_d) = app_with(|st| {
      for (x, y, z) in lorenz(5000) {
        st.push_point("lorenz", Some(x), y, Some(z)).unwrap();
      }
      for i in 0..20_000 {
        let t = i as f64;
        st.push_point(
          "cloud",
          Some((t * 0.618_034).fract()),
          (t * 0.381_966).fract(),
          Some((t * 0.569_840).fract()),
        )
        .unwrap();
      }
      st.set_type(PlotType::Lines3d, Some("lorenz")).unwrap();
      st.set_type(PlotType::Points3d, Some("cloud")).unwrap();
    });
    shot("3d.png", &mut three_d);

    // Exponential growth on a log y axis. The wave has values <= 0, which are left out.
    let (_, mut xy_log) = app_with(|st| {
      for i in 0..300 {
        let t = i as f64;
        st.push_point("growth", None, 10.0 * 1.04f64.powf(t), None)
          .unwrap();
        st.push_point("decay", None, 1e4 * 0.97f64.powf(t), None)
          .unwrap();
        st.push_point("wave", None, (t * 0.05).sin(), None).unwrap();
      }
    });
    xy_log.log = [false, true, false];
    shot("xy_log.png", &mut xy_log);

    // Log-normal values in two groups, binned in log x and stacked with log counts. The normal
    // deviates come from two low-discrepancy sequences, so the picture is the same on every run.
    let normal = |i: usize, salt: f64| {
      let t = i as f64;
      let u1 = (t * 0.618_034 + salt).fract().max(1e-9);
      let u2 = (t * 0.414_214 + 2.0 * salt).fract();
      (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    };
    let (_, mut hist_log) = app_with(|st| {
      for i in 0..600 {
        st.push_point("low", None, (0.5 * normal(i, 0.1)).exp(), None)
          .unwrap();
        st.push_point("high", None, (1.2 + 0.4 * normal(i, 0.7)).exp(), None)
          .unwrap();
      }
      st.set_type(PlotType::Hist, None).unwrap();
    });
    hist_log.log = [true, true, false];
    hist_log.hist_layout = HistLayout::Stack;
    shot("hist_log.png", &mut hist_log);
  }
}
