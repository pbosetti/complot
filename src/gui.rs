//! The plot window: a side panel with settings and the plot itself.

use crate::lod::{decimate_line, decimate_points};
use crate::model::{PlotState, Series};
use eframe::egui;
use egui_plot::{Legend, Line, Plot, PlotPoint, PlotPoints, Points};
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

/// Which marks are drawn for each series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Geometry {
  Points,
  Lines,
  Both,
}

impl Geometry {
  fn has_points(self) -> bool {
    self != Geometry::Lines
  }

  fn has_lines(self) -> bool {
    self != Geometry::Points
  }
}

/// Settings and view state of one window. Settings live in memory only.
struct PlotApp {
  name: String,
  state: Arc<Mutex<PlotState>>,
  geometry: Geometry,
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
}

/// Locks the plot state. A poisoned lock is still used, since a panic cannot leave the data invalid.
fn lock(state: &Mutex<PlotState>) -> MutexGuard<'_, PlotState> {
  state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Colour of series `i`, cycling through [`PALETTE`].
fn palette(i: usize) -> egui::Color32 {
  PALETTE[i % PALETTE.len()]
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

impl PlotApp {
  fn new(name: &str, state: Arc<Mutex<PlotState>>) -> Self {
    let max_points = lock(&state).max_points;
    PlotApp {
      name: name.to_string(),
      state,
      geometry: Geometry::Both,
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
    }
  }

  /// The side panel with the settings and the status line.
  fn side_panel(&mut self, ui: &mut egui::Ui) {
    egui::Panel::left("settings")
      .resizable(true)
      .default_size(190.0)
      .show(ui, |ui| {
        ui.heading(self.name.as_str());
        ui.add_space(4.0);

        ui.group(|ui| {
          ui.label(egui::RichText::new("Geometry").strong());
          ui.radio_value(&mut self.geometry, Geometry::Points, "Points");
          ui.radio_value(&mut self.geometry, Geometry::Lines, "Lines");
          ui.radio_value(&mut self.geometry, Geometry::Both, "Both");
          ui.add_enabled(
            self.geometry.has_points(),
            egui::Slider::new(&mut self.point_radius, 0.5..=10.0).text("point size"),
          );
          ui.add_enabled(
            self.geometry.has_lines(),
            egui::Slider::new(&mut self.line_width, 0.5..=8.0).text("line width"),
          );
        });
        ui.checkbox(&mut self.legend, "Legend");
        ui.checkbox(&mut self.grid, "Grid");

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

        ui.group(|ui| {
          ui.label(egui::RichText::new("Buffer").strong());
          ui.horizontal(|ui| {
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
        });

        ui.group(|ui| {
          ui.label(egui::RichText::new("Data").strong());
          ui.horizontal(|ui| {
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
        let status = if skipped > 0 {
          format!("{series} series · {points} points · {skipped} skipped")
        } else {
          format!("{series} series · {points} points")
        };
        let status = match self.decimated {
          Some((drawn, total)) => format!("{status} · drawing {drawn} of {total}"),
          None => status,
        };
        ui.label(status);
      });
  }

  /// The plot, filling the rest of the window.
  fn plot(&mut self, ui: &mut egui::Ui) {
    // One lock for the whole frame; network threads hold it only briefly.
    let mut guard = lock(&self.state);
    for s in guard.series.iter_mut() {
      s.points.make_contiguous();
    }
    let st: &PlotState = &guard;
    let empty = st.total_points() == 0;
    let total = st.total_points();

    // A request from the panel is applied inside the plot, which then reports the follow state.
    let fit = std::mem::take(&mut self.fit_request);
    let toggle = self.follow_request.take();
    let request = if fit { Some(true) } else { toggle };

    let (geometry, radius, width) = (self.geometry, self.point_radius, self.line_width);
    let (legend, grid) = (self.legend, self.grid);
    let name = &self.name;
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
        let auto: egui::Vec2b = match request {
          Some(on) => {
            plot_ui.set_auto_bounds(on);
            on.into()
          }
          None => plot_ui.auto_bounds(),
        };
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
        let as_line = |s: &Series| geometry.has_lines() && (s.sorted || !big(s));
        let as_points = |s: &Series| geometry.has_points() || !as_line(s);

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

      if empty {
        ui.painter().text(
          response.response.rect.center(),
          egui::Align2::CENTER_CENTER,
          format!("waiting for data…  complot {name} -y 1"),
          egui::FontId::proportional(16.0),
          ui.visuals().weak_text_color(),
        );
      }
    });

    if let Some(on) = follow {
      self.follow = on;
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
    self.side_panel(ui);
    self.plot(ui);
  }
}
