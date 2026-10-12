//! Data model: aesthetic mapping, plot state, and CSV chunk ingestion.
//!
//! Pure logic, no I/O and no GUI, so that it can be unit tested.

use egui_plot::PlotPoint;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;

/// Aesthetic mapping from CSV columns to plot aesthetics.
///
/// Column references are header names, or 1-based column indices when no
/// header name matches.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Aes {
  pub x: Option<String>,
  /// One or more y columns; with several, each becomes its own series.
  pub y: Vec<String>,
  pub g: Option<String>,
}

impl Aes {
  /// Parses a spec like `"x=t y=a,b g=sensor"`. A blank spec is the default mapping.
  pub fn parse(spec: &str) -> Result<Aes, String> {
    let mut aes = Aes::default();
    let mut seen: Vec<&str> = Vec::new();
    for token in spec.split_whitespace() {
      let (key, value) = token
        .split_once('=')
        .ok_or_else(|| format!("expected key=value, got '{token}'"))?;
      let name = match key.to_ascii_lowercase().as_str() {
        "x" => "x",
        "y" => "y",
        "g" | "group" | "color" | "colour" => "g",
        _ => return Err(format!("unknown aesthetic '{key}' (expected x, y or g)")),
      };
      if seen.contains(&name) {
        return Err(format!("duplicate aesthetic '{name}'"));
      }
      seen.push(name);
      match name {
        "x" => aes.x = Some(single_column(name, value)?),
        "g" => aes.g = Some(single_column(name, value)?),
        _ => aes.y = y_columns(value)?,
      }
    }
    Ok(aes)
  }
}

/// One plotted series: a group, a y column, or both.
#[derive(Debug, Clone, Default)]
pub struct Series {
  pub name: String,
  pub points: VecDeque<PlotPoint>,
  /// Next implicit x for points pushed without an x value.
  pub next_index: f64,
  /// True while x never decreases, so that the points are sorted by x.
  pub sorted: bool,
}

/// Everything a plot server knows, shared between network threads and GUI.
#[derive(Debug, Default)]
pub struct PlotState {
  pub aes: Aes,
  /// Incremented whenever `aes` changes, so chunks re-resolve their columns.
  pub aes_rev: u64,
  /// Last header seen in any chunk; reused by chunks without a header.
  pub header: Option<Vec<String>>,
  /// Series in order of first appearance.
  pub series: Vec<Series>,
  /// Per-series buffer limit, 0 = unlimited. Oldest points are dropped.
  pub max_points: usize,
  /// Number of rows or values rejected so far.
  pub skipped: u64,
  /// Axis labels; empty means none.
  pub x_label: String,
  pub y_label: String,
  /// Window title requested by a client, if any.
  pub title: Option<String>,
  /// Set by a `Close` message; the GUI closes the window when it sees it.
  pub close_requested: bool,
  /// Series name to position in `series`.
  index: HashMap<String, usize>,
}

impl PlotState {
  /// Creates an empty state: default mapping, no buffer limit.
  pub fn new() -> Self {
    Self::default()
  }

  /// Replaces the aesthetic mapping; chunks re-resolve their columns on the next row.
  pub fn set_aes(&mut self, aes: Aes) {
    self.aes = aes;
    self.aes_rev += 1;
  }

  /// Appends a point to a named series, creating the series on first use.
  /// A missing `x` is the series' implicit index. Non-finite values are skipped.
  pub fn push_point(&mut self, series: &str, x: Option<f64>, y: f64) {
    if !y.is_finite() || x.is_some_and(|x| !x.is_finite()) {
      self.skipped += 1;
      return;
    }
    let pos = self.series_pos(series);
    let s = &mut self.series[pos];
    let x = x.unwrap_or(s.next_index);
    if s.points.back().is_some_and(|last| x < last.x) {
      s.sorted = false;
    }
    s.next_index += 1.0;
    s.points.push_back(PlotPoint::new(x, y));
    drop_oldest(&mut s.points, self.max_points);
  }

  /// Removes all series; settings, header and labels are kept.
  pub fn clear(&mut self) {
    self.series.clear();
    self.index.clear();
    self.skipped = 0;
  }

  /// Sets the per-series buffer limit (0 = unlimited) and trims existing series.
  pub fn set_max_points(&mut self, n: usize) {
    self.max_points = n;
    for s in &mut self.series {
      drop_oldest(&mut s.points, n);
    }
  }

  /// Number of points stored across all series.
  pub fn total_points(&self) -> usize {
    self.series.iter().map(|s| s.points.len()).sum()
  }

  /// Renders all points as CSV with a `series,x,y` header line.
  pub fn to_csv(&self) -> String {
    let mut out = String::from("series,x,y\n");
    for s in &self.series {
      let name = csv_quote(&s.name);
      for p in &s.points {
        let _ = writeln!(out, "{name},{},{}", p.x, p.y);
      }
    }
    out
  }

  /// Position of the series called `name`, appending an empty series if it is new.
  fn series_pos(&mut self, name: &str) -> usize {
    if let Some(&pos) = self.index.get(name) {
      return pos;
    }
    let pos = self.series.len();
    self.series.push(Series {
      name: name.to_string(),
      sorted: true,
      ..Default::default()
    });
    self.index.insert(name.to_string(), pos);
    pos
  }
}

/// Column separator of a chunk, decided by its first line.
#[derive(Debug, Clone, Copy)]
enum Sep {
  Comma,
  Tab,
  Semicolon,
  Whitespace,
}

/// Column indices of the aesthetic mapping, resolved at aes revision `rev`.
#[derive(Debug)]
struct Resolved {
  rev: u64,
  /// Column of x, `None` for the implicit index.
  x: Option<usize>,
  /// Columns of y, in the order of `Aes::y`.
  ys: Vec<usize>,
  g: Option<usize>,
  /// Names of the x and y columns, for labels and messages.
  x_name: Option<String>,
  y_names: Vec<String>,
  /// Number of fields a row needs to cover every resolved column.
  needed: usize,
}

/// Per-connection CSV parsing state: separator, header, resolved columns.
#[derive(Debug, Default)]
pub struct Chunk {
  /// Decided by the first non-ignored line; `None` until then.
  sep: Option<Sep>,
  /// This chunk's header, if its first non-ignored line was one.
  header: Option<Vec<String>>,
  /// Cached column resolution, valid while its `rev` matches `aes_rev`.
  resolved: Option<Resolved>,
}

impl Chunk {
  /// Starts a chunk before any of its lines has been seen.
  pub fn new() -> Self {
    Self::default()
  }

  /// Ingests one CSV line into `state`.
  pub fn ingest(&mut self, state: &mut PlotState, line: &str) -> Result<(), String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
      return Ok(());
    }
    let first = self.sep.is_none();
    let sep = *self.sep.get_or_insert_with(|| detect_sep(line));
    let fields = split_fields(line, sep);
    if first && fields.iter().all(|f| f.parse::<f64>().is_err()) {
      state.header = Some(fields.clone());
      self.header = Some(fields);
      return Ok(());
    }

    // Take the cached resolution and put it back after mapping, so that
    // a failed resolution is never cached.
    let resolved = match self.resolved.take() {
      Some(r) if r.rev == state.aes_rev => r,
      _ => {
        let n = fields.len();
        let names = column_names(self.header.as_deref(), state.header.as_deref(), n);
        match resolve(&state.aes, &names, n, state.aes_rev) {
          Ok(r) => {
            state.x_label = r.x_name.clone().unwrap_or_else(|| "index".to_string());
            state.y_label = if r.y_names.len() == 1 {
              r.y_names[0].clone()
            } else {
              String::new()
            };
            r
          }
          Err(e) => {
            state.skipped += 1;
            return Err(e);
          }
        }
      }
    };
    let result = map_row(state, &resolved, &fields, line);
    self.resolved = Some(resolved);
    result
  }
}

/// Quotes a series name for CSV if it contains a comma or a double quote.
fn csv_quote(field: &str) -> String {
  if field.contains([',', '"']) {
    format!("\"{}\"", field.replace('"', "\"\""))
  } else {
    field.to_string()
  }
}

/// Drops the oldest points so that at most `max` remain; 0 means unlimited.
fn drop_oldest(points: &mut VecDeque<PlotPoint>, max: usize) {
  if max > 0 {
    let excess = points.len().saturating_sub(max);
    points.drain(..excess);
  }
}

/// Value of a single-column aesthetic (`x` or `g`).
fn single_column(name: &str, value: &str) -> Result<String, String> {
  if value.is_empty() {
    Err(format!("no column given for aesthetic '{name}'"))
  } else if value.contains(',') {
    Err(format!(
      "aesthetic '{name}' takes one column, got '{value}'"
    ))
  } else {
    Ok(value.to_string())
  }
}

/// Value of the `y` aesthetic: a comma-separated list with empty items dropped.
fn y_columns(value: &str) -> Result<Vec<String>, String> {
  let columns: Vec<String> = value
    .split(',')
    .filter(|c| !c.is_empty())
    .map(String::from)
    .collect();
  if columns.is_empty() {
    Err("no column given for aesthetic 'y'".to_string())
  } else {
    Ok(columns)
  }
}

/// Picks the separator from a line: comma, then tab, then semicolon, else whitespace.
fn detect_sep(line: &str) -> Sep {
  if line.contains(',') {
    Sep::Comma
  } else if line.contains('\t') {
    Sep::Tab
  } else if line.contains(';') {
    Sep::Semicolon
  } else {
    Sep::Whitespace
  }
}

/// Splits a line into fields, each trimmed with one pair of quotes removed.
fn split_fields(line: &str, sep: Sep) -> Vec<String> {
  let raw: Vec<&str> = match sep {
    Sep::Comma => line.split(',').collect(),
    Sep::Tab => line.split('\t').collect(),
    Sep::Semicolon => line.split(';').collect(),
    Sep::Whitespace => line.split_whitespace().collect(),
  };
  raw
    .into_iter()
    .map(|f| unquote(f.trim()).to_string())
    .collect()
}

/// Removes one pair of surrounding double quotes, if present.
fn unquote(field: &str) -> &str {
  field
    .strip_prefix('"')
    .and_then(|f| f.strip_suffix('"'))
    .unwrap_or(field)
}

/// Values that mark a missing observation.
fn is_missing(v: &str) -> bool {
  matches!(v, "" | "NA" | "na" | "NaN" | "nan" | "null")
}

/// Column names for a row of `n` fields: this chunk's header, else the last
/// header seen if it has `n` names, else 1-based numbers.
fn column_names(chunk: Option<&[String]>, last: Option<&[String]>, n: usize) -> Vec<String> {
  match (chunk, last) {
    (Some(h), _) => h.to_vec(),
    (None, Some(h)) if h.len() == n => h.to_vec(),
    _ => (1..=n).map(|i| i.to_string()).collect(),
  }
}

/// Column index of a reference: an exact name first, then a 1-based index.
fn column_index(names: &[String], n: usize, reference: &str) -> Result<usize, String> {
  if let Some(i) = names.iter().position(|name| name == reference) {
    return Ok(i);
  }
  match reference.parse::<usize>() {
    Ok(k) if (1..=n).contains(&k) => Ok(k - 1),
    _ => Err(format!(
      "unknown column '{reference}' (columns: {})",
      names.join(", ")
    )),
  }
}

/// Resolves the aesthetic mapping to column indices for rows of `n` fields.
fn resolve(aes: &Aes, names: &[String], n: usize, rev: u64) -> Result<Resolved, String> {
  let g = aes
    .g
    .as_deref()
    .map(|r| column_index(names, n, r))
    .transpose()?;
  let (x, ys) = if aes.y.is_empty() {
    let avail: Vec<usize> = (0..n).filter(|i| Some(*i) != g).collect();
    let x = match aes.x.as_deref() {
      Some(r) => Some(column_index(names, n, r)?),
      None if avail.len() >= 2 => Some(avail[0]),
      None => None,
    };
    let ys: Vec<usize> = avail.into_iter().filter(|i| Some(*i) != x).collect();
    if ys.is_empty() {
      return Err("no column left to map to y".to_string());
    }
    (x, ys)
  } else {
    let x = aes
      .x
      .as_deref()
      .map(|r| column_index(names, n, r))
      .transpose()?;
    let ys = aes
      .y
      .iter()
      .map(|r| column_index(names, n, r))
      .collect::<Result<Vec<usize>, String>>()?;
    (x, ys)
  };
  let needed = g.iter().chain(&x).chain(&ys).max().map_or(0, |&m| m + 1);
  let name_of = |i: usize| names.get(i).cloned().unwrap_or_else(|| (i + 1).to_string());
  Ok(Resolved {
    rev,
    x_name: x.map(name_of),
    y_names: ys.iter().map(|&i| name_of(i)).collect(),
    x,
    ys,
    g,
    needed,
  })
}

/// Maps one data row to points in `state`. Rejected rows count as skipped.
fn map_row(
  state: &mut PlotState,
  r: &Resolved,
  fields: &[String],
  line: &str,
) -> Result<(), String> {
  if fields.len() < r.needed {
    state.skipped += 1;
    return Err(format!(
      "row has {} fields, expected at least {}: '{line}'",
      fields.len(),
      r.needed
    ));
  }

  let x = match r.x {
    None => None,
    Some(i) => {
      let v = fields[i].as_str();
      if is_missing(v) {
        state.skipped += 1;
        return Ok(());
      }
      match v.parse::<f64>() {
        Ok(x) => Some(x),
        Err(_) => {
          state.skipped += 1;
          let col = r.x_name.as_deref().unwrap_or_default();
          return Err(format!("cannot parse x value '{v}' in column '{col}'"));
        }
      }
    }
  };

  // Parse every y first, so that one bad value rejects the whole row.
  let mut values = Vec::with_capacity(r.ys.len());
  for (k, &i) in r.ys.iter().enumerate() {
    let v = fields[i].as_str();
    if is_missing(v) {
      continue;
    }
    match v.parse::<f64>() {
      Ok(y) => values.push((k, y)),
      Err(_) => {
        state.skipped += 1;
        let col = &r.y_names[k];
        return Err(format!("cannot parse y value '{v}' in column '{col}'"));
      }
    }
  }

  let g = r.g.map(|i| fields[i].as_str());
  for (k, y) in values {
    let ycol = &r.y_names[k];
    let name = match g {
      Some(g) if r.ys.len() > 1 => format!("{g}/{ycol}"),
      Some(g) => g.to_string(),
      None => ycol.clone(),
    };
    state.push_point(&name, x, y);
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Ingests `lines` as one chunk, panicking on the first error.
  fn feed(state: &mut PlotState, lines: &[&str]) {
    let mut chunk = Chunk::new();
    for line in lines {
      chunk.ingest(state, line).unwrap();
    }
  }

  /// Points of the series called `name`, as (x, y) pairs.
  fn points(state: &PlotState, name: &str) -> Vec<(f64, f64)> {
    let series = state
      .series
      .iter()
      .find(|s| s.name == name)
      .unwrap_or_else(|| panic!("no series '{name}'"));
    series.points.iter().map(|p| (p.x, p.y)).collect()
  }

  /// Names of all series, in order.
  fn names(state: &PlotState) -> Vec<&str> {
    state.series.iter().map(|s| s.name.as_str()).collect()
  }

  fn aes(spec: &str) -> Aes {
    Aes::parse(spec).unwrap()
  }

  #[test]
  fn aes_parse_valid() {
    let a = aes("x=t y=a g=s");
    assert_eq!(a.x.as_deref(), Some("t"));
    assert_eq!(a.y, ["a"]);
    assert_eq!(a.g.as_deref(), Some("s"));
  }

  #[test]
  fn aes_parse_aliases_and_case() {
    assert_eq!(aes("group=s").g.as_deref(), Some("s"));
    assert_eq!(aes("color=s").g.as_deref(), Some("s"));
    assert_eq!(aes("colour=s").g.as_deref(), Some("s"));
    assert_eq!(aes("Group=s").g.as_deref(), Some("s"));
    assert_eq!(aes("X=t Y=a G=s"), aes("x=t y=a g=s"));
  }

  #[test]
  fn aes_parse_multiple_y() {
    assert_eq!(aes("y=a,b").y, ["a", "b"]);
    assert_eq!(aes("y=a,,b").y, ["a", "b"]);
    assert_eq!(aes("y=,a,").y, ["a"]);
  }

  #[test]
  fn aes_parse_empty_spec_is_default() {
    assert_eq!(aes(""), Aes::default());
    assert_eq!(aes("  \t "), Aes::default());
  }

  #[test]
  fn aes_parse_errors() {
    assert!(Aes::parse("x").is_err());
    assert_eq!(
      Aes::parse("foo=1").unwrap_err(),
      "unknown aesthetic 'foo' (expected x, y or g)"
    );
    assert_eq!(
      Aes::parse("x=a x=b").unwrap_err(),
      "duplicate aesthetic 'x'"
    );
    assert_eq!(
      Aes::parse("g=a color=b").unwrap_err(),
      "duplicate aesthetic 'g'"
    );
    assert!(Aes::parse("y=").is_err());
    assert!(Aes::parse("y=,,").is_err());
    assert!(Aes::parse("x=").is_err());
    assert!(Aes::parse("x=a,b").is_err());
    assert!(Aes::parse("g=").is_err());
    assert!(Aes::parse("g=a,b").is_err());
  }

  #[test]
  fn header_with_default_mapping() {
    let mut state = PlotState::new();
    feed(&mut state, &["t,a,b", "0,1,2", "1,3,4"]);
    assert_eq!(names(&state), ["a", "b"]);
    assert_eq!(points(&state, "a"), [(0.0, 1.0), (1.0, 3.0)]);
    assert_eq!(points(&state, "b"), [(0.0, 2.0), (1.0, 4.0)]);
    assert_eq!(state.x_label, "t");
    assert_eq!(state.y_label, "");
    assert_eq!(state.skipped, 0);
    assert_eq!(state.total_points(), 4);
  }

  #[test]
  fn single_column_uses_index_as_x() {
    let mut state = PlotState::new();
    feed(&mut state, &["v", "5", "7"]);
    assert_eq!(points(&state, "v"), [(0.0, 5.0), (1.0, 7.0)]);
    assert_eq!(state.x_label, "index");
    assert_eq!(state.y_label, "v");
  }

  #[test]
  fn no_header_uses_numeric_names() {
    let mut state = PlotState::new();
    feed(&mut state, &["1,2,3", "4,5,6"]);
    assert_eq!(names(&state), ["2", "3"]);
    assert_eq!(points(&state, "3"), [(1.0, 3.0), (4.0, 6.0)]);
    assert_eq!(state.x_label, "1");
  }

  #[test]
  fn no_header_maps_aes_by_index() {
    let mut state = PlotState::new();
    state.set_aes(aes("x=1 y=3"));
    feed(&mut state, &["1,2,3", "2,5,6"]);
    assert_eq!(names(&state), ["3"]);
    assert_eq!(points(&state, "3"), [(1.0, 3.0), (2.0, 6.0)]);
    assert_eq!(state.x_label, "1");
    assert_eq!(state.y_label, "3");
  }

  #[test]
  fn header_is_reused_by_later_chunks() {
    let mut state = PlotState::new();
    feed(&mut state, &["t,a,b"]);
    feed(&mut state, &["0,1,2"]);
    assert_eq!(names(&state), ["a", "b"]);
    assert_eq!(points(&state, "a"), [(0.0, 1.0)]);
    assert_eq!(state.x_label, "t");
  }

  #[test]
  fn header_of_other_length_is_not_reused() {
    let mut state = PlotState::new();
    feed(&mut state, &["t,a,b"]);
    feed(&mut state, &["0,1"]);
    assert_eq!(names(&state), ["2"]);
    assert_eq!(points(&state, "2"), [(0.0, 1.0)]);
    assert_eq!(state.x_label, "1");
    assert_eq!(state.y_label, "2");
  }

  #[test]
  fn column_references_fall_back_to_index() {
    let mut state = PlotState::new();
    state.set_aes(aes("x=1 y=3"));
    feed(&mut state, &["t,a,b", "0,1,2"]);
    assert_eq!(points(&state, "b"), [(0.0, 2.0)]);
    assert_eq!(state.x_label, "t");
    assert_eq!(state.y_label, "b");
  }

  #[test]
  fn multiple_y_with_group_names_series_group_slash_column() {
    let mut state = PlotState::new();
    state.set_aes(aes("x=t y=a,b g=s"));
    feed(&mut state, &["s,t,a,b", "k1,0,1,2", "k2,1,3,4"]);
    assert_eq!(names(&state), ["k1/a", "k1/b", "k2/a", "k2/b"]);
    assert_eq!(points(&state, "k1/a"), [(0.0, 1.0)]);
    assert_eq!(points(&state, "k2/b"), [(1.0, 4.0)]);
    assert_eq!(state.y_label, "");
  }

  #[test]
  fn single_y_with_group_names_series_by_group() {
    let mut state = PlotState::new();
    state.set_aes(aes("y=a g=s"));
    feed(&mut state, &["s,t,a", "k1,0,1", "k2,1,3"]);
    assert_eq!(names(&state), ["k1", "k2"]);
    assert_eq!(points(&state, "k2"), [(0.0, 3.0)]);
    assert_eq!(state.x_label, "index");
    assert_eq!(state.y_label, "a");
  }

  #[test]
  fn tab_semicolon_and_whitespace_separators() {
    for lines in [
      ["t\ta\tb", "0\t1\t2"],
      ["t;a;b", "0;1;2"],
      ["t  a b", "0 1   2"],
    ] {
      let mut state = PlotState::new();
      feed(&mut state, &lines);
      assert_eq!(names(&state), ["a", "b"], "lines: {lines:?}");
      assert_eq!(points(&state, "b"), [(0.0, 2.0)], "lines: {lines:?}");
      assert_eq!(state.x_label, "t", "lines: {lines:?}");
    }
  }

  #[test]
  fn separator_is_fixed_by_first_line() {
    let mut state = PlotState::new();
    let mut chunk = Chunk::new();
    chunk.ingest(&mut state, "1,2").unwrap();
    let err = chunk.ingest(&mut state, "3 4").unwrap_err();
    assert_eq!(err, "row has 1 fields, expected at least 2: '3 4'");
    assert_eq!(state.skipped, 1);
  }

  #[test]
  fn surrounding_quotes_are_removed() {
    let mut state = PlotState::new();
    feed(&mut state, &[r#""t", "a""#, r#""0", 1"#]);
    assert_eq!(points(&state, "a"), [(0.0, 1.0)]);
    assert_eq!(state.x_label, "t");
  }

  #[test]
  fn comments_and_blank_lines_are_ignored() {
    let mut state = PlotState::new();
    let mut chunk = Chunk::new();
    let lines = [
      "# a comment",
      "",
      "   ",
      "\r",
      "t,a",
      "# not a header",
      "0,1",
      "  # indented",
      "1,2",
    ];
    for line in lines {
      chunk.ingest(&mut state, line).unwrap();
    }
    assert_eq!(state.header, Some(vec!["t".to_string(), "a".to_string()]));
    assert_eq!(points(&state, "a"), [(0.0, 1.0), (1.0, 2.0)]);
    assert_eq!(state.skipped, 0);
  }

  #[test]
  fn header_is_only_accepted_on_first_line() {
    let mut state = PlotState::new();
    let mut chunk = Chunk::new();
    chunk.ingest(&mut state, "0,1,2").unwrap();
    assert!(chunk.ingest(&mut state, "t,a,b").is_err());
    assert_eq!(state.header, None);
    assert_eq!(state.skipped, 1);
  }

  #[test]
  fn missing_x_skips_row_and_missing_y_skips_point() {
    let mut state = PlotState::new();
    feed(
      &mut state,
      &["t,a,b", "NA,1,2", ",1,2", "0,,2", "nan,3,4", "null,5,6"],
    );
    assert_eq!(names(&state), ["b"]);
    assert_eq!(points(&state, "b"), [(0.0, 2.0)]);
    assert_eq!(state.total_points(), 1);
    // Four rows lost their x; the missing y is not counted.
    assert_eq!(state.skipped, 4);
  }

  #[test]
  fn unparsable_value_rejects_whole_row() {
    let mut state = PlotState::new();
    let mut chunk = Chunk::new();
    chunk.ingest(&mut state, "t,a,b").unwrap();
    chunk.ingest(&mut state, "0,1,2").unwrap();
    let err = chunk.ingest(&mut state, "1,abc,4").unwrap_err();
    assert_eq!(err, "cannot parse y value 'abc' in column 'a'");
    let err = chunk.ingest(&mut state, "zz,3,4").unwrap_err();
    assert_eq!(err, "cannot parse x value 'zz' in column 't'");
    assert_eq!(state.skipped, 2);
    assert_eq!(points(&state, "a"), [(0.0, 1.0)]);
    assert_eq!(points(&state, "b"), [(0.0, 2.0)]);
  }

  #[test]
  fn short_rows_are_rejected_and_extra_fields_ignored() {
    let mut state = PlotState::new();
    let mut chunk = Chunk::new();
    chunk.ingest(&mut state, "t,a,b").unwrap();
    chunk.ingest(&mut state, "0,1,2").unwrap();
    let err = chunk.ingest(&mut state, "0,1").unwrap_err();
    assert_eq!(err, "row has 2 fields, expected at least 3: '0,1'");
    chunk.ingest(&mut state, "1,2,3,9").unwrap();
    assert_eq!(state.skipped, 1);
    assert_eq!(points(&state, "a"), [(0.0, 1.0), (1.0, 2.0)]);
  }

  #[test]
  fn unknown_column_lists_columns_and_retries_after_fix() {
    let mut state = PlotState::new();
    state.set_aes(aes("x=t y=zz"));
    let mut chunk = Chunk::new();
    chunk.ingest(&mut state, "t,a,b").unwrap();
    let err = chunk.ingest(&mut state, "0,1,2").unwrap_err();
    assert_eq!(err, "unknown column 'zz' (columns: t, a, b)");
    assert_eq!(state.skipped, 1);
    state.set_aes(aes("x=t y=a"));
    chunk.ingest(&mut state, "0,1,2").unwrap();
    assert_eq!(points(&state, "a"), [(0.0, 1.0)]);
    assert_eq!(state.skipped, 1);
  }

  #[test]
  fn no_column_left_for_y_is_an_error() {
    let mut state = PlotState::new();
    state.set_aes(aes("x=v g=s"));
    let mut chunk = Chunk::new();
    chunk.ingest(&mut state, "s,v").unwrap();
    let err = chunk.ingest(&mut state, "k,1").unwrap_err();
    assert_eq!(err, "no column left to map to y");
    assert_eq!(state.skipped, 1);
  }

  #[test]
  fn changing_aes_mid_chunk_reresolves_columns() {
    let mut state = PlotState::new();
    let mut chunk = Chunk::new();
    chunk.ingest(&mut state, "t,a,b").unwrap();
    chunk.ingest(&mut state, "0,1,2").unwrap();
    state.set_aes(aes("x=t y=b"));
    chunk.ingest(&mut state, "1,3,4").unwrap();
    assert_eq!(points(&state, "a"), [(0.0, 1.0)]);
    assert_eq!(points(&state, "b"), [(0.0, 2.0), (1.0, 4.0)]);
    assert_eq!(state.y_label, "b");
    assert_eq!(state.aes_rev, 1);
  }

  #[test]
  fn explicit_y_without_x_uses_index() {
    let mut state = PlotState::new();
    state.set_aes(aes("y=b"));
    feed(&mut state, &["t,a,b", "0,1,2", "1,3,4"]);
    assert_eq!(points(&state, "b"), [(0.0, 2.0), (1.0, 4.0)]);
    assert_eq!(state.x_label, "index");
    assert_eq!(state.y_label, "b");
  }

  #[test]
  fn non_finite_values_are_skipped() {
    let mut state = PlotState::new();
    feed(&mut state, &["t,a", "0,1", "1,inf", "-inf,2"]);
    assert_eq!(state.skipped, 2);
    assert_eq!(points(&state, "a"), [(0.0, 1.0)]);
  }

  #[test]
  fn push_point_uses_implicit_index_per_series() {
    let mut state = PlotState::new();
    state.push_point("a", None, 1.0);
    state.push_point("a", None, 2.0);
    state.push_point("b", None, 5.0);
    state.push_point("a", Some(10.0), 3.0);
    state.push_point("a", None, 4.0);
    assert_eq!(names(&state), ["a", "b"]);
    assert_eq!(
      points(&state, "a"),
      [(0.0, 1.0), (1.0, 2.0), (10.0, 3.0), (3.0, 4.0)]
    );
    assert_eq!(points(&state, "b"), [(0.0, 5.0)]);
  }

  #[test]
  fn push_point_skips_non_finite_without_advancing_index() {
    let mut state = PlotState::new();
    state.push_point("a", Some(f64::NAN), 1.0);
    state.push_point("a", None, f64::INFINITY);
    assert_eq!(state.skipped, 2);
    assert_eq!(state.total_points(), 0);
    state.push_point("a", None, 1.0);
    assert_eq!(points(&state, "a"), [(0.0, 1.0)]);
  }

  #[test]
  fn max_points_drops_oldest_and_set_max_points_trims() {
    let mut state = PlotState::new();
    state.set_max_points(3);
    for i in 0..5 {
      state.push_point("a", None, f64::from(i));
    }
    assert_eq!(points(&state, "a"), [(2.0, 2.0), (3.0, 3.0), (4.0, 4.0)]);
    assert_eq!(state.total_points(), 3);
    state.set_max_points(1);
    assert_eq!(points(&state, "a"), [(4.0, 4.0)]);
    state.set_max_points(0);
    state.push_point("a", None, 5.0);
    state.push_point("a", None, 6.0);
    assert_eq!(state.total_points(), 3);
  }

  #[test]
  fn clear_removes_series_and_keeps_settings() {
    let mut state = PlotState::new();
    state.set_aes(aes("x=t"));
    state.set_max_points(5);
    state.title = Some("demo".to_string());
    state.skipped = 3;
    feed(&mut state, &["t,a", "0,1"]);
    state.clear();
    assert!(state.series.is_empty());
    assert_eq!(state.total_points(), 0);
    assert_eq!(state.skipped, 0);
    assert_eq!(state.aes.x.as_deref(), Some("t"));
    assert_eq!(state.aes_rev, 1);
    assert_eq!(state.max_points, 5);
    assert_eq!(state.title.as_deref(), Some("demo"));
    assert_eq!(state.header, Some(vec!["t".to_string(), "a".to_string()]));
    assert_eq!(state.x_label, "t");
    // The name index was cleared too, so the series is created afresh.
    state.push_point("a", None, 9.0);
    assert_eq!(names(&state), ["a"]);
    assert_eq!(points(&state, "a"), [(0.0, 9.0)]);
  }

  #[test]
  fn to_csv_lists_points_and_quotes_names() {
    let mut state = PlotState::new();
    assert_eq!(state.to_csv(), "series,x,y\n");
    state.push_point("a", Some(0.0), 1.5);
    state.push_point("b,c", Some(1.0), 2.0);
    state.push_point("a", Some(2.0), -3.0);
    state.push_point("q\"t", None, 4.0);
    assert_eq!(
      state.to_csv(),
      "series,x,y\na,0,1.5\na,2,-3\n\"b,c\",1,2\n\"q\"\"t\",0,4\n"
    );
  }

  /// Whether the series called `name` is flagged as sorted.
  fn is_sorted(state: &PlotState, name: &str) -> bool {
    state
      .series
      .iter()
      .find(|s| s.name == name)
      .unwrap_or_else(|| panic!("no series '{name}'"))
      .sorted
  }

  #[test]
  fn implicit_index_keeps_series_sorted() {
    let mut state = PlotState::new();
    for _ in 0..4 {
      state.push_point("a", None, 1.0);
    }
    assert!(is_sorted(&state, "a"));
    // Equal x values are still in order.
    state.push_point("b", Some(2.0), 1.0);
    state.push_point("b", Some(2.0), 2.0);
    assert!(is_sorted(&state, "b"));
  }

  #[test]
  fn decreasing_x_clears_sorted_for_good() {
    let mut state = PlotState::new();
    state.push_point("a", Some(1.0), 1.0);
    state.push_point("a", Some(3.0), 1.0);
    assert!(is_sorted(&state, "a"));
    state.push_point("a", Some(2.0), 1.0);
    assert!(!is_sorted(&state, "a"));
    // Larger x values do not restore the flag.
    state.push_point("a", Some(10.0), 1.0);
    assert!(!is_sorted(&state, "a"));
    // The implicit index can also decrease, after a larger explicit x.
    state.push_point("b", Some(10.0), 1.0);
    state.push_point("b", None, 1.0);
    assert!(!is_sorted(&state, "b"));
    // Other series are unaffected.
    state.push_point("c", Some(5.0), 1.0);
    assert!(is_sorted(&state, "c"));
  }

  #[test]
  fn clear_then_push_gives_sorted_series_again() {
    let mut state = PlotState::new();
    state.push_point("a", Some(2.0), 1.0);
    state.push_point("a", Some(1.0), 1.0);
    assert!(!is_sorted(&state, "a"));
    state.clear();
    state.push_point("a", Some(0.0), 1.0);
    assert!(is_sorted(&state, "a"));
  }
}
