//! The plot window: a side panel with settings and the plot itself.

use crate::hist::{self, BinRule, Histogram};
use crate::lod::{decimate_line, decimate_points};
use crate::model::{Geom, Kind, PlotState, Series};
use crate::view3d::{Series3d, View3d};
use eframe::egui;
use egui_plot::{Bar, BarChart, Legend, Line, Plot, PlotPoint, PlotPoints, PlotUi, Points};
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// Series with more points than this are decimated to what fits on screen.
const LOD_MIN: usize = 5_000;

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

/// A histogram and the key it was computed for: data version, bin rule and series count.
struct HistCache {
  key: (u64, BinRule, usize),
  /// `None` when there are no values.
  hist: Option<Histogram>,
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
  /// Camera and display state of the 3D view.
  view3d: View3d,
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

/// Histogram of the series in `st`, from `cache` if its key still matches. The points must be
/// contiguous, as [`lock_contiguous`] makes them.
fn cached_histogram<'a>(
  cache: &'a mut Option<HistCache>,
  rule: BinRule,
  st: &PlotState,
) -> Option<&'a Histogram> {
  let key = (st.version, rule, st.series.len());
  if cache.as_ref().is_none_or(|c| c.key != key) {
    let slices: Vec<&[PlotPoint]> = st.series.iter().map(|s| s.points.as_slices().0).collect();
    *cache = Some(HistCache {
      key,
      hist: hist::histogram(&slices, rule),
    });
  }
  cache.as_ref()?.hist.as_ref()
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

/// One bar chart per series, in series order. In a stack, each chart sits on the ones before it.
fn bar_charts(h: &Histogram, layout: HistLayout, series: &[Series]) -> Vec<BarChart> {
  let mut charts: Vec<BarChart> = Vec::with_capacity(series.len());
  for (g, s) in series.iter().enumerate() {
    let color = palette(g);
    // egui_plot's default fill is 20% opaque, too faint to tell stacked bars apart.
    let (fill, stroke) = (color.gamma_multiply(0.6), egui::Stroke::new(1.0, color));
    let bars: Vec<Bar> = bar_layout(h, layout, g, series.len())
      .into_iter()
      .map(|(arg, width, height)| Bar::new(arg, height).width(width).fill(fill).stroke(stroke))
      .collect();
    let chart = BarChart::new(s.name.as_str(), bars).color(color);
    let chart = match layout {
      HistLayout::Side => chart,
      HistLayout::Stack => {
        let below: Vec<&BarChart> = charts.iter().collect();
        chart.stack_on(&below)
      }
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
      view3d: View3d::default(),
    }
  }

  /// Draws the window: the settings panel, then the plot.
  fn draw(&mut self, ui: &mut egui::Ui) {
    self.side_panel(ui);
    self.plot(ui);
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

  /// Number of bins of the histogram of the data, 0 with no values. Refreshes the cache.
  fn hist_bins(&mut self) -> usize {
    let state = Arc::clone(&self.state);
    let st = lock_contiguous(&state);
    cached_histogram(&mut self.hist_cache, self.bin_rule, &st).map_or(0, Histogram::bins)
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

  /// The x–y plot, each series with its own geometry.
  fn xy_plot(&mut self, ui: &mut egui::Ui, st: &PlotState, request: Option<bool>) {
    let (radius, width) = (self.point_radius, self.line_width);
    let (legend, grid) = (self.legend, self.grid);
    let name = &self.name;
    let total = st.total_points();
    let mut follow: Option<bool> = None;
    let mut decimated: Option<(usize, usize)> = None;

    egui::CentralPanel::default().show(ui, |ui| {
      let mut plot = Plot::new("complot_plot").show_grid(grid);
      if legend {
        plot = plot.legend(Legend::default());
      }
      if !st.x_label.is_empty() {
        plot = plot.x_axis_label(st.x_label.as_str());
      }
      if !st.y_label.is_empty() {
        plot = plot.y_axis_label(st.y_label.as_str());
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
        let mut marks = vec![0; st.series.len()];
        let mut any_decimated = false;
        // Lines first, so that the points of every series are drawn on top.
        // A line and the points of a series share a name and an id (the
        // default), so the legend shows one entry for both.
        for (i, s) in st.series.iter().enumerate().filter(|(_, s)| as_line(s)) {
          let pts = s.points.as_slices().0;
          let line = if big(s) {
            let [x0, x1, _, _] = decimation_range(pts, s.sorted, auto, min, max);
            let kept = decimate_line(pts, x0, x1, columns);
            any_decimated = true;
            marks[i] = kept.len();
            Line::new(s.name.as_str(), PlotPoints::Owned(kept))
          } else {
            marks[i] = pts.len();
            Line::new(s.name.as_str(), PlotPoints::Borrowed(pts))
          };
          plot_ui.line(line.color(palette(i)).width(width));
        }
        for (i, s) in st.series.iter().enumerate().filter(|(_, s)| as_points(s)) {
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
    });

    if let Some(on) = follow {
      self.follow = on;
    }
    self.set_decimated(ui, decimated);
  }

  /// The histogram: one bar chart per series, over bins shared by all of them.
  fn hist_plot(&mut self, ui: &mut egui::Ui, st: &PlotState, request: Option<bool>) {
    let layout = self.hist_layout;
    let charts = match cached_histogram(&mut self.hist_cache, self.bin_rule, st) {
      Some(h) => bar_charts(h, layout, &st.series),
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
    let mut follow: Option<bool> = None;

    egui::CentralPanel::default().show(ui, |ui| {
      let mut plot = Plot::new("complot_hist")
        .show_grid(grid)
        .x_axis_label(x_label)
        .y_axis_label("count");
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
    });

    if let Some(on) = follow {
      self.follow = on;
    }
    self.set_decimated(ui, None);
  }

  /// The 3D view of the series that have z values. Each series is drawn with its geometry.
  fn three_d_plot(&mut self, ui: &mut egui::Ui, st: &PlotState) {
    self.view3d.point_radius = self.point_radius;
    self.view3d.line_width = self.line_width;
    self.view3d.legend = self.legend;
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

    egui::CentralPanel::default().show(ui, |ui| {
      let drawn = self.view3d.show(ui, &series, labels);
      if drawn.marks < drawn.total {
        decimated = Some((drawn.marks, drawn.total));
      }
      if empty {
        paint_waiting(ui, drawn.response.rect, &self.name);
      }
    });
    self.set_decimated(ui, decimated);
  }

  /// Stores the decimation status for the settings of the next frame. The settings of this frame
  /// are already drawn, so a changed status asks for another frame.
  fn set_decimated(&mut self, ui: &egui::Ui, decimated: Option<(usize, usize)>) {
    if self.decimated != decimated {
      ui.ctx().request_repaint();
    }
    self.decimated = decimated;
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

  /// Time between frames, as at 60 Hz. Animations run on this clock.
  const FRAME_TIME: f64 = 1.0 / 60.0;

  /// Size of the screen the panel is drawn in.
  const SCREEN: egui::Vec2 = egui::vec2(900.0, 560.0);

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
    let mut output = ctx.run_ui(raw_input(ctx, SCREEN, events), |ui| app.draw(ui));
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

  fn pointer_moved(pos: egui::Pos2) -> egui::Event {
    egui::Event::PointerMoved(pos)
  }

  /// A press or a release of the primary button at `pos`.
  fn pointer_button(pos: egui::Pos2, pressed: bool) -> egui::Event {
    egui::Event::PointerButton {
      pos,
      button: egui::PointerButton::Primary,
      pressed,
      modifiers: egui::Modifiers::NONE,
    }
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

  /// Widest values: the largest slider values and the largest buffer limit.
  fn widest(app: &mut PlotApp) {
    app.point_radius = 10.0;
    app.line_width = 8.0;
    app.max_points = usize::MAX;
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
      let mut out = whole_frame(&ctx, &mut app, Vec::new());
      for _ in 0..2 {
        out = whole_frame(&ctx, &mut app, Vec::new());
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
  }
}
