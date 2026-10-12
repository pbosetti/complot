//! Wire protocol between `complot` clients and a plot server.
//!
//! Client to server: one JSON-encoded [`Msg`] per line; `Hello` comes first.
//! Server to client: plain text lines. After a valid `Hello` the server
//! replies `"complot ok <name>"` ([`GREETING`]); later lines start with
//! [`WARN`] or [`ERROR`]. Each connection is one CSV chunk.
//!
//! Encoding is confined to [`send`] and [`recv`] so it can be swapped.

use crate::model::{Aes, PlotType};
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, Write};

/// Messages sent by a client to a plot server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Msg {
  /// Authenticates the connection with the token of the registry entry.
  Hello { token: String },
  /// Replaces the aesthetic mapping of the plot.
  SetAes { aes: Aes },
  /// A single point; `x: None` means the next sample index of the series.
  Point {
    x: Option<f64>,
    y: f64,
    z: Option<f64>,
    g: Option<String>,
  },
  /// Sets the plot type of the whole plot, or of group `group` only.
  SetType {
    plot_type: PlotType,
    group: Option<String>,
  },
  /// Raw CSV lines, ingested in order as part of this connection's chunk.
  Rows { lines: Vec<String> },
  /// Removes all data, keeping the aesthetic and the last header.
  Clear,
  /// Sets the window title.
  Title { text: String },
  /// Closes the plot window, terminating the server.
  Close,
}

/// First reply of the server after a valid `Hello`, followed by the name.
pub const GREETING: &str = "complot ok";
/// Prefix of a server warning line.
pub const WARN: &str = "warn: ";
/// Prefix of a server error line.
pub const ERROR: &str = "error: ";

/// Writes `msg` as one line. The caller flushes.
pub fn send(w: &mut impl Write, msg: &Msg) -> io::Result<()> {
  serde_json::to_writer(&mut *w, msg)?;
  w.write_all(b"\n")
}

/// Reads the next message, skipping blank lines. `Ok(None)` at end of stream.
pub fn recv(r: &mut impl BufRead) -> io::Result<Option<Msg>> {
  let mut line = String::new();
  loop {
    line.clear();
    if r.read_line(&mut line)? == 0 {
      return Ok(None);
    }
    if !line.trim().is_empty() {
      break;
    }
  }
  serde_json::from_str(&line)
    .map(Some)
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn roundtrip() {
    let msgs = vec![
      Msg::Hello {
        token: "abc".into(),
      },
      Msg::SetAes {
        aes: Aes {
          x: Some("t".into()),
          y: vec!["a".into(), "b".into()],
          g: None,
          z: None,
        },
      },
      Msg::Point {
        x: None,
        y: -1.5,
        z: Some(2.0),
        g: Some("live".into()),
      },
      Msg::SetType {
        plot_type: PlotType::Lines3d,
        group: Some("a".into()),
      },
      Msg::Rows {
        lines: vec!["1,2".into(), "3,4".into()],
      },
      Msg::Clear,
      Msg::Title {
        text: "temp".into(),
      },
      Msg::Close,
    ];
    let mut buf = Vec::new();
    for m in &msgs {
      send(&mut buf, m).unwrap();
    }
    buf.extend_from_slice(b"\n\n");
    let mut r = io::Cursor::new(buf);
    for m in &msgs {
      assert_eq!(recv(&mut r).unwrap().as_ref(), Some(m));
    }
    assert_eq!(recv(&mut r).unwrap(), None);
  }

  #[test]
  fn garbage_is_invalid_data() {
    let mut r = io::Cursor::new(b"not json\n".to_vec());
    let e = recv(&mut r).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidData);
  }
}
