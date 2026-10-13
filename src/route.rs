//! `--by COL`: routes the rows of a CSV chunk to the plots named by the values
//! of one column, which is removed from the forwarded lines.

use crate::model::{Sep, column_index, detect_sep, is_header, split_fields};
use crate::registry::valid_name;

/// What to do with one line of a chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Routed {
  /// A blank line or a comment: nothing to send.
  Skip,
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
  /// Separator of the chunk, decided by its first non-skipped line.
  sep: Option<Sep>,
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
      index: None,
      header: None,
      failure: None,
    }
  }

  /// Classifies the next line of the chunk.
  pub fn route(&mut self, line: &str) -> Routed {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
      return Routed::Skip;
    }
    let first = self.sep.is_none();
    let sep = *self.sep.get_or_insert_with(|| detect_sep(line));
    let mut fields = split_fields(line, sep);
    if first && is_header(&fields) {
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
        let reason = format!(
          "unknown column '{}' for --by (columns: {})",
          self.col,
          fields.join(", ")
        );
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
  fn comments_and_blank_lines_are_skipped_before_the_header() {
    assert_eq!(
      route_all(
        "sensor",
        &["", "   ", "# units: s", "  # x", "t,sensor,v", "0,a,1"]
      ),
      vec![
        Routed::Skip,
        Routed::Skip,
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
}
