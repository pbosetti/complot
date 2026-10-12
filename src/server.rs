//! Server side: one process per plot, owning the window and the data.

use crate::client::{self, ConnectError};
use crate::gui;
use crate::model::{Chunk, PlotState};
use crate::protocol::{self, ERROR, GREETING, Msg, WARN};
use crate::registry::{self, Entry};
use eframe::egui;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

/// Pause after a failed accept, so that a persistent error does not spin.
const ACCEPT_RETRY: Duration = Duration::from_millis(10);

/// Runs the plot server for `name` until its window is closed.
pub fn run(name: &str) -> Result<(), String> {
  ensure_not_open(name)?;
  let listener =
    TcpListener::bind("127.0.0.1:0").map_err(|e| format!("cannot listen on 127.0.0.1: {e}"))?;
  let port = listener
    .local_addr()
    .map_err(|e| format!("cannot find the listening port: {e}"))?
    .port();
  let token = registry::new_token();
  // Registered only after binding: clients delete entries they cannot reach.
  let entry = Entry {
    name: name.to_string(),
    port,
    token: token.clone(),
    pid: std::process::id(),
  };
  register(&entry)?;

  let state = Arc::new(Mutex::new(PlotState::new()));
  let on_start = {
    let (state, token, name) = (state.clone(), token.clone(), name.to_string());
    Box::new(move |ctx: egui::Context| {
      thread::spawn(move || accept_loop(listener, state, token, name, ctx));
    })
  };
  let result = gui::run(name, state, on_start);
  remove_entry_if(name, &token);
  result
}

/// Fails if a live server already answers for `name`.
fn ensure_not_open(name: &str) -> Result<(), String> {
  match client::connect(name) {
    Ok(_) => Err(format!("plot '{name}' is already open")),
    Err(ConnectError::NotRunning) => Ok(()),
    Err(ConnectError::Failed(msg)) => Err(msg),
  }
}

/// Claims the registry entry, replacing a stale one. Fails if a live server
/// answers for the name, e.g. one that started at the same time.
fn register(entry: &Entry) -> Result<(), String> {
  let name = &entry.name;
  for _ in 0..3 {
    match registry::claim_entry(entry) {
      Ok(true) => return Ok(()),
      // Removes the entry if it is stale, so the next claim can succeed.
      Ok(false) => ensure_not_open(name)?,
      Err(e) => return Err(format!("cannot register plot '{name}': {e}")),
    }
  }
  Err(format!(
    "cannot register plot '{name}': its entry keeps reappearing"
  ))
}

/// Removes the registry entry of `name`, unless another server has replaced it.
fn remove_entry_if(name: &str, token: &str) {
  if registry::read_entry(name).is_some_and(|e| e.token == token) {
    registry::remove_entry(name);
  }
}

/// Accepts connections until the process exits, serving each on its own thread.
fn accept_loop(
  listener: TcpListener,
  state: Arc<Mutex<PlotState>>,
  token: String,
  name: String,
  ctx: egui::Context,
) {
  for stream in listener.incoming() {
    let Ok(stream) = stream else {
      thread::sleep(ACCEPT_RETRY);
      continue;
    };
    let (state, token, name, ctx) = (state.clone(), token.clone(), name.clone(), ctx.clone());
    thread::spawn(move || {
      let repaint = || ctx.request_repaint();
      serve(stream, &state, &token, &name, &repaint);
    });
  }
}

/// Serves one connection. The socket is closed when this returns.
fn serve(stream: TcpStream, state: &Mutex<PlotState>, token: &str, name: &str, repaint: &dyn Fn()) {
  let _ = stream.set_nodelay(true);
  let Ok(read_half) = stream.try_clone() else {
    return;
  };
  let reader = BufReader::new(read_half);
  let writer = BufWriter::new(stream);
  // An I/O error only ends this connection; the client may have gone away.
  let _ = handle_connection(reader, writer, state, token, name, repaint);
}

/// Serves one client connection until it closes.
///
/// `repaint` is called after every change to `state`.
pub fn handle_connection<R: BufRead, W: Write>(
  mut reader: R,
  mut writer: W,
  state: &Mutex<PlotState>,
  token: &str,
  name: &str,
  repaint: &dyn Fn(),
) -> io::Result<()> {
  match protocol::recv(&mut reader) {
    Ok(Some(Msg::Hello { token: given })) if given == token => {}
    Err(e) if e.kind() != io::ErrorKind::InvalidData => return Err(e),
    _ => {
      reply(&mut writer, &format!("{ERROR}authentication failed\n"));
      return Ok(());
    }
  }
  reply(&mut writer, &format!("{GREETING} {name}\n"));

  let mut chunk = Chunk::new();
  // Over the whole connection: rejected lines and accepted points.
  let mut rejected: u64 = 0;
  let mut added: u64 = 0;
  loop {
    let msg = match protocol::recv(&mut reader) {
      Ok(Some(msg)) => msg,
      Ok(None) => break,
      Err(e) if e.kind() == io::ErrorKind::InvalidData => {
        reply(&mut writer, &format!("{ERROR}invalid message: {e}\n"));
        return Ok(());
      }
      Err(e) => return Err(e),
    };
    match msg {
      Msg::Hello { .. } => {}
      Msg::Rows { lines } => {
        let batch = ingest_lines(state, &mut chunk, &lines);
        repaint();
        added += batch.added;
        // Only the first rejection of the connection is reported at once.
        if rejected == 0
          && let Some(e) = &batch.first_error
        {
          reply(&mut writer, &format!("{WARN}{e}\n"));
        }
        rejected += batch.rejected;
      }
      Msg::Point { x, y, g } => {
        {
          let mut st = lock(state);
          let before = accepted_so_far(&st);
          st.push_point(g.as_deref().unwrap_or("y"), x, y);
          added += (accepted_so_far(&st) - before) as u64;
          if st.x_label.is_empty() {
            let label = if x.is_none() { "index" } else { "x" };
            st.x_label = label.to_string();
          }
          if st.y_label.is_empty() {
            st.y_label = "y".to_string();
          }
        }
        repaint();
      }
      Msg::SetAes { aes } => {
        lock(state).set_aes(aes);
        repaint();
      }
      Msg::Title { text } => {
        lock(state).title = Some(text);
        repaint();
      }
      Msg::Clear => {
        lock(state).clear();
        repaint();
      }
      Msg::Close => {
        // Unregister before the client gets our EOF, so that a `--list` or a
        // new plot of the same name right after `--close` doesn't find this
        // server while its window is still closing.
        remove_entry_if(name, token);
        lock(state).close_requested = true;
        repaint();
      }
    }
  }

  if rejected > 0 {
    if added == 0 {
      reply(
        &mut writer,
        &format!("{ERROR}no data plotted, {rejected} rows rejected\n"),
      );
    } else {
      reply(&mut writer, &format!("{WARN}{rejected} rows rejected\n"));
    }
  }
  Ok(())
}

/// What one `Rows` message did to the plot.
struct Batch {
  added: u64,
  rejected: u64,
  /// Message of the first rejected line.
  first_error: Option<String>,
}

/// Ingests `lines` into the plot under a single lock.
fn ingest_lines(state: &Mutex<PlotState>, chunk: &mut Chunk, lines: &[String]) -> Batch {
  let mut st = lock(state);
  let before = accepted_so_far(&st);
  let mut batch = Batch {
    added: 0,
    rejected: 0,
    first_error: None,
  };
  for line in lines {
    if let Err(e) = chunk.ingest(&mut st, line) {
      batch.rejected += 1;
      batch.first_error.get_or_insert(e);
    }
  }
  batch.added = (accepted_so_far(&st) - before) as u64;
  batch
}

/// Total of the series' next sample indices. It grows by one per accepted point.
fn accepted_so_far(st: &PlotState) -> f64 {
  st.series.iter().map(|s| s.next_index).sum()
}

/// Writes one reply line and flushes it. Errors are ignored: the client may be gone.
fn reply(writer: &mut impl Write, line: &str) {
  let _ = writer
    .write_all(line.as_bytes())
    .and_then(|()| writer.flush());
}

/// Locks the plot state. A poisoned lock is still used, since a panic cannot leave the data invalid.
fn lock(state: &Mutex<PlotState>) -> MutexGuard<'_, PlotState> {
  state.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::model::Aes;
  use std::cell::Cell;
  use std::io::Read;
  use std::net::Shutdown;

  const NAME: &str = "plot1";
  const TOKEN: &str = "secret";

  /// Client bytes: a valid `Hello`, then `msgs`.
  fn session(msgs: &[Msg]) -> Vec<u8> {
    let mut buf = Vec::new();
    let hello = Msg::Hello {
      token: TOKEN.into(),
    };
    for m in std::iter::once(&hello).chain(msgs) {
      protocol::send(&mut buf, m).unwrap();
    }
    buf
  }

  /// A `Rows` message of `lines`.
  fn rows(lines: &[&str]) -> Msg {
    Msg::Rows {
      lines: lines.iter().map(|l| l.to_string()).collect(),
    }
  }

  /// Runs one connection over `input`. Returns the server's reply and the number of repaints.
  fn drive(state: &Mutex<PlotState>, input: Vec<u8>) -> (String, usize) {
    // `Close` touches the registry; keep it away from the user's.
    registry::init_test_dir();
    let repaints = Cell::new(0);
    let repaint = || repaints.set(repaints.get() + 1);
    let mut out = Vec::new();
    handle_connection(
      io::Cursor::new(input),
      &mut out,
      state,
      TOKEN,
      NAME,
      &repaint,
    )
    .unwrap();
    (String::from_utf8(out).unwrap(), repaints.get())
  }

  /// Points of series `name` as (x, y) pairs.
  fn points(state: &Mutex<PlotState>, name: &str) -> Option<Vec<(f64, f64)>> {
    let st = lock(state);
    st.series
      .iter()
      .find(|s| s.name == name)
      .map(|s| s.points.iter().map(|p| (p.x, p.y)).collect())
  }

  /// Names of all series, in order.
  fn series_names(state: &Mutex<PlotState>) -> Vec<String> {
    lock(state).series.iter().map(|s| s.name.clone()).collect()
  }

  #[test]
  fn valid_hello_gets_the_greeting() {
    let state = Mutex::new(PlotState::new());
    let (out, repaints) = drive(&state, session(&[]));
    assert_eq!(out, "complot ok plot1\n");
    assert_eq!(repaints, 0);
  }

  #[test]
  fn wrong_token_is_refused_and_state_untouched() {
    let state = Mutex::new(PlotState::new());
    let mut input = Vec::new();
    protocol::send(
      &mut input,
      &Msg::Hello {
        token: "guess".into(),
      },
    )
    .unwrap();
    protocol::send(&mut input, &rows(&["t,a", "0,1"])).unwrap();
    let (out, repaints) = drive(&state, input);
    assert_eq!(out, "error: authentication failed\n");
    assert_eq!(repaints, 0);
    let st = lock(&state);
    assert!(st.series.is_empty());
    assert!(st.header.is_none());
  }

  #[test]
  fn first_message_must_be_hello() {
    let state = Mutex::new(PlotState::new());
    let mut input = Vec::new();
    protocol::send(&mut input, &rows(&["t,a", "0,1"])).unwrap();
    let (out, _) = drive(&state, input);
    assert_eq!(out, "error: authentication failed\n");
    let (out, _) = drive(&state, b"garbage\n".to_vec());
    assert_eq!(out, "error: authentication failed\n");
    assert!(lock(&state).series.is_empty());
  }

  #[test]
  fn repeated_hello_is_ignored() {
    let state = Mutex::new(PlotState::new());
    let (out, repaints) = drive(
      &state,
      session(&[Msg::Hello {
        token: TOKEN.into(),
      }]),
    );
    assert_eq!(out, "complot ok plot1\n");
    assert_eq!(repaints, 0);
  }

  #[test]
  fn rows_header_and_data_fill_series() {
    let state = Mutex::new(PlotState::new());
    let (out, repaints) = drive(
      &state,
      session(&[
        rows(&["t,a,b", "0,1,2", "1,3,4"]),
        // The header of the first message still applies to this one.
        rows(&["2,5,6"]),
      ]),
    );
    assert_eq!(out, "complot ok plot1\n");
    assert_eq!(repaints, 2);
    assert_eq!(series_names(&state), ["a", "b"]);
    assert_eq!(
      points(&state, "a"),
      Some(vec![(0.0, 1.0), (1.0, 3.0), (2.0, 5.0)])
    );
    assert_eq!(
      points(&state, "b"),
      Some(vec![(0.0, 2.0), (1.0, 4.0), (2.0, 6.0)])
    );
    let st = lock(&state);
    assert_eq!(st.x_label, "t");
    assert_eq!(st.y_label, "");
  }

  #[test]
  fn point_without_group_goes_to_series_y() {
    let state = Mutex::new(PlotState::new());
    let (out, repaints) = drive(
      &state,
      session(&[
        Msg::Point {
          x: None,
          y: 2.5,
          g: None,
        },
        Msg::Point {
          x: None,
          y: 3.5,
          g: None,
        },
      ]),
    );
    assert_eq!(out, "complot ok plot1\n");
    assert_eq!(repaints, 2);
    assert_eq!(points(&state, "y"), Some(vec![(0.0, 2.5), (1.0, 3.5)]));
    let st = lock(&state);
    assert_eq!(st.x_label, "index");
    assert_eq!(st.y_label, "y");
  }

  #[test]
  fn point_with_x_and_group_labels_the_x_axis() {
    let state = Mutex::new(PlotState::new());
    drive(
      &state,
      session(&[Msg::Point {
        x: Some(5.0),
        y: 1.0,
        g: Some("k".into()),
      }]),
    );
    assert_eq!(points(&state, "k"), Some(vec![(5.0, 1.0)]));
    let st = lock(&state);
    assert_eq!(st.x_label, "x");
    assert_eq!(st.y_label, "y");
  }

  #[test]
  fn set_aes_applies_to_the_rows_that_follow() {
    let state = Mutex::new(PlotState::new());
    let (_, repaints) = drive(
      &state,
      session(&[
        rows(&["t,a,b", "0,1,2"]),
        Msg::SetAes {
          aes: Aes::parse("x=t y=b").unwrap(),
        },
        rows(&["1,3,4"]),
      ]),
    );
    assert_eq!(repaints, 3);
    assert_eq!(points(&state, "a"), Some(vec![(0.0, 1.0)]));
    assert_eq!(points(&state, "b"), Some(vec![(0.0, 2.0), (1.0, 4.0)]));
    assert_eq!(lock(&state).y_label, "b");
  }

  #[test]
  fn title_clear_and_close_change_state() {
    registry::init_test_dir();
    let entry = Entry {
      name: NAME.into(),
      port: 1,
      token: TOKEN.into(),
      pid: 1,
    };
    registry::write_entry(&entry).unwrap();
    let state = Mutex::new(PlotState::new());
    let (out, repaints) = drive(
      &state,
      session(&[
        rows(&["t,a", "0,1"]),
        Msg::Title {
          text: "temp".into(),
        },
        Msg::Clear,
        Msg::Close,
      ]),
    );
    assert_eq!(out, "complot ok plot1\n");
    assert_eq!(repaints, 4);
    let st = lock(&state);
    assert_eq!(st.title.as_deref(), Some("temp"));
    assert!(st.series.is_empty());
    assert!(st.close_requested);
    // Unregistered at once, before the window has closed.
    assert_eq!(registry::read_entry(NAME), None);
  }

  #[test]
  fn bad_row_gives_one_warning_and_a_summary() {
    let state = Mutex::new(PlotState::new());
    let (out, _) = drive(&state, session(&[rows(&["t,a", "0,1", "x,2", "1,3"])]));
    assert_eq!(
      out,
      "complot ok plot1\n\
       warn: cannot parse x value 'x' in column 't'\n\
       warn: 1 rows rejected\n"
    );
    assert_eq!(points(&state, "a"), Some(vec![(0.0, 1.0), (1.0, 3.0)]));
  }

  #[test]
  fn only_the_first_rejection_is_sent_at_once() {
    let state = Mutex::new(PlotState::new());
    let (out, _) = drive(
      &state,
      session(&[rows(&["t,a", "0,1", "x,2"]), rows(&["1,3", "zz,4"])]),
    );
    assert_eq!(
      out,
      "complot ok plot1\n\
       warn: cannot parse x value 'x' in column 't'\n\
       warn: 2 rows rejected\n"
    );
  }

  #[test]
  fn all_rows_rejected_is_an_error() {
    let state = Mutex::new(PlotState::new());
    let (out, _) = drive(
      &state,
      session(&[
        Msg::SetAes {
          aes: Aes::parse("y=zz").unwrap(),
        },
        rows(&["t,a", "0,1", "1,2"]),
      ]),
    );
    assert_eq!(
      out,
      "complot ok plot1\n\
       warn: unknown column 'zz' (columns: t, a)\n\
       error: no data plotted, 2 rows rejected\n"
    );
    assert!(lock(&state).series.is_empty());
  }

  #[test]
  fn invalid_json_is_reported() {
    let state = Mutex::new(PlotState::new());
    let mut input = session(&[]);
    input.extend_from_slice(b"not json\n");
    let (out, _) = drive(&state, input);
    assert!(
      out.starts_with("complot ok plot1\nerror: invalid message: "),
      "{out}"
    );
    assert!(out.ends_with('\n'));
  }

  /// A reader whose every read fails.
  struct Broken;

  impl Read for Broken {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
      Err(io::Error::from(io::ErrorKind::TimedOut))
    }
  }

  #[test]
  fn other_io_errors_are_returned() {
    let state = Mutex::new(PlotState::new());
    let mut out = Vec::new();
    let err = handle_connection(
      BufReader::new(Broken),
      &mut out,
      &state,
      TOKEN,
      NAME,
      &|| {},
    )
    .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
  }

  #[test]
  fn socket_closes_when_the_client_is_done() {
    let state = Arc::new(Mutex::new(PlotState::new()));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server_state = state.clone();
    let server = thread::spawn(move || {
      let (stream, _) = listener.accept().unwrap();
      serve(stream, &server_state, TOKEN, NAME, &|| {});
    });

    let mut client = TcpStream::connect(addr).unwrap();
    client
      .set_read_timeout(Some(Duration::from_secs(5)))
      .unwrap();
    client
      .write_all(&session(&[rows(&["t,a", "0,1"])]))
      .unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    // Returns only once the server has closed its side.
    let mut reply = String::new();
    client.read_to_string(&mut reply).unwrap();
    assert_eq!(reply, "complot ok plot1\n");
    server.join().unwrap();
    assert_eq!(points(&state, "a"), Some(vec![(0.0, 1.0)]));
  }

  #[test]
  fn ensure_not_open_refuses_a_live_plot() {
    registry::init_test_dir();
    let name = "sv_open";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let token = registry::new_token();
    let entry = Entry {
      name: name.into(),
      port: listener.local_addr().unwrap().port(),
      token: token.clone(),
      pid: std::process::id(),
    };
    registry::write_entry(&entry).unwrap();
    let fake = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      let mut reader = BufReader::new(stream.try_clone().unwrap());
      assert_eq!(
        protocol::recv(&mut reader).unwrap(),
        Some(Msg::Hello { token })
      );
      stream.write_all(b"complot ok sv_open\n").unwrap();
    });

    assert_eq!(
      ensure_not_open(name),
      Err("plot 'sv_open' is already open".to_string())
    );
    fake.join().unwrap();
    registry::remove_entry(name);
  }

  #[test]
  fn ensure_not_open_accepts_a_free_name() {
    registry::init_test_dir();
    assert_eq!(ensure_not_open("sv_free"), Ok(()));
  }

  #[test]
  fn register_replaces_a_stale_entry_but_not_a_fresh_claim() {
    registry::init_test_dir();
    let name = "sv_claim";
    let stale_port = TcpListener::bind("127.0.0.1:0")
      .unwrap()
      .local_addr()
      .unwrap()
      .port();
    let entry = |port, token: &str| Entry {
      name: name.into(),
      port,
      token: token.into(),
      pid: 1,
    };
    registry::write_entry(&entry(stale_port, "old")).unwrap();
    assert_eq!(register(&entry(2, "new")), Ok(()));
    assert_eq!(registry::read_entry(name), Some(entry(2, "new")));
    // A second claim must not overwrite the first.
    assert!(!registry::claim_entry(&entry(3, "late")).unwrap());
    assert_eq!(registry::read_entry(name), Some(entry(2, "new")));
    registry::remove_entry(name);
  }

  #[test]
  fn remove_entry_if_keeps_an_entry_of_another_server() {
    registry::init_test_dir();
    let name = "sv_rm";
    let entry = Entry {
      name: name.into(),
      port: 1,
      token: "mine".into(),
      pid: 1,
    };
    registry::write_entry(&entry).unwrap();
    remove_entry_if(name, "theirs");
    assert_eq!(registry::read_entry(name), Some(entry));
    remove_entry_if(name, "mine");
    assert_eq!(registry::read_entry(name), None);
  }
}
