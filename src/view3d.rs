//! 3D view: an orbiting camera over boxed axes, drawn with egui's painter.

use crate::scale;
use eframe::egui;
use egui_plot::PlotPoint;
use std::collections::HashSet;
use std::sync::Arc;

/// One series to draw in 3D.
pub struct Series3d<'a> {
  pub name: &'a str,
  /// x and y of each point.
  pub xy: &'a [PlotPoint],
  /// z of each point, as many as `xy`.
  pub z: &'a [f64],
  pub color: egui::Color32,
  pub lines: bool,
  pub points: bool,
}

/// Camera and display state of a 3D view, kept between frames.
#[derive(Debug, Clone, PartialEq)]
pub struct View3d {
  /// Rotation about the vertical z axis, in radians.
  pub azimuth: f32,
  /// Angle of the camera above the x–y plane, in radians, within ±89°.
  pub elevation: f32,
  /// Magnification; 1 fits the box in the view.
  pub zoom: f32,
  /// Perspective projection instead of orthographic.
  pub perspective: bool,
  pub point_radius: f32,
  pub line_width: f32,
  pub legend: bool,
  /// Names of the series hidden by clicking their legend entry.
  pub hidden: HashSet<String>,
  /// Log scale on the x, y and z axes. Points with a coordinate ≤ 0 on a log axis are left out.
  pub log: [bool; 3],
}

impl Default for View3d {
  fn default() -> Self {
    // The camera of matplotlib's 3D axes.
    View3d {
      azimuth: (-60.0f32).to_radians(),
      elevation: 30.0f32.to_radians(),
      zoom: 1.0,
      perspective: false,
      point_radius: 2.0,
      line_width: 1.5,
      legend: true,
      hidden: HashSet::new(),
      log: [false; 3],
    }
  }
}

/// What one frame of a 3D view drew.
pub struct Drawn {
  pub response: egui::Response,
  /// Marks drawn after decimation: line vertices plus markers.
  pub marks: usize,
  /// Points of the visible series.
  pub total: usize,
  /// Points left out because a coordinate is ≤ 0 on a log axis.
  pub dropped: usize,
}

/// Distance from the camera to the centre of the box, in box half-sizes, for perspective.
const CAMERA_DISTANCE: f32 = 4.0;
/// Half the diagonal of the box `[-1, 1]^3`.
const SQRT_3: f32 = 1.732_050_8;
/// Radians of rotation per point dragged.
const DRAG_SPEED: f32 = 0.01;
/// Exponent of the zoom factor per point scrolled.
const SCROLL_SPEED: f32 = 0.0015;
const ZOOM_MIN: f32 = 0.1;
const ZOOM_MAX: f32 = 20.0;
/// Largest elevation, in degrees, either side of the x–y plane.
const MAX_ELEVATION_DEG: f32 = 89.0;
/// Ticks per axis.
const TICKS: usize = 5;
/// Ticks per log axis: every decade of a span of up to this many, see `scale::log_ticks`.
const LOG_TICKS: usize = 8;
/// Distance in points from an edge to its tick labels, and to its axis title.
const TICK_OFFSET: f32 = 14.0;
const TITLE_OFFSET: f32 = 34.0;
/// Space in points between the outermost tick label of an axis and its title.
const TITLE_GAP: f32 = 6.0;
/// Smallest distance in points between two kept line vertices.
const MIN_LINE_GAP: f32 = 0.5;
/// Legend geometry in points: swatch size, gap after the swatch, padding of a row, margin to the corner.
const SWATCH: f32 = 10.0;
const LEGEND_GAP: f32 = 6.0;
const LEGEND_PAD: f32 = 3.0;
const LEGEND_MARGIN: f32 = 8.0;
/// Tolerance on tick values, in units of the tick step, and the most decimals a tick label shows.
const TICK_EPS: f64 = 1e-9;
const MAX_DECIMALS: usize = 15;

impl View3d {
  /// Puts the camera back to its defaults, keeping the display settings.
  pub fn reset_camera(&mut self) {
    let default = View3d::default();
    self.azimuth = default.azimuth;
    self.elevation = default.elevation;
    self.zoom = default.zoom;
  }

  /// Draws `series` in all the available space and handles the input: drag
  /// rotates, scroll or pinch zooms, double-click resets the camera, and a
  /// click on a legend entry hides or shows its series. `labels` are the
  /// titles of the x, y and z axes.
  pub fn show(&mut self, ui: &mut egui::Ui, series: &[Series3d<'_>], labels: [&str; 3]) -> Drawn {
    let (rect, response) =
      ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
    self.camera_input(ui, &response);

    let painter = ui.painter_at(rect);
    let font = egui::TextStyle::Body.resolve(ui.style());
    let rows = if self.legend {
      legend_rows(&painter, &font, rect, series)
    } else {
      Vec::new()
    };
    // Toggled before drawing, so that this frame already shows the change.
    for (i, row) in rows.iter().enumerate() {
      let row_response = ui
        .interact(*row, legend_id(ui.id(), i), egui::Sense::click_and_drag())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
      if row_response.clicked() {
        let name = series[i].name;
        if !self.hidden.remove(name) {
          self.hidden.insert(name.to_owned());
        }
      }
    }

    let ranges = data_ranges(series, &self.hidden, self.log);
    let cam = Camera::new(
      rect,
      self.azimuth,
      self.elevation,
      self.zoom,
      self.perspective,
    );
    draw_box(ui, &painter, &cam, ranges, self.log, labels);
    let (marks, total, dropped) = self.draw_series(&painter, &cam, ranges, series);
    self.draw_legend(ui, &painter, &font, &rows, series);
    Drawn {
      response,
      marks,
      total,
      dropped,
    }
  }

  /// Applies this frame's drag, scroll, pinch and double-click to the camera.
  fn camera_input(&mut self, ui: &egui::Ui, response: &egui::Response) {
    let drag = response.drag_delta();
    let max_elevation = MAX_ELEVATION_DEG.to_radians();
    self.azimuth -= drag.x * DRAG_SPEED;
    self.elevation = (self.elevation + drag.y * DRAG_SPEED).clamp(-max_elevation, max_elevation);
    if response.hovered() {
      let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta(), i.zoom_delta()));
      self.zoom = (self.zoom * (scroll.y * SCROLL_SPEED).exp() * pinch).clamp(ZOOM_MIN, ZOOM_MAX);
    }
    if response.double_clicked() {
      self.reset_camera();
    }
  }

  /// Draws the visible series after decimation. Returns the marks drawn, the points seen and the
  /// points left out by a log axis.
  fn draw_series(
    &self,
    painter: &egui::Painter,
    cam: &Camera,
    ranges: [(f64, f64); 3],
    series: &[Series3d<'_>],
  ) -> (usize, usize, usize) {
    let (mut marks, mut total, mut dropped) = (0, 0, 0);
    for s in series.iter().filter(|s| !self.hidden.contains(s.name)) {
      total += s.xy.len().min(s.z.len());
      dropped += s
        .xy
        .iter()
        .zip(s.z)
        .filter(|&(p, &z)| log_dropped([p.x, p.y, z], self.log))
        .count();
      if s.lines {
        // Each run of drawable points is a line of its own, decimated on its own.
        for run in line_runs(s.xy, s.z, self.log) {
          let positions = run.into_iter().filter_map(|v| screen(cam, ranges, v));
          let line = decimate_line(positions, MIN_LINE_GAP);
          marks += line.len();
          if line.len() >= 2 {
            painter.add(egui::Shape::line(
              line,
              egui::Stroke::new(self.line_width, s.color),
            ));
          }
        }
      }
      if s.points {
        let cell = self.point_radius.max(1.0);
        let positions = space_points(s.xy, s.z, self.log)
          .flatten()
          .filter_map(|v| screen(cam, ranges, v));
        let kept = decimate_points(positions, cam.rect, cell);
        marks += kept.len();
        for p in kept {
          painter.circle_filled(p, self.point_radius, s.color);
        }
      }
    }
    (marks, total, dropped)
  }

  /// Draws the legend rows from `legend_rows`, on a background, with a swatch and name each.
  fn draw_legend(
    &self,
    ui: &egui::Ui,
    painter: &egui::Painter,
    font: &egui::FontId,
    rows: &[egui::Rect],
    series: &[Series3d<'_>],
  ) {
    let Some(boxed) = rows.iter().copied().reduce(|a, b| a.union(b)) else {
      return;
    };
    let background = ui.visuals().window_fill().gamma_multiply(0.9);
    painter.rect_filled(boxed, egui::CornerRadius::same(4), background);
    for (row, s) in rows.iter().zip(series) {
      let (swatch, text) = if self.hidden.contains(s.name) {
        (egui::Color32::GRAY, ui.visuals().weak_text_color())
      } else {
        (s.color, ui.visuals().text_color())
      };
      let centre = egui::pos2(row.left() + LEGEND_PAD + SWATCH / 2.0, row.center().y);
      let swatch_rect = egui::Rect::from_center_size(centre, egui::Vec2::splat(SWATCH));
      painter.rect_filled(swatch_rect, egui::CornerRadius::same(2), swatch);
      let galley = painter.layout_no_wrap(s.name.to_owned(), font.clone(), text);
      let at = egui::pos2(
        row.left() + LEGEND_PAD + SWATCH + LEGEND_GAP,
        row.center().y - galley.size().y / 2.0,
      );
      painter.galley(at, galley, text);
    }
  }
}

/// Projection of the box `[-1, 1]^3` onto a view rectangle.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Camera {
  rect: egui::Rect,
  /// Points per box unit, reduced for perspective so that the box still fits.
  scale: f32,
  /// Unit vector from the centre towards the camera.
  d: [f32; 3],
  /// Unit vector pointing right on screen.
  r: [f32; 3],
  /// Unit vector pointing up on screen.
  u: [f32; 3],
  perspective: bool,
}

impl Camera {
  /// A camera at `azimuth` and `elevation`, in radians, that fits the box in `rect` at `zoom`.
  fn new(rect: egui::Rect, azimuth: f32, elevation: f32, zoom: f32, perspective: bool) -> Self {
    let (sin_az, cos_az) = azimuth.sin_cos();
    let (sin_el, cos_el) = elevation.sin_cos();
    let mut scale = zoom * 0.5 * rect.width().min(rect.height()) / SQRT_3;
    if perspective {
      scale *= (CAMERA_DISTANCE - SQRT_3) / CAMERA_DISTANCE;
    }
    Camera {
      rect,
      scale,
      d: [cos_el * cos_az, cos_el * sin_az, sin_el],
      r: [-sin_az, cos_az, 0.0],
      u: [-sin_el * cos_az, -sin_el * sin_az, cos_el],
      perspective,
    }
  }

  /// Screen position of the point `p` of the box, and its depth. A larger depth is nearer the camera.
  fn project(&self, p: [f32; 3]) -> (egui::Pos2, f32) {
    let depth = dot(p, self.d);
    let factor = if self.perspective {
      CAMERA_DISTANCE / (CAMERA_DISTANCE - depth)
    } else {
      1.0
    };
    let (sx, sy) = (dot(p, self.r), dot(p, self.u));
    let offset = egui::vec2(sx, -sy) * (self.scale * factor);
    (self.rect.center() + offset, depth)
  }
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
  a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// The point with the given coordinates on the three axes.
fn place(coords: [(usize, f32); 3]) -> [f32; 3] {
  let mut p = [0.0; 3];
  for (axis, v) in coords {
    p[axis] = v;
  }
  p
}

/// For each axis, the side (-1 or 1) of the box whose pane faces away from the camera at `d`.
fn back_sides(d: [f32; 3]) -> [f32; 3] {
  d.map(|c| if c >= 0.0 { -1.0 } else { 1.0 })
}

/// The edges of the box that lie on a back pane, as pairs of corners.
fn back_edges(back: [f32; 3]) -> Vec<([f32; 3], [f32; 3])> {
  let mut edges = Vec::new();
  for k in 0..3 {
    let (i, j) = ((k + 1) % 3, (k + 2) % 3);
    for si in [-1.0, 1.0] {
      for sj in [-1.0, 1.0] {
        if si == back[i] || sj == back[j] {
          let from = place([(k, -1.0), (i, si), (j, sj)]);
          let to = place([(k, 1.0), (i, si), (j, sj)]);
          edges.push((from, to));
        }
      }
    }
  }
  edges
}

/// The x and y of the vertical edge of the box that projects furthest left.
fn leftmost_vertical(cam: &Camera) -> [f32; 2] {
  let corners = [[-1.0, -1.0], [1.0, -1.0], [1.0, 1.0], [-1.0, 1.0]];
  let mut best = corners[0];
  let mut best_x = f32::INFINITY;
  for c in corners {
    let x = cam.project([c[0], c[1], 0.0]).0.x;
    if x < best_x {
      best_x = x;
      best = c;
    }
  }
  best
}

/// Point of the edge that carries the ticks of axis `a` at normalised position `n`. The x and y
/// ticks run along the front bottom edges; z runs up the vertical edge `zedge`.
fn tick_edge(a: usize, n: f32, front: [f32; 3], zedge: [f32; 2]) -> [f32; 3] {
  match a {
    0 => place([(0, n), (1, front[1]), (2, -1.0)]),
    1 => place([(0, front[0]), (1, n), (2, -1.0)]),
    _ => place([(0, zedge[0]), (1, zedge[1]), (2, n)]),
  }
}

/// Position of `v` in `range`: -1 at its low end and 1 at its high end.
fn to_unit(v: f64, (lo, hi): (f64, f64)) -> f32 {
  // Halved first, so that a range wider than f64::MAX does not overflow.
  (2.0 * (v / 2.0 - lo / 2.0) / (hi / 2.0 - lo / 2.0) - 1.0) as f32
}

/// Range of each axis over the drawable points of the visible series, in the space of the box
/// (exponents on a log axis), widened where it is flat. `[0, 1]` when there are no points.
fn data_ranges(
  series: &[Series3d<'_>],
  hidden: &HashSet<String>,
  log: [bool; 3],
) -> [(f64, f64); 3] {
  let mut lo = [f64::INFINITY; 3];
  let mut hi = [f64::NEG_INFINITY; 3];
  let mut any = false;
  for s in series.iter().filter(|s| !hidden.contains(s.name)) {
    for v in space_points(s.xy, s.z, log).flatten() {
      any = true;
      for (a, &c) in v.iter().enumerate() {
        lo[a] = lo[a].min(c);
        hi[a] = hi[a].max(c);
      }
    }
  }
  if !any {
    return [(0.0, 1.0); 3];
  }
  std::array::from_fn(|a| {
    if lo[a] == hi[a] {
      (lo[a] - 0.5, hi[a] + 0.5)
    } else {
      (lo[a], hi[a])
    }
  })
}

/// The coordinates of a point in the space the box is drawn in: log10 on the log axes. None when
/// a coordinate is not finite, or not positive on a log axis.
fn to_space(v: [f64; 3], log: [bool; 3]) -> Option<[f64; 3]> {
  let mut out = [0.0; 3];
  for ((o, c), is_log) in out.iter_mut().zip(v).zip(log) {
    *o = if is_log {
      scale::to_log(c)?
    } else {
      c.is_finite().then_some(c)?
    };
  }
  Some(out)
}

/// Whether a coordinate ≤ 0 on a log axis leaves the point out of the plot.
fn log_dropped(v: [f64; 3], log: [bool; 3]) -> bool {
  log.iter().zip(v).any(|(&is_log, c)| is_log && c <= 0.0)
}

/// The points of a series in the space of the box (see `to_space`), in order. A point left out by
/// a log axis gives `Some(None)`, which ends a line; a point that is not finite is skipped.
fn space_points<'a>(
  xy: &'a [PlotPoint],
  z: &'a [f64],
  log: [bool; 3],
) -> impl Iterator<Item = Option<[f64; 3]>> + 'a {
  xy.iter().zip(z).filter_map(move |(p, &zv)| {
    let v = [p.x, p.y, zv];
    if log_dropped(v, log) {
      Some(None)
    } else {
      to_space(v, log).map(Some)
    }
  })
}

/// The runs of consecutive drawable points of a line. A point left out by a log axis ends a run;
/// a point that is not finite is skipped without ending it.
fn line_runs(xy: &[PlotPoint], z: &[f64], log: [bool; 3]) -> Vec<Vec<[f64; 3]>> {
  let mut runs = Vec::new();
  let mut run = Vec::new();
  for point in space_points(xy, z, log) {
    match point {
      Some(v) => run.push(v),
      None => {
        if !run.is_empty() {
          runs.push(std::mem::take(&mut run));
        }
      }
    }
  }
  if !run.is_empty() {
    runs.push(run);
  }
  runs
}

/// Screen position of a point in the space of the box, or None if it is not finite on screen.
fn screen(cam: &Camera, ranges: [(f64, f64); 3], v: [f64; 3]) -> Option<egui::Pos2> {
  let unit: [f32; 3] = std::array::from_fn(|a| to_unit(v[a], ranges[a]));
  let pos = cam.project(unit).0;
  pos.is_finite().then_some(pos)
}

/// Title of an axis: its label, with " (log)" after it on a log axis. An empty label on a log
/// axis gives just "(log)".
fn axis_title(label: &str, log: bool) -> String {
  match (log, label.is_empty()) {
    (false, _) => label.to_owned(),
    (true, true) => "(log)".to_owned(),
    (true, false) => format!("{label} (log)"),
  }
}

/// Spacing of `nice_ticks` over `min..=max`: 1, 2 or 5 times a power of ten, giving about
/// `target` ticks. Zero when there are none.
fn nice_step(min: f64, max: f64, target: usize) -> f64 {
  let (lo, hi) = (min.min(max), min.max(max));
  if target == 0 || !lo.is_finite() || !hi.is_finite() || hi <= lo {
    return 0.0;
  }
  let raw = (hi - lo) / target as f64;
  if !raw.is_finite() || raw <= 0.0 {
    return 0.0;
  }
  let exp = (raw.log10() + TICK_EPS).floor();
  let mag = 10f64.powi(exp as i32);
  let norm = raw / mag;
  let nice = if norm <= 1.0 + TICK_EPS {
    1.0
  } else if norm <= 2.0 + TICK_EPS {
    2.0
  } else if norm <= 5.0 + TICK_EPS {
    5.0
  } else {
    10.0
  };
  let step = nice * mag;
  if step > 0.0 && step.is_finite() {
    step
  } else {
    0.0
  }
}

/// About `target` round values within `min..=max`, in steps of 1, 2 or 5 times a power of ten.
/// Reversed bounds are swapped, and a single value gives itself.
pub(crate) fn nice_ticks(min: f64, max: f64, target: usize) -> Vec<f64> {
  if !min.is_finite() || !max.is_finite() || target == 0 {
    return Vec::new();
  }
  if min == max {
    return vec![min];
  }
  let step = nice_step(min, max, target);
  if step == 0.0 {
    return Vec::new();
  }
  let (lo, hi) = (min.min(max), min.max(max));
  let digits = decimals(step);
  let first = (lo / step - TICK_EPS).ceil() as i64;
  let last = (hi / step + TICK_EPS).floor() as i64;
  (first..=last)
    .map(|k| round_to(k as f64 * step, digits))
    .collect()
}

/// Digits after the point that show every multiple of `step`.
fn decimals(step: f64) -> usize {
  if step <= 0.0 || !step.is_finite() {
    return 0;
  }
  let exp = (step.log10() + TICK_EPS).floor();
  ((-exp).max(0.0) as usize).min(MAX_DECIMALS)
}

/// `v` rounded to `digits` decimals, with no negative zero.
fn round_to(v: f64, digits: usize) -> f64 {
  let scale = 10f64.powi(digits as i32);
  (v * scale).round() / scale + 0.0
}

/// Labels and normalised positions of the ticks of an axis spanning `range`. On a log axis the
/// range is in exponents, and the ticks are labelled with the values they stand for.
fn axis_ticks(range: (f64, f64), log: bool) -> Vec<(String, f32)> {
  if log {
    return scale::log_ticks(range.0, range.1, LOG_TICKS)
      .into_iter()
      .map(|e| (scale::log_label(e), to_unit(e, range)))
      .filter(|(_, pos)| pos.is_finite())
      .collect();
  }
  let digits = decimals(nice_step(range.0, range.1, TICKS));
  nice_ticks(range.0, range.1, TICKS)
    .into_iter()
    .map(|v| (format!("{:.*}", digits, v), to_unit(v, range)))
    .filter(|(_, pos)| pos.is_finite())
    .collect()
}

/// Keeps each point at least `min_gap` points from the last one kept. The first and last
/// points are always kept. Non-finite points are dropped.
pub(crate) fn decimate_line(
  points: impl IntoIterator<Item = egui::Pos2>,
  min_gap: f32,
) -> Vec<egui::Pos2> {
  let mut kept: Vec<egui::Pos2> = Vec::new();
  let mut last = None;
  for p in points.into_iter().filter(|p| p.is_finite()) {
    if kept.last().is_none_or(|k| (p - *k).length() >= min_gap) {
      kept.push(p);
    }
    last = Some(p);
  }
  if let Some(p) = last
    && kept.last() != Some(&p)
  {
    kept.push(p);
  }
  kept
}

/// Keeps the first point in each `cell`-point square of `rect`, and drops the points outside it.
pub(crate) fn decimate_points(
  points: impl IntoIterator<Item = egui::Pos2>,
  rect: egui::Rect,
  cell: f32,
) -> Vec<egui::Pos2> {
  let cell = cell.max(1.0);
  let cols = ((rect.width() / cell).ceil() as usize).max(1);
  let rows = ((rect.height() / cell).ceil() as usize).max(1);
  let mut taken = vec![0u64; (cols * rows).div_ceil(64)];
  let mut kept = Vec::new();
  for p in points {
    if !rect.contains(p) {
      continue;
    }
    let col = (((p.x - rect.min.x) / cell) as usize).min(cols - 1);
    let row = (((p.y - rect.min.y) / cell) as usize).min(rows - 1);
    let index = row * cols + col;
    let (word, bit) = (index / 64, 1u64 << (index % 64));
    if taken[word] & bit == 0 {
      taken[word] |= bit;
      kept.push(p);
    }
  }
  kept
}

/// Id of the legend row of the series at `index`, in the view whose ui has the id `base`.
fn legend_id(base: egui::Id, index: usize) -> egui::Id {
  base.with(("view3d legend", index))
}

/// Rows of the legend in the top-right corner of `view`, one per series.
fn legend_rows(
  painter: &egui::Painter,
  font: &egui::FontId,
  view: egui::Rect,
  series: &[Series3d<'_>],
) -> Vec<egui::Rect> {
  let text: Vec<egui::Vec2> = series
    .iter()
    .map(|s| {
      painter
        .layout_no_wrap(s.name.to_owned(), font.clone(), egui::Color32::WHITE)
        .size()
    })
    .collect();
  let text_h = text.iter().fold(SWATCH, |h, t| h.max(t.y));
  let text_w = text.iter().fold(0.0f32, |w, t| w.max(t.x));
  let row_h = text_h + 2.0 * LEGEND_PAD;
  let row_w = SWATCH + LEGEND_GAP + text_w + 2.0 * LEGEND_PAD;
  let left = view.right() - LEGEND_MARGIN - row_w;
  let top = view.top() + LEGEND_MARGIN;
  (0..series.len())
    .map(|i| {
      let min = egui::pos2(left, top + i as f32 * row_h);
      egui::Rect::from_min_size(min, egui::vec2(row_w, row_h))
    })
    .collect()
}

/// Unit vector from `center` towards `at` on screen (up if they coincide).
fn outward(center: egui::Pos2, at: egui::Pos2) -> egui::Vec2 {
  let away = at - center;
  let len = away.length();
  if len > 1e-3 {
    away / len
  } else {
    egui::vec2(0.0, -1.0)
  }
}

/// Lays out `text` to go `offset` points from `at`, away from `center`, anchored on the side
/// facing away. Returns the galley and the rect it will occupy.
fn label_at(
  painter: &egui::Painter,
  center: egui::Pos2,
  at: egui::Pos2,
  offset: f32,
  text: &str,
  font: &egui::FontId,
  color: egui::Color32,
) -> (Arc<egui::Galley>, egui::Rect) {
  let dir = outward(center, at);
  let side = |v: f32| {
    if v > 0.3 {
      egui::Align::Min
    } else if v < -0.3 {
      egui::Align::Max
    } else {
      egui::Align::Center
    }
  };
  let anchor = egui::Align2([side(dir.x), side(dir.y)]);
  let galley = painter.layout_no_wrap(text.to_owned(), font.clone(), color);
  let rect = anchor.anchor_size(at + dir * offset, galley.size());
  (galley, rect)
}

/// Extent of `size` along the unit direction `dir`, i.e. how far a label of that size reaches
/// outwards from its anchor.
fn extent_along(size: egui::Vec2, dir: egui::Vec2) -> f32 {
  size.x * dir.x.abs() + size.y * dir.y.abs()
}

/// Draws the back panes with their grid, the edges of the back panes, and the tick labels and
/// titles of the axes.
fn draw_box(
  ui: &egui::Ui,
  painter: &egui::Painter,
  cam: &Camera,
  ranges: [(f64, f64); 3],
  log: [bool; 3],
  labels: [&str; 3],
) {
  let visuals = ui.visuals();
  let back = back_sides(cam.d);
  let front = back.map(|side| -side);
  let zedge = leftmost_vertical(cam);
  let ticks: [Vec<(String, f32)>; 3] = std::array::from_fn(|a| axis_ticks(ranges[a], log[a]));
  let titles: [String; 3] = std::array::from_fn(|a| axis_title(labels[a], log[a]));
  let pos = |p: [f32; 3]| cam.project(p).0;
  let grid = egui::Stroke::new(0.5, visuals.widgets.noninteractive.bg_stroke.color);

  for (a, &side) in back.iter().enumerate() {
    let (b, c) = ((a + 1) % 3, (a + 2) % 3);
    let corners = [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)]
      .map(|(vb, vc)| pos(place([(a, side), (b, vb), (c, vc)])));
    let fill =
      egui::Shape::convex_polygon(corners.to_vec(), visuals.faint_bg_color, egui::Stroke::NONE);
    painter.add(fill);
    for (_, n) in &ticks[b] {
      let from = pos(place([(a, side), (b, *n), (c, -1.0)]));
      let to = pos(place([(a, side), (b, *n), (c, 1.0)]));
      painter.line_segment([from, to], grid);
    }
    for (_, n) in &ticks[c] {
      let from = pos(place([(a, side), (b, -1.0), (c, *n)]));
      let to = pos(place([(a, side), (b, 1.0), (c, *n)]));
      painter.line_segment([from, to], grid);
    }
  }

  let edge = visuals.widgets.noninteractive.bg_stroke;
  for (p, q) in back_edges(back) {
    painter.line_segment([pos(p), pos(q)], edge);
  }

  let center = cam.rect.center();
  let small = egui::TextStyle::Small.resolve(ui.style());
  let body = egui::TextStyle::Body.resolve(ui.style());
  let color = visuals.text_color();
  // Where two axes end on the same box corner their end labels would print over each other, so
  // a label that would overlap one already drawn is left out. Log axes go first, so that their
  // decade labels win.
  let mut taken: Vec<egui::Rect> = Vec::new();
  let mut order = [0, 1, 2];
  order.sort_by_key(|&a| !log[a]);
  // How far each axis's tick labels reach outwards, so that its title clears them.
  let mut reach = [0.0f32; 3];
  for a in order {
    for (text, n) in &ticks[a] {
      let at = pos(tick_edge(a, *n, front, zedge));
      let (galley, rect) = label_at(painter, center, at, TICK_OFFSET, text, &small, color);
      reach[a] = reach[a].max(extent_along(galley.size(), outward(center, at)));
      if taken.iter().any(|r| r.intersects(rect.shrink(1.0))) {
        continue;
      }
      painter.galley(rect.min, galley, color);
      taken.push(rect);
    }
  }
  for a in 0..3 {
    if !titles[a].is_empty() {
      let at = pos(tick_edge(a, 0.0, front, zedge));
      let offset = TITLE_OFFSET.max(TICK_OFFSET + reach[a] + TITLE_GAP);
      let (galley, rect) = label_at(painter, center, at, offset, &titles[a], &body, color);
      painter.galley(rect.min, galley, color);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// `n` points of a ramp along the diagonal, as xy and z.
  fn ramp(n: usize) -> (Vec<PlotPoint>, Vec<f64>) {
    let xy = (0..n).map(|i| PlotPoint::new(i as f64, i as f64)).collect();
    let z = (0..n).map(|i| i as f64).collect();
    (xy, z)
  }

  /// Size of the screen the view is drawn on.
  const SCREEN: egui::Vec2 = egui::vec2(900.0, 560.0);

  /// Time between frames, as at 60 Hz.
  const FRAME_TIME: f64 = 1.0 / 60.0;

  /// Azimuth and elevation, in degrees, of the camera angles that the projection tests cover.
  const ANGLES: [(f32, f32); 6] = [
    (0.0, 0.0),
    (-60.0, 30.0),
    (45.0, -70.0),
    (120.0, 89.0),
    (-135.0, 10.0),
    (30.0, -89.0),
  ];

  /// xorshift64, so that the tests need no random number crate.
  struct Rng(u64);

  impl Rng {
    /// A number in `[0, 1)`.
    fn unit(&mut self) -> f64 {
      self.0 ^= self.0 << 13;
      self.0 ^= self.0 >> 7;
      self.0 ^= self.0 << 17;
      (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
  }

  /// The 8 corners of the box.
  fn corners() -> [[f32; 3]; 8] {
    std::array::from_fn(|i| {
      let sign = |bit: usize| if i & bit == 0 { -1.0 } else { 1.0 };
      [sign(1), sign(2), sign(4)]
    })
  }

  /// Points of the Lorenz attractor, from RK4 steps of 0.01.
  fn lorenz(n: usize) -> (Vec<PlotPoint>, Vec<f64>) {
    let rhs = |[x, y, z]: [f64; 3]| [10.0 * (y - x), x * (28.0 - z) - y, x * y - 8.0 / 3.0 * z];
    let advance =
      |s: [f64; 3], k: [f64; 3], h: f64| [s[0] + h * k[0], s[1] + h * k[1], s[2] + h * k[2]];
    let h = 0.01;
    let mut s = [1.0, 1.0, 1.0];
    let (mut xy, mut z) = (Vec::with_capacity(n), Vec::with_capacity(n));
    for _ in 0..n {
      let k1 = rhs(s);
      let k2 = rhs(advance(s, k1, h / 2.0));
      let k3 = rhs(advance(s, k2, h / 2.0));
      let k4 = rhs(advance(s, k3, h));
      for (i, v) in s.iter_mut().enumerate() {
        *v += h / 6.0 * (k1[i] + 2.0 * k2[i] + 2.0 * k3[i] + k4[i]);
      }
      xy.push(PlotPoint::new(s[0], s[1]));
      z.push(s[2]);
    }
    (xy, z)
  }

  fn view_rect() -> egui::Rect {
    egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(300.0, 200.0))
  }

  #[test]
  fn nice_ticks_of_the_unit_range() {
    let ticks = nice_ticks(0.0, 1.0, 5);
    let want = [0.0, 0.2, 0.4, 0.6, 0.8, 1.0];
    assert_eq!(ticks.len(), want.len(), "{ticks:?}");
    for (t, w) in ticks.iter().zip(want) {
      assert!((t - w).abs() < 1e-12, "{ticks:?}");
    }
  }

  #[test]
  fn nice_ticks_are_multiples_of_five_in_a_wide_range() {
    let ticks = nice_ticks(-3.7, 12.1, 5);
    assert_eq!(ticks.len(), 3, "{ticks:?}");
    for (t, w) in ticks.iter().zip([0.0, 5.0, 10.0]) {
      assert!((t - w).abs() < 1e-12, "{ticks:?}");
    }
  }

  #[test]
  fn nice_ticks_stay_in_range_for_many_ranges() {
    let ranges = [
      (0.0, 1.0),
      (-3.7, 12.1),
      (1e-6, 3e-6),
      (-2e9, 7e8),
      (0.3, 0.7),
      (99.5, 100.5),
    ];
    for (lo, hi) in ranges {
      let ticks = nice_ticks(lo, hi, 5);
      assert!(!ticks.is_empty(), "no ticks in {lo}..{hi}");
      assert!(ticks.len() <= 7, "{} ticks in {lo}..{hi}", ticks.len());
      let eps = 1e-9 * (hi - lo);
      for t in &ticks {
        assert!(*t >= lo - eps && *t <= hi + eps, "{t} outside {lo}..{hi}");
      }
    }
  }

  #[test]
  fn nice_ticks_survive_degenerate_and_reversed_input() {
    assert_eq!(nice_ticks(1.0, 0.0, 5), nice_ticks(0.0, 1.0, 5));
    assert_eq!(nice_ticks(2.0, 2.0, 5), vec![2.0]);
    assert!(nice_ticks(0.0, 1.0, 0).is_empty());
    assert!(nice_ticks(f64::NAN, 1.0, 5).is_empty());
    assert!(nice_ticks(0.0, f64::INFINITY, 5).is_empty());
    assert!(nice_ticks(-1e308, 1e308, 5).is_empty());
  }

  #[test]
  fn default_camera_puts_y_right_z_up_and_x_nearest() {
    let rect = view_rect();
    let cam = Camera::new(rect, 0.0, 0.0, 1.0, false);
    let c = rect.center();
    let (x, depth_x) = cam.project([1.0, 0.0, 0.0]);
    let (y, depth_y) = cam.project([0.0, 1.0, 0.0]);
    let (z, depth_z) = cam.project([0.0, 0.0, 1.0]);
    assert!(y.x > c.x, "+y at {y:?}, centre {c:?}");
    assert!(z.y < c.y, "+z at {z:?}, centre {c:?}");
    assert!((x - c).length() < 1e-3, "+x at {x:?}, centre {c:?}");
    assert!(depth_x > depth_y && depth_x > depth_z);
  }

  #[test]
  fn all_corners_fit_the_view_at_zoom_one() {
    for rect in [
      view_rect(),
      egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(200.0, 300.0)),
    ] {
      let inner = rect.expand(1e-3);
      for perspective in [false, true] {
        for (az, el) in ANGLES {
          let cam = Camera::new(rect, az.to_radians(), el.to_radians(), 1.0, perspective);
          for corner in corners() {
            let p = cam.project(corner).0;
            assert!(
              inner.contains(p),
              "corner {corner:?} at {p:?} outside {rect:?} (perspective {perspective}, az {az}, el {el})"
            );
          }
        }
      }
    }
  }

  #[test]
  fn back_panes_face_away_from_the_camera() {
    for (az, el) in [
      (-60.0, 30.0),
      (30.0, -20.0),
      (170.0, 60.0),
      (-100.0, 75.0),
      (45.0, -45.0),
    ] {
      let d = Camera::new(
        view_rect(),
        f32::to_radians(az),
        f32::to_radians(el),
        1.0,
        false,
      )
      .d;
      let back = back_sides(d);
      for (axis, (&side, &c)) in back.iter().zip(&d).enumerate() {
        // The chosen face is the one at -sign(d), so its outward normal points away.
        assert_eq!(side < 0.0, c > 0.0, "axis {axis} at az {az}, el {el}");
        assert!(side * c < 0.0, "axis {axis} at az {az}, el {el}");
      }
    }
  }

  #[test]
  fn line_decimation_drops_duplicates_and_keeps_both_ends() {
    let p = egui::pos2(0.0, 0.0);
    let q = egui::pos2(5.0, 0.0);
    let kept = decimate_line([p, p, p, q], MIN_LINE_GAP);
    assert_eq!(kept, vec![p, q]);
  }

  #[test]
  fn line_decimation_keeps_a_last_point_that_is_close() {
    let a = egui::pos2(0.0, 0.0);
    let b = egui::pos2(1.0, 0.0);
    let c = egui::pos2(1.1, 0.0);
    assert_eq!(decimate_line([a, b, c], 0.5), vec![a, b, c]);
  }

  #[test]
  fn line_decimation_spaces_the_kept_vertices() {
    let input: Vec<egui::Pos2> = (0..=1000)
      .map(|i| egui::pos2(i as f32 * 0.01, 0.0))
      .collect();
    let kept = decimate_line(input.iter().copied(), 0.5);
    assert_eq!(kept.first(), input.first());
    assert_eq!(kept.last(), input.last());
    assert!(kept.len() <= 23, "{} vertices kept", kept.len());
    for pair in kept[..kept.len() - 1].windows(2) {
      assert!((pair[1] - pair[0]).length() >= 0.5 - 1e-4);
    }
  }

  #[test]
  fn point_decimation_keeps_the_first_point_of_each_cell() {
    let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(100.0, 100.0));
    let p = egui::pos2(12.0, 12.0);
    assert_eq!(decimate_points(vec![p; 1000], rect, 5.0), vec![p]);
    let first = egui::pos2(11.0, 11.0);
    let kept = decimate_points([first, egui::pos2(14.0, 14.0)], rect, 5.0);
    assert_eq!(kept, vec![first]);
  }

  #[test]
  fn point_decimation_bounds_the_count_by_cells_and_drops_outside_points() {
    let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(100.0, 100.0));
    let mut rng = Rng(7);
    let points: Vec<egui::Pos2> = (0..10_000)
      .map(|_| {
        egui::pos2(
          rng.unit() as f32 * 120.0 - 10.0,
          rng.unit() as f32 * 120.0 - 10.0,
        )
      })
      .collect();
    let kept = decimate_points(points.iter().copied(), rect, 5.0);
    assert!(kept.len() > 100 && kept.len() <= 400, "{} kept", kept.len());
    assert!(kept.iter().all(|p| rect.contains(*p)));
    let outside = [
      egui::pos2(-1.0, 50.0),
      egui::pos2(101.0, 50.0),
      egui::pos2(50.0, -1.0),
      egui::pos2(50.0, 101.0),
    ];
    assert!(decimate_points(outside, rect, 5.0).is_empty());
  }

  #[test]
  fn ranges_cover_the_visible_points_and_widen_flat_axes() {
    let xy = [PlotPoint::new(1.0, 2.0), PlotPoint::new(3.0, 2.0)];
    let z = [5.0, 5.0];
    let series = [Series3d {
      name: "a",
      xy: &xy,
      z: &z,
      color: egui::Color32::RED,
      lines: true,
      points: false,
    }];
    let none = HashSet::new();
    let r = data_ranges(&series, &none, [false; 3]);
    assert_eq!(r, [(1.0, 3.0), (1.5, 2.5), (4.5, 5.5)]);

    let hidden = HashSet::from(["a".to_owned()]);
    assert_eq!(data_ranges(&series, &hidden, [false; 3]), [(0.0, 1.0); 3]);
    assert_eq!(data_ranges(&[], &none, [false; 3]), [(0.0, 1.0); 3]);
  }

  #[test]
  fn to_unit_maps_the_range_onto_the_box() {
    let range = (10.0, 30.0);
    assert!((to_unit(10.0, range) + 1.0).abs() < 1e-6);
    assert!((to_unit(20.0, range)).abs() < 1e-6);
    assert!((to_unit(30.0, range) - 1.0).abs() < 1e-6);
  }

  #[test]
  fn reset_camera_keeps_the_display_settings() {
    let mut view = View3d {
      azimuth: 1.0,
      elevation: -0.5,
      zoom: 4.0,
      perspective: true,
      point_radius: 5.0,
      line_width: 3.0,
      legend: false,
      ..View3d::default()
    };
    view.hidden.insert("a".to_owned());
    view.reset_camera();
    let default = View3d::default();
    assert_eq!(view.azimuth, default.azimuth);
    assert_eq!(view.elevation, default.elevation);
    assert_eq!(view.zoom, default.zoom);
    assert!(view.perspective);
    assert_eq!(view.point_radius, 5.0);
    assert!(!view.legend);
    assert!(view.hidden.contains("a"));
  }

  /// One frame of `view` drawing `series`, with `events` as input. Returns what it drew and the
  /// id of its ui.
  fn frame(
    ctx: &egui::Context,
    view: &mut View3d,
    series: &[Series3d<'_>],
    events: Vec<egui::Event>,
  ) -> (Drawn, egui::Id) {
    let input = egui::RawInput {
      screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SCREEN)),
      time: Some(ctx.input(|i| i.time) + FRAME_TIME),
      predicted_dt: FRAME_TIME as f32,
      events,
      ..Default::default()
    };
    let mut out = None;
    let mut output = ctx.run_ui(input, |ui| {
      egui::CentralPanel::default().show(ui, |ui| {
        let id = ui.id();
        out = Some((view.show(ui, series, ["x", "y", "z"]), id));
      });
    });
    // No renderer here, so the texture uploads are dropped unused.
    output.textures_delta.clear();
    out.expect("the frame drew the view")
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
  fn click(ctx: &egui::Context, view: &mut View3d, series: &[Series3d<'_>], pos: egui::Pos2) {
    frame(ctx, view, series, vec![pointer_moved(pos)]);
    frame(
      ctx,
      view,
      series,
      vec![pointer_moved(pos), pointer_button(pos, true)],
    );
    frame(ctx, view, series, vec![pointer_button(pos, false)]);
  }

  /// Drags from `start` by `delta` in `steps` moves, then releases.
  fn drag(
    ctx: &egui::Context,
    view: &mut View3d,
    series: &[Series3d<'_>],
    start: egui::Pos2,
    delta: egui::Vec2,
    steps: usize,
  ) {
    frame(ctx, view, series, vec![pointer_moved(start)]);
    frame(
      ctx,
      view,
      series,
      vec![pointer_moved(start), pointer_button(start, true)],
    );
    for i in 1..=steps {
      let pos = start + delta * (i as f32 / steps as f32);
      frame(ctx, view, series, vec![pointer_moved(pos)]);
    }
    frame(
      ctx,
      view,
      series,
      vec![pointer_button(start + delta, false)],
    );
  }

  /// Scrolls by `dy` points over the view, then lets the smoothed scroll settle.
  fn scroll(ctx: &egui::Context, view: &mut View3d, series: &[Series3d<'_>], dy: f32) {
    let wheel = egui::Event::MouseWheel {
      unit: egui::MouseWheelUnit::Point,
      delta: egui::vec2(0.0, dy),
      phase: egui::TouchPhase::Move,
      modifiers: egui::Modifiers::NONE,
    };
    frame(ctx, view, series, vec![wheel]);
    for _ in 0..30 {
      frame(ctx, view, series, Vec::new());
    }
  }

  #[test]
  fn trajectory_and_cloud_draw_with_marks_within_total() {
    let ctx = egui::Context::default();
    let (lxy, lz) = lorenz(5000);
    let mut rng = Rng(42);
    let cxy: Vec<PlotPoint> = (0..20_000)
      .map(|_| PlotPoint::new(rng.unit(), rng.unit()))
      .collect();
    let cz: Vec<f64> = (0..20_000).map(|_| rng.unit()).collect();
    let series = [
      Series3d {
        name: "lorenz",
        xy: &lxy,
        z: &lz,
        color: egui::Color32::from_rgb(90, 160, 220),
        lines: true,
        points: false,
      },
      Series3d {
        name: "cloud",
        xy: &cxy,
        z: &cz,
        color: egui::Color32::from_rgb(230, 120, 80),
        lines: false,
        points: true,
      },
    ];
    for perspective in [false, true] {
      let mut view = View3d {
        perspective,
        ..View3d::default()
      };
      let (drawn, _) = frame(&ctx, &mut view, &series, Vec::new());
      assert_eq!(drawn.total, 25_000);
      assert!(
        drawn.marks > 0 && drawn.marks <= drawn.total,
        "{} marks",
        drawn.marks
      );
    }
  }

  #[test]
  fn drag_turns_the_camera_and_clamps_the_elevation() {
    let ctx = egui::Context::default();
    let (xy, z) = ramp(10);
    let series = [Series3d {
      name: "ramp",
      xy: &xy,
      z: &z,
      color: egui::Color32::GREEN,
      lines: true,
      points: false,
    }];
    let mut view = View3d::default();
    let (drawn, _) = frame(&ctx, &mut view, &series, Vec::new());
    let start = drawn.response.rect.center();

    let (az, el) = (view.azimuth, view.elevation);
    drag(
      &ctx,
      &mut view,
      &series,
      start,
      egui::vec2(100.0, -50.0),
      10,
    );
    assert!(
      (view.azimuth - (az - 1.0)).abs() < 1e-3,
      "azimuth {}",
      view.azimuth
    );
    assert!(
      (view.elevation - (el - 0.5)).abs() < 1e-3,
      "elevation {}",
      view.elevation
    );

    let max = MAX_ELEVATION_DEG.to_radians();
    for _ in 0..20 {
      drag(&ctx, &mut view, &series, start, egui::vec2(0.0, 250.0), 5);
    }
    assert!(
      (view.elevation - max).abs() < 1e-5,
      "elevation {}",
      view.elevation
    );
    for _ in 0..40 {
      drag(&ctx, &mut view, &series, start, egui::vec2(0.0, -250.0), 5);
    }
    assert!(
      (view.elevation + max).abs() < 1e-5,
      "elevation {}",
      view.elevation
    );
  }

  #[test]
  fn scrolling_over_the_view_zooms() {
    let ctx = egui::Context::default();
    let (xy, z) = ramp(10);
    let series = [Series3d {
      name: "ramp",
      xy: &xy,
      z: &z,
      color: egui::Color32::GREEN,
      lines: true,
      points: false,
    }];
    let mut view = View3d::default();
    let (drawn, _) = frame(&ctx, &mut view, &series, Vec::new());
    let center = drawn.response.rect.center();
    frame(&ctx, &mut view, &series, vec![pointer_moved(center)]);

    scroll(&ctx, &mut view, &series, 60.0);
    assert!(view.zoom > 1.05, "zoom {}", view.zoom);
    let zoomed = view.zoom;
    scroll(&ctx, &mut view, &series, -200.0);
    assert!(view.zoom < zoomed, "zoom {}", view.zoom);
  }

  #[test]
  fn double_click_resets_the_camera() {
    let ctx = egui::Context::default();
    let (xy, z) = ramp(10);
    let series = [Series3d {
      name: "ramp",
      xy: &xy,
      z: &z,
      color: egui::Color32::GREEN,
      lines: true,
      points: false,
    }];
    let mut view = View3d {
      azimuth: 1.0,
      elevation: -0.4,
      zoom: 3.0,
      perspective: true,
      ..View3d::default()
    };
    let (drawn, _) = frame(&ctx, &mut view, &series, Vec::new());
    let center = drawn.response.rect.center();

    click(&ctx, &mut view, &series, center);
    assert_eq!(view.azimuth, 1.0, "a single click must not reset");

    click(&ctx, &mut view, &series, center);
    let default = View3d::default();
    assert_eq!(view.azimuth, default.azimuth);
    assert_eq!(view.elevation, default.elevation);
    assert_eq!(view.zoom, default.zoom);
    assert!(
      view.perspective,
      "the projection is a display setting and stays"
    );
  }

  #[test]
  fn clicking_a_legend_row_hides_and_shows_its_series() {
    let ctx = egui::Context::default();
    let (line_xy, line_z) = ramp(100);
    let (cloud_xy, cloud_z) = ramp(50);
    let series = [
      Series3d {
        name: "line",
        xy: &line_xy,
        z: &line_z,
        color: egui::Color32::RED,
        lines: true,
        points: false,
      },
      Series3d {
        name: "cloud",
        xy: &cloud_xy,
        z: &cloud_z,
        color: egui::Color32::BLUE,
        lines: false,
        points: true,
      },
    ];
    let mut view = View3d::default();
    let (drawn, base) = frame(&ctx, &mut view, &series, Vec::new());
    assert_eq!(drawn.total, 150);

    let row = ctx
      .read_response(legend_id(base, 0))
      .expect("the legend row of the line was drawn")
      .rect
      .center();
    click(&ctx, &mut view, &series, row);
    let (drawn, _) = frame(&ctx, &mut view, &series, Vec::new());
    assert_eq!(drawn.total, 50);
    assert!(view.hidden.contains("line"));

    click(&ctx, &mut view, &series, row);
    let (drawn, _) = frame(&ctx, &mut view, &series, Vec::new());
    assert_eq!(drawn.total, 150);
    assert!(view.hidden.is_empty());
  }

  #[test]
  fn a_million_points_are_bounded_by_the_cells() {
    let ctx = egui::Context::default();
    let mut rng = Rng(1);
    let n = 1_000_000;
    let xy: Vec<PlotPoint> = (0..n)
      .map(|_| PlotPoint::new(rng.unit(), rng.unit()))
      .collect();
    let z: Vec<f64> = (0..n).map(|_| rng.unit()).collect();
    let series = [Series3d {
      name: "cloud",
      xy: &xy,
      z: &z,
      color: egui::Color32::from_rgb(120, 200, 120),
      lines: false,
      points: true,
    }];
    let mut view = View3d::default();
    let (drawn, _) = frame(&ctx, &mut view, &series, Vec::new());
    let rect = drawn.response.rect;
    let cell = view.point_radius.max(1.0);
    let cols = (rect.width() / cell).ceil() as usize;
    let rows = (rect.height() / cell).ceil() as usize;
    assert_eq!(drawn.total, n);
    assert!(
      drawn.marks > 0 && drawn.marks <= cols * rows,
      "{} marks for {} cells",
      drawn.marks,
      cols * rows
    );
  }

  fn near(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
  }

  #[test]
  fn log_bounds_are_exponents_of_the_drawable_values() {
    // z spans 1 to 1e6, and the zero is left out of the bounds.
    let xy = vec![PlotPoint::new(1.0, 1.0); 4];
    let z = [1.0, 1e3, 0.0, 1e6];
    let series = [Series3d {
      name: "a",
      xy: &xy,
      z: &z,
      color: egui::Color32::RED,
      lines: true,
      points: false,
    }];
    let r = data_ranges(&series, &HashSet::new(), [false, false, true]);
    assert!(near(r[2].0, 0.0) && near(r[2].1, 6.0), "{:?}", r[2]);

    // A single value is widened by half a decade either way, as on a linear axis.
    let z = [100.0; 4];
    let series = [Series3d {
      name: "a",
      xy: &xy,
      z: &z,
      color: egui::Color32::RED,
      lines: true,
      points: false,
    }];
    let r = data_ranges(&series, &HashSet::new(), [false, false, true]);
    assert!(near(r[2].0, 1.5) && near(r[2].1, 2.5), "{:?}", r[2]);
  }

  #[test]
  fn log_axis_ticks_are_decades_labelled_as_values() {
    let ticks = axis_ticks((0.0, 6.0), true);
    let labels: Vec<&str> = ticks.iter().map(|(l, _)| l.as_str()).collect();
    assert_eq!(labels, ["1", "10", "100", "1000", "1e4", "1e5", "1e6"]);
    assert!(near(f64::from(ticks[0].1), -1.0));
    assert!(near(f64::from(ticks[ticks.len() - 1].1), 1.0));

    // A span within one decade gets the 1 to 9 subdivisions.
    let narrow = axis_ticks((2f64.log10(), 8f64.log10()), true);
    let labels: Vec<&str> = narrow.iter().map(|(l, _)| l.as_str()).collect();
    assert_eq!(labels, ["2", "3", "4", "5", "6", "7", "8"]);

    // Linear axes keep their round values.
    let linear = axis_ticks((0.0, 1.0), false);
    assert_eq!(linear.len(), 6);
    assert_eq!(linear[0].0, "0.0");
  }

  #[test]
  fn log_axis_titles_say_log() {
    assert_eq!(axis_title("z", true), "z (log)");
    assert_eq!(axis_title("", true), "(log)");
    assert_eq!(axis_title("z", false), "z");
    assert_eq!(axis_title("", false), "");
  }

  #[test]
  fn dropped_counts_the_points_left_out_by_a_log_axis() {
    let ctx = egui::Context::default();
    let (xy, _) = ramp(5);
    let z = [1.0, 0.0, 10.0, -2.0, 100.0];
    let series = [Series3d {
      name: "a",
      xy: &xy,
      z: &z,
      color: egui::Color32::RED,
      lines: false,
      points: true,
    }];
    let mut view = View3d {
      log: [false, false, true],
      ..View3d::default()
    };
    let (drawn, _) = frame(&ctx, &mut view, &series, Vec::new());
    assert_eq!((drawn.total, drawn.dropped), (5, 2));

    // The point at x = 0 is left out when x is a log axis.
    view.log = [true, false, false];
    let (drawn, _) = frame(&ctx, &mut view, &series, Vec::new());
    assert_eq!(drawn.dropped, 1);

    view.log = [false; 3];
    let (drawn, _) = frame(&ctx, &mut view, &series, Vec::new());
    assert_eq!(drawn.dropped, 0);
  }

  #[test]
  fn a_non_positive_point_splits_a_line_into_runs() {
    let log = [false, false, true];
    let (xy, _) = ramp(5);
    let z = [1.0, 10.0, 0.0, 100.0, 1000.0];
    let lens: Vec<usize> = line_runs(&xy, &z, log).iter().map(Vec::len).collect();
    assert_eq!(lens, [2, 2]);
    // Without a log axis nothing is left out.
    assert_eq!(line_runs(&xy, &z, [false; 3]).len(), 1);

    // A non-positive point at either end only shortens the line.
    let (xy4, _) = ramp(4);
    assert_eq!(line_runs(&xy4, &[0.0, 1.0, 10.0, 0.0], log).len(), 1);

    // A point that is not finite is skipped, and the line goes on across it.
    let (xy3, _) = ramp(3);
    let runs = line_runs(&xy3, &[1.0, f64::NAN, 10.0], log);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].len(), 2);
  }

  #[test]
  fn log_axes_with_nothing_drawable_draw_an_empty_box() {
    let ctx = egui::Context::default();
    let (xy, _) = ramp(10);
    let z = [-1.0; 10];
    let series = [Series3d {
      name: "a",
      xy: &xy,
      z: &z,
      color: egui::Color32::RED,
      lines: true,
      points: true,
    }];
    let mut view = View3d {
      log: [true; 3],
      ..View3d::default()
    };
    let (drawn, _) = frame(&ctx, &mut view, &series, Vec::new());
    assert_eq!((drawn.marks, drawn.dropped), (0, 10));
  }

  #[test]
  fn log_axes_draw_mixed_data_within_the_totals() {
    let ctx = egui::Context::default();
    let (lxy, lz) = lorenz(5000);
    let mut rng = Rng(9);
    let mut cxy: Vec<PlotPoint> = (0..20_000)
      .map(|_| PlotPoint::new(rng.unit() * 2.0 - 1.0, rng.unit() * 2.0 - 1.0))
      .collect();
    let mut cz: Vec<f64> = (0..20_000).map(|_| rng.unit() * 2.0 - 1.0).collect();
    // Not finite: skipped as before, and not counted as dropped.
    cxy[0] = PlotPoint::new(f64::NAN, 0.5);
    cz[1] = f64::INFINITY;
    let series = [
      Series3d {
        name: "lorenz",
        xy: &lxy,
        z: &lz,
        color: egui::Color32::from_rgb(90, 160, 220),
        lines: true,
        points: false,
      },
      Series3d {
        name: "cloud",
        xy: &cxy,
        z: &cz,
        color: egui::Color32::from_rgb(230, 120, 80),
        lines: false,
        points: true,
      },
    ];
    let non_positive = |v: [f64; 3]| v.iter().any(|&c| c <= 0.0);
    let expected: usize = lxy
      .iter()
      .zip(&lz)
      .filter(|&(p, &z)| non_positive([p.x, p.y, z]))
      .count()
      + cxy
        .iter()
        .zip(&cz)
        .filter(|&(p, &z)| non_positive([p.x, p.y, z]))
        .count();

    let mut view = View3d {
      log: [true; 3],
      ..View3d::default()
    };
    for perspective in [false, true] {
      view.perspective = perspective;
      let (drawn, _) = frame(&ctx, &mut view, &series, Vec::new());
      assert_eq!(drawn.total, 25_000);
      assert_eq!(drawn.dropped, expected);
      assert!(
        drawn.marks > 0 && drawn.marks <= drawn.total,
        "{} marks",
        drawn.marks
      );
    }
  }

  /// Renders a helix with a log z axis to the file named by `LOG3D_PNG`, or to the temp dir, for
  /// a person to look at. Run with `cargo test log3d_snapshot -- --ignored --nocapture`.
  #[test]
  #[ignore = "renders a PNG for a person to look at"]
  fn log3d_snapshot() {
    // Three turns while z climbs from 1 to 1e6: the helix is evenly spaced on the log axis.
    let t: Vec<f64> = (0..=240).map(|i| f64::from(i) / 20.0).collect();
    let xy: Vec<PlotPoint> = t
      .iter()
      .map(|&t| {
        let a = t * std::f64::consts::TAU / 4.0;
        PlotPoint::new(a.cos(), a.sin())
      })
      .collect();
    let z: Vec<f64> = t.iter().map(|&t| 10f64.powf(t / 2.0)).collect();
    let series = [Series3d {
      name: "helix",
      xy: &xy,
      z: &z,
      color: egui::Color32::from_rgb(90, 160, 220),
      lines: true,
      points: false,
    }];
    let mut view = View3d {
      log: [false, false, true],
      ..View3d::default()
    };
    let img = crate::snapshot::snapshot(egui::vec2(900.0, 560.0), 2.0, 10, |ui| {
      egui::CentralPanel::default().show(ui, |ui| {
        view.show(ui, &series, ["x", "y", "z"]);
      });
    });
    let path = std::env::var_os("LOG3D_PNG")
      .map(std::path::PathBuf::from)
      .unwrap_or_else(|| std::env::temp_dir().join("complot-log3d.png"));
    img.write_png(&path).expect("PNG written");
    println!("wrote {}", path.display());
  }
}
