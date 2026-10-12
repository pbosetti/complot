//! Level of detail: picks the points to draw from many stored points.
//!
//! Pure functions, no GUI state, so that they can be unit tested. The stored
//! data is never changed; only the points handed to the plot are reduced.

use egui_plot::PlotPoint;

/// Reduces an x-sorted line to the M4 points of `columns` buckets over `[x0, x1]`.
///
/// Keeps the points with x in `[x0, x1]`, plus the nearest point just outside
/// each side so that the line reaches the edges. Each non-empty bucket keeps
/// its first, min-y, max-y and last point, in data order and without
/// duplicates. The output is x-sorted and has at most `4 * columns + 2` points.
/// Bounds given in reverse order are swapped; `columns == 0` counts as 1.
pub fn decimate_line(points: &[PlotPoint], x0: f64, x1: f64, columns: usize) -> Vec<PlotPoint> {
  let columns = columns.max(1);
  let (x0, x1) = (x0.min(x1), x0.max(x1));
  let lo = points.partition_point(|p| p.x < x0);
  let hi = points.partition_point(|p| p.x <= x1);
  let mut out = Vec::new();
  if lo > 0 {
    out.push(points[lo - 1]);
  }
  let inside = &points[lo..hi];
  let mut start = 0;
  while start < inside.len() {
    let bucket = cell_index(inside[start].x, x0, x1, columns);
    let run = inside[start..]
      .iter()
      .take_while(|p| cell_index(p.x, x0, x1, columns) == bucket)
      .count();
    push_m4(&inside[start..start + run], &mut out);
    start += run;
  }
  if hi < points.len() {
    out.push(points[hi]);
  }
  out
}

/// Keeps the first point of each cell of a `cols` x `rows` grid over the box
/// `[x0, x1] x [y0, y1]`, in data order. Points outside the box are dropped.
///
/// Any x order is accepted. The output has at most `cols * rows` points. A
/// zero-width or zero-height box puts all points in one column or row, as
/// `decimate_line` does for a zero-width range. Bounds given in reverse order
/// are swapped; `cols == 0` and `rows == 0` count as 1.
pub fn decimate_points(
  points: &[PlotPoint],
  x0: f64,
  x1: f64,
  y0: f64,
  y1: f64,
  cols: usize,
  rows: usize,
) -> Vec<PlotPoint> {
  let (cols, rows) = (cols.max(1), rows.max(1));
  let (x0, x1) = (x0.min(x1), x0.max(x1));
  let (y0, y1) = (y0.min(y1), y0.max(y1));
  let mut taken = vec![false; cols * rows];
  let mut out = Vec::new();
  for p in points {
    if !(x0..=x1).contains(&p.x) || !(y0..=y1).contains(&p.y) {
      continue;
    }
    let cx = cell_index(p.x, x0, x1, cols);
    let cy = cell_index(p.y, y0, y1, rows);
    let cell = cy * cols + cx;
    if !taken[cell] {
      taken[cell] = true;
      out.push(*p);
    }
  }
  out
}

/// Index of the equal-width cell of `[lo, hi]` that holds `v`, in `0..n`.
///
/// Values outside the range are clamped to the first or last cell. A zero-width
/// range, or a NaN one, puts everything in cell 0. `n` must be at least 1.
fn cell_index(v: f64, lo: f64, hi: f64, n: usize) -> usize {
  let width = hi - lo;
  if width.is_nan() || width <= 0.0 {
    return 0;
  }
  // Float to integer casts saturate, so a negative or NaN position gives 0.
  let index = ((v - lo) / width * n as f64) as usize;
  index.min(n - 1)
}

/// Appends the first, min-y, max-y and last point of a non-empty run, in data
/// order and without duplicates.
fn push_m4(run: &[PlotPoint], out: &mut Vec<PlotPoint>) {
  let (mut low, mut high) = (0, 0);
  for (k, p) in run.iter().enumerate() {
    if p.y < run[low].y {
      low = k;
    }
    if p.y > run[high].y {
      high = k;
    }
  }
  let mut picks = [0, low, high, run.len() - 1];
  picks.sort_unstable();
  let mut prev = None;
  for k in picks {
    if prev != Some(k) {
      out.push(run[k]);
      prev = Some(k);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A point at (x, y).
  fn pt(x: f64, y: f64) -> PlotPoint {
    PlotPoint::new(x, y)
  }

  /// Points (i, ys[i]) for each index i.
  fn series(ys: &[f64]) -> Vec<PlotPoint> {
    ys.iter()
      .enumerate()
      .map(|(i, &y)| pt(i as f64, y))
      .collect()
  }

  /// `n` pseudo-random values in [0, 1), from a fixed seed so that tests repeat.
  fn noise(n: usize) -> Vec<f64> {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    (0..n)
      .map(|_| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f64 / (1u64 << 53) as f64
      })
      .collect()
  }

  /// True if the x values never decrease.
  fn x_sorted(points: &[PlotPoint]) -> bool {
    points.windows(2).all(|w| w[0].x <= w[1].x)
  }

  #[test]
  fn line_empty_input() {
    assert!(decimate_line(&[], 0.0, 1.0, 10).is_empty());
    assert!(decimate_line(&[], 0.0, 0.0, 0).is_empty());
  }

  #[test]
  fn line_small_input_is_unchanged_when_buckets_hold_one_point() {
    let pts = series(&[3.0, 1.0, 4.0, 1.0, 5.0, 9.0, 2.0, 6.0, 5.0, 3.0]);
    assert_eq!(decimate_line(&pts, 0.0, 9.0, 10), pts);
  }

  #[test]
  fn line_output_is_bounded() {
    let pts = series(&noise(1000));
    for columns in [1, 2, 7, 100] {
      for (x0, x1) in [(0.0, 999.0), (-50.0, 2000.0), (100.0, 300.0)] {
        let out = decimate_line(&pts, x0, x1, columns);
        assert!(
          out.len() <= 4 * columns + 2,
          "{} points for {columns} columns",
          out.len()
        );
      }
    }
  }

  #[test]
  fn line_full_range_keeps_extremes_and_ends() {
    let pts = series(&noise(5000));
    let first = pts[0];
    let last = pts[pts.len() - 1];
    let min = pts
      .iter()
      .copied()
      .min_by(|a, b| a.y.total_cmp(&b.y))
      .unwrap();
    let max = pts
      .iter()
      .copied()
      .max_by(|a, b| a.y.total_cmp(&b.y))
      .unwrap();
    let out = decimate_line(&pts, first.x, last.x, 37);
    assert!(out.contains(&first));
    assert!(out.contains(&last));
    assert!(out.contains(&min));
    assert!(out.contains(&max));
    assert!(x_sorted(&out));
  }

  #[test]
  fn line_output_is_x_sorted() {
    let pts = series(&noise(3000));
    for columns in [1, 3, 64, 500] {
      let out = decimate_line(&pts, 100.0, 2900.0, columns);
      assert!(x_sorted(&out), "columns {columns}");
    }
  }

  #[test]
  fn line_range_keeps_neighbours_just_outside() {
    let pts = series(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]);
    let out = decimate_line(&pts, 3.5, 6.5, 100);
    let xs: Vec<f64> = out.iter().map(|p| p.x).collect();
    assert_eq!(xs, [3.0, 4.0, 5.0, 6.0, 7.0]);
  }

  #[test]
  fn line_gap_keeps_the_points_around_it() {
    let pts = vec![pt(0.0, 0.0), pt(10.0, 1.0)];
    assert_eq!(decimate_line(&pts, 3.0, 5.0, 4), pts);
  }

  #[test]
  fn line_range_beside_the_data_keeps_one_neighbour() {
    let pts = series(&[0.0, 1.0, 2.0, 3.0]);
    assert_eq!(decimate_line(&pts, -5.0, -1.0, 4), [pts[0]]);
    assert_eq!(decimate_line(&pts, 10.0, 20.0, 4), [pts[3]]);
  }

  #[test]
  fn line_zero_width_range_is_one_bucket() {
    let pts = vec![pt(5.0, 1.0), pt(5.0, 3.0), pt(5.0, 2.0), pt(5.0, 0.0)];
    let out = decimate_line(&pts, 5.0, 5.0, 10);
    assert_eq!(out, [pt(5.0, 1.0), pt(5.0, 3.0), pt(5.0, 0.0)]);
  }

  #[test]
  fn line_zero_columns_counts_as_one() {
    let pts = series(&noise(100));
    assert_eq!(
      decimate_line(&pts, 0.0, 99.0, 0),
      decimate_line(&pts, 0.0, 99.0, 1)
    );
  }

  #[test]
  fn line_bounds_in_reverse_order_are_swapped() {
    let pts = series(&noise(200));
    assert_eq!(
      decimate_line(&pts, 150.0, 20.0, 8),
      decimate_line(&pts, 20.0, 150.0, 8)
    );
  }

  #[test]
  fn line_million_point_sine() {
    let pts: Vec<PlotPoint> = (0..1_000_000u32)
      .map(|i| {
        let x = f64::from(i);
        pt(x, (x / 5000.0).sin() + 0.1 * (x / 7.0).sin())
      })
      .collect();
    let out = decimate_line(&pts, 0.0, 999_999.0, 2000);
    assert!(out.len() <= 8002, "{} points", out.len());
    assert!(x_sorted(&out));
    let min = pts.iter().map(|p| p.y).fold(f64::INFINITY, f64::min);
    let max = pts.iter().map(|p| p.y).fold(f64::NEG_INFINITY, f64::max);
    assert!(out.iter().any(|p| p.y == min), "global min missing");
    assert!(out.iter().any(|p| p.y == max), "global max missing");
  }

  #[test]
  fn points_empty_input() {
    assert!(decimate_points(&[], 0.0, 1.0, 0.0, 1.0, 10, 10).is_empty());
  }

  #[test]
  fn points_small_input_is_unchanged_when_cells_hold_one_point() {
    // A 4 x 4 grid with one point per cell, in a scrambled order.
    let pts: Vec<PlotPoint> = (0..16)
      .map(|k| {
        let order = (k * 7) % 16;
        pt((order % 4) as f64 + 0.5, (order / 4) as f64 + 0.5)
      })
      .collect();
    assert_eq!(decimate_points(&pts, 0.0, 4.0, 0.0, 4.0, 4, 4), pts);
  }

  #[test]
  fn points_output_is_bounded() {
    let values = noise(20_000);
    let pts: Vec<PlotPoint> = values
      .chunks(2)
      .map(|c| pt(c[0] * 10.0, c[1] * 10.0))
      .collect();
    let out = decimate_points(&pts, 0.0, 10.0, 0.0, 10.0, 12, 7);
    assert!(out.len() <= 12 * 7);
    assert!(out.len() > 12, "expected many cells filled");
  }

  #[test]
  fn points_keep_first_point_of_each_cell() {
    let pts = [pt(0.1, 0.1), pt(0.2, 0.2), pt(0.9, 0.9)];
    let out = decimate_points(&pts, 0.0, 1.0, 0.0, 1.0, 2, 2);
    assert_eq!(out, [pt(0.1, 0.1), pt(0.9, 0.9)]);
  }

  #[test]
  fn points_drop_points_outside_the_box() {
    let pts = [
      pt(-1.0, 0.5),
      pt(0.5, 2.0),
      pt(1.5, 0.5),
      pt(0.5, -0.5),
      pt(0.5, 0.5),
    ];
    let out = decimate_points(&pts, 0.0, 1.0, 0.0, 1.0, 1, 1);
    assert_eq!(out, [pt(0.5, 0.5)]);
  }

  #[test]
  fn points_box_edges_are_inclusive() {
    let pts = [pt(1.0, 1.0)];
    assert_eq!(decimate_points(&pts, 0.0, 1.0, 0.0, 1.0, 2, 2), pts);
  }

  #[test]
  fn points_accept_any_x_order() {
    // The first point in data order wins its cell, whatever its x.
    let pts = [pt(0.9, 0.5), pt(0.1, 0.5)];
    assert_eq!(decimate_points(&pts, 0.0, 1.0, 0.0, 1.0, 1, 1), [pts[0]]);
  }

  #[test]
  fn points_zero_width_box_is_one_column() {
    let pts: Vec<PlotPoint> = (0..5).map(|i| pt(2.0, f64::from(i))).collect();
    let out = decimate_points(&pts, 2.0, 2.0, 0.0, 4.0, 10, 5);
    assert_eq!(out, pts);
    let crowded: Vec<PlotPoint> = (0..50).map(|i| pt(2.0, f64::from(i) * 0.01)).collect();
    let out = decimate_points(&crowded, 2.0, 2.0, 0.0, 4.0, 10, 5);
    assert!(out.len() <= 5);
  }

  #[test]
  fn points_zero_height_box_is_one_row() {
    let pts = [pt(0.5, 1.0), pt(1.5, 1.0), pt(2.5, 1.0), pt(3.5, 0.0)];
    let out = decimate_points(&pts, 0.0, 4.0, 1.0, 1.0, 5, 5);
    assert_eq!(out, [pts[0], pts[1], pts[2]]);
  }

  #[test]
  fn points_zero_counts_as_one() {
    let pts = series(&noise(300));
    assert_eq!(
      decimate_points(&pts, 0.0, 299.0, 0.0, 1.0, 0, 0),
      decimate_points(&pts, 0.0, 299.0, 0.0, 1.0, 1, 1)
    );
  }
}
