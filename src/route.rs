//! `--by COL`: routes the rows of a CSV chunk to the plots named by the values
//! of one column, which is removed from the forwarded lines.

use crate::model::{Sep, column_index, detect_sep, is_header, split_fields};
use crate::registry::valid_name;

/// What to do with one line of a chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Routed {
  /// A comment, other than `#clear`: nothing to send.
  Skip,
  /// A blank line: nothing to send, but it ends a frame with `--frames`.
  Blank,
  /// A `#clear` directive, with its names without the routing column joined
  /// with commas, if any were given.
  Clear(Option<String>),
  /// The chunk's header without the routing column. Every plot of this chunk
  /// gets it before its first row.
  Header(String),
  /// A data row for plot `plot`, without the routing column.
  Row { plot: String, line: String },
  /// A line that can't be routed, and why.
  Reject(String),
}

/// Routing state of one chunk: separator, header and routing column.
#[derive(Debug)]
pub struct Router {
  col: String,
  /// Separator of the chunk, decided by its first line that is not blank or a comment.
  sep: Option<Sep>,
  /// Whether the next line may be the header: at the start of a chunk, and
  /// after a `#clear` without names.
  may_be_header: bool,
  /// Names given by `#clear`, without the routing column, until the separator
  /// is known and they become the header.
  pending: Option<Vec<String>>,
  /// Zero-based index of the routing column, once a header has resolved it.
  index: Option<usize>,
  /// The header without the routing column.
  header: Option<String>,
  /// Why every row is rejected, when the header has no such column.
  failure: Option<String>,
}

impl Router {
  /// A router on column `col`: a header name, or a 1-based column index.
  pub fn new(col: &str) -> Self {
    Router {
      col: col.to_string(),
      sep: None,
      may_be_header: true,
      pending: None,
      index: None,
      header: None,
      failure: None,
    }
  }

  /// Classifies the next line of the chunk.
  pub fn route(&mut self, line: &str) -> Routed {
    let line = line.trim();
    if line.is_empty() {
      return Routed::Blank;
    }
    if let Some(rest) = line
      .strip_prefix("#clear")
      .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
      return self.route_clear(rest.trim());
    }
    if line.starts_with('#') {
      return Routed::Skip;
    }
    let sep = *self.sep.get_or_insert_with(|| detect_sep(line));
    let mut fields = split_fields(line, sep);
    if let Some(names) = self.pending.take() {
      self.header = Some(sep.join(names.iter().map(String::as_str)));
    }
    if std::mem::take(&mut self.may_be_header) && is_header(&fields) {
      return self.route_header(sep, &fields);
    }
    if let Some(reason) = &self.failure {
      return Routed::Reject(reason.clone());
    }
    self.route_row(sep, &mut fields, line)
  }

  /// The header without the routing column, once the chunk has had one.
  pub fn header(&self) -> Option<&str> {
    self.header.as_deref()
  }

  /// Routes a `#clear` directive with the names `rest`, which restarts the
  /// chunk. Given names are the header, so the next line is data.
  fn route_clear(&mut self, rest: &str) -> Routed {
    self.restart();
    let mut names = split_names(rest);
    self.may_be_header = names.is_empty();
    if names.is_empty() {
      return Routed::Clear(None);
    }
    match column_index(&names, names.len(), &self.col) {
      Ok(k) => {
        self.index = Some(k);
        names.remove(k);
        let header = names.join(",");
        self.pending = Some(names);
        Routed::Clear(Some(header))
      }
      Err(_) => {
        let header = names.join(",");
        self.failure = Some(unknown_column(&self.col, &names));
        Routed::Clear(Some(header))
      }
    }
  }

  /// Forgets the separator, header and routing column, as at a chunk's start.
  fn restart(&mut self) {
    self.sep = None;
    self.pending = None;
    self.index = None;
    self.header = None;
    self.failure = None;
  }

  /// Routes the first line when it is a header, resolving the column in it.
  fn route_header(&mut self, sep: Sep, fields: &[String]) -> Routed {
    match column_index(fields, fields.len(), &self.col) {
      Ok(k) => {
        self.index = Some(k);
        let header = without(sep, fields, k);
        self.header = Some(header.clone());
        Routed::Header(header)
      }
      Err(_) => {
        let reason = unknown_column(&self.col, fields);
        self.failure = Some(reason.clone());
        Routed::Reject(reason)
      }
    }
  }

  /// Routes a data row. Without a header, the column is a 1-based index.
  fn route_row(&self, sep: Sep, fields: &mut [String], line: &str) -> Routed {
    let k = match self.index {
      Some(k) => k,
      None => match self.col.parse::<usize>() {
        Ok(n) if n > 0 => n - 1,
        Ok(_) => {
          return Routed::Reject(format!(
            "unknown column '{}' for --by (columns start at 1)",
            self.col
          ));
        }
        Err(_) => {
          return Routed::Reject(format!("column '{}' needs a header line", self.col));
        }
      },
    };
    if k >= fields.len() {
      return Routed::Reject(format!(
        "row has {} fields, but --by needs column {}: '{line}'",
        fields.len(),
        k + 1
      ));
    }
    if fields[k].is_empty() {
      return Routed::Reject(format!("missing plot name: column {} is empty", self.col));
    }
    if !valid_name(&fields[k]) {
      return Routed::Reject(format!(
        "'{}' in column {} is not a valid plot name",
        fields[k], self.col
      ));
    }
    let rest = without(sep, fields, k);
    Routed::Row {
      plot: std::mem::take(&mut fields[k]),
      line: rest,
    }
  }
}

/// The fields except column `k`, joined with `sep`.
fn without(sep: Sep, fields: &[String], k: usize) -> String {
  sep.join(
    fields
      .iter()
      .enumerate()
      .filter(|&(i, _)| i != k)
      .map(|(_, f)| f.as_str()),
  )
}

/// The column names of a `#clear` line: split on commas if it has any, else
/// on whitespace. Unquoted, with empty names dropped.
fn split_names(rest: &str) -> Vec<String> {
  let sep = if rest.contains(',') {
    Sep::Comma
  } else {
    Sep::Whitespace
  };
  split_fields(rest, sep)
    .into_iter()
    .filter(|name| !name.is_empty())
    .collect()
}

/// The rejection of every row when the routing column `col` is not in `names`.
fn unknown_column(col: &str, names: &[String]) -> String {
  format!(
    "unknown column '{col}' for --by (columns: {})",
    names.join(", ")
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Routes `lines` in order with one router on `col`.
  fn route_all(col: &str, lines: &[&str]) -> Vec<Routed> {
    let mut router = Router::new(col);
    lines.iter().map(|line| router.route(line)).collect()
  }

  /// A data row for `plot` with the forwarded `line`.
  fn row(plot: &str, line: &str) -> Routed {
    Routed::Row {
      plot: plot.to_string(),
      line: line.to_string(),
    }
  }

  /// A header line.
  fn head(line: &str) -> Routed {
    Routed::Header(line.to_string())
  }

  /// A rejection with message `msg`.
  fn reject(msg: &str) -> Routed {
    Routed::Reject(msg.to_string())
  }

  #[test]
  fn named_column_is_dropped_from_header_and_rows() {
    let mut router = Router::new("sensor");
    assert_eq!(router.route("t,sensor,v"), head("t,v"));
    assert_eq!(router.route("0,a,1"), row("a", "0,1"));
    assert_eq!(router.route("1,b,2"), row("b", "1,2"));
    assert_eq!(router.header(), Some("t,v"));
  }

  #[test]
  fn index_column_counts_from_one_with_header() {
    assert_eq!(
      route_all("2", &["t,sensor,v", "0,a,1"]),
      vec![head("t,v"), row("a", "0,1")]
    );
  }

  #[test]
  fn index_column_without_header_uses_the_row_fields() {
    let mut router = Router::new("1");
    assert_eq!(router.route("a 0 1"), row("a", "0 1"));
    assert_eq!(router.header(), None);
  }

  #[test]
  fn tab_semicolon_and_whitespace_separators_are_kept() {
    assert_eq!(
      route_all("sensor", &["t\tsensor\tv", "0\ta\t1"]),
      vec![head("t\tv"), row("a", "0\t1")]
    );
    assert_eq!(
      route_all("sensor", &["t;sensor;v", "0;a;1"]),
      vec![head("t;v"), row("a", "0;1")]
    );
    assert_eq!(
      route_all("sensor", &["t  sensor v", "0 a   1"]),
      vec![head("t v"), row("a", "0 1")]
    );
  }

  #[test]
  fn comments_are_skipped_and_blank_lines_are_reported_before_the_header() {
    assert_eq!(
      route_all(
        "sensor",
        &["", "   ", "# units: s", "  # x", "t,sensor,v", "0,a,1"]
      ),
      vec![
        Routed::Blank,
        Routed::Blank,
        Routed::Skip,
        Routed::Skip,
        head("t,v"),
        row("a", "0,1"),
      ]
    );
  }

  #[test]
  fn unknown_column_rejects_the_header_and_every_row() {
    let msg = "unknown column 'nope' for --by (columns: t, sensor, v)";
    let mut router = Router::new("nope");
    assert_eq!(router.route("t,sensor,v"), reject(msg));
    assert_eq!(router.route("0,a,1"), reject(msg));
    assert_eq!(router.route("1,b,2"), reject(msg));
    assert_eq!(router.header(), None);
  }

  #[test]
  fn unknown_index_lists_the_columns_too() {
    assert_eq!(
      route_all("9", &["t,sensor,v", "0,a,1"]),
      vec![reject("unknown column '9' for --by (columns: t, sensor, v)"); 2]
    );
  }

  #[test]
  fn named_column_without_header_is_rejected() {
    assert_eq!(
      route_all("sensor", &["a 0 1", "b 1 2"]),
      vec![reject("column 'sensor' needs a header line"); 2]
    );
  }

  #[test]
  fn zero_index_is_rejected() {
    assert_eq!(
      route_all("0", &["a 0 1"]),
      vec![reject("unknown column '0' for --by (columns start at 1)")]
    );
  }

  #[test]
  fn short_row_is_rejected() {
    assert_eq!(
      route_all("3", &["a 0"]),
      vec![reject("row has 2 fields, but --by needs column 3: 'a 0'")]
    );
    assert_eq!(
      route_all("v", &["t,sensor,v", "0,a"]),
      vec![
        head("t,sensor"),
        reject("row has 2 fields, but --by needs column 3: '0,a'"),
      ]
    );
  }

  #[test]
  fn invalid_or_empty_plot_names_are_rejected() {
    assert_eq!(
      route_all(
        "name",
        &["name,v", "a b,1", "x/y,2", "-x,3", ",4", "ok_1.x,5"]
      ),
      vec![
        head("v"),
        reject("'a b' in column name is not a valid plot name"),
        reject("'x/y' in column name is not a valid plot name"),
        reject("'-x' in column name is not a valid plot name"),
        reject("missing plot name: column name is empty"),
        row("ok_1.x", "5"),
      ]
    );
  }

  #[test]
  fn quoted_fields_are_unquoted_like_the_plots_see_them() {
    assert_eq!(
      route_all("sensor", &["\"t\",\"sensor\",v", "0,\"a\",\"1\""]),
      vec![head("t,v"), row("a", "0,1")]
    );
  }

  #[test]
  fn header_is_none_until_a_header_is_routed() {
    let mut router = Router::new("sensor");
    assert_eq!(router.header(), None);
    assert_eq!(router.route("# comment"), Routed::Skip);
    assert_eq!(router.header(), None);
    assert_eq!(router.route("t,sensor,v"), head("t,v"));
    assert_eq!(router.header(), Some("t,v"));
  }

  #[test]
  fn blank_lines_are_reported_and_leave_the_header_alone() {
    let mut router = Router::new("sensor");
    assert_eq!(router.route("t,sensor,v"), head("t,v"));
    assert_eq!(router.route(""), Routed::Blank);
    assert_eq!(router.route("  "), Routed::Blank);
    assert_eq!(router.route("0,a,1"), row("a", "0,1"));
  }

  #[test]
  fn clear_with_names_becomes_the_header() {
    let mut router = Router::new("sensor");
    assert_eq!(router.route("t,sensor,v"), head("t,v"));
    assert_eq!(
      router.route("#clear t,sensor,v"),
      Routed::Clear(Some("t,v".to_string()))
    );
    assert_eq!(router.header(), None);
    assert_eq!(router.route("0,a,1"), row("a", "0,1"));
    assert_eq!(router.header(), Some("t,v"));
  }

  #[test]
  fn clear_alone_lets_a_header_follow() {
    let mut router = Router::new("sensor");
    assert_eq!(router.route("t,sensor,v"), head("t,v"));
    assert_eq!(router.route("#clear"), Routed::Clear(None));
    assert_eq!(router.header(), None);
    assert_eq!(router.route("u,sensor,w"), head("u,w"));
    assert_eq!(router.route("0,b,1"), row("b", "0,1"));
    assert_eq!(router.header(), Some("u,w"));
  }

  #[test]
  fn clear_names_split_on_whitespace_when_there_are_no_commas() {
    let mut router = Router::new("sensor");
    assert_eq!(
      router.route("Starting run 42"),
      reject("column 'sensor' needs a header line")
    );
    assert_eq!(
      router.route("#clear t sensor v"),
      Routed::Clear(Some("t,v".to_string()))
    );
    assert_eq!(router.route("0,a,1"), row("a", "0,1"));
  }

  #[test]
  fn clear_names_are_unquoted_and_empty_ones_dropped() {
    let mut router = Router::new("sensor");
    assert_eq!(
      router.route("#clear \"t\", ,sensor,v"),
      Routed::Clear(Some("t,v".to_string()))
    );
    assert_eq!(router.route("0,a,1"), row("a", "0,1"));
  }

  #[test]
  fn clear_names_may_resolve_the_routing_column_by_index() {
    assert_eq!(
      route_all("2", &["#clear t,sensor,v", "0,a,1"]),
      vec![Routed::Clear(Some("t,v".to_string())), row("a", "0,1")]
    );
  }

  #[test]
  fn clear_names_without_the_routing_column_reject_the_rows() {
    let mut router = Router::new("sensor");
    assert_eq!(
      router.route("#clear t,v"),
      Routed::Clear(Some("t,v".to_string()))
    );
    let msg = "unknown column 'sensor' for --by (columns: t, v)";
    assert_eq!(router.route("0,1"), reject(msg));
    assert_eq!(router.route("1,2"), reject(msg));
    assert_eq!(router.header(), None);
  }

  #[test]
  fn clearance_is_a_comment() {
    let mut router = Router::new("sensor");
    assert_eq!(router.route("#clearance"), Routed::Skip);
    assert_eq!(router.route("t,sensor,v"), head("t,v"));
  }
}
