//! Client side: connecting to plot servers, spawning them, pushing data.

use crate::Cli;
use crate::model::{Aes, PlotType};
use crate::protocol::{self, ERROR, GREETING, Msg};
use crate::registry;
use clap::ValueEnum;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Timeout for the TCP connect and for the handshake reply.
const TIMEOUT: Duration = Duration::from_secs(2);
/// How long `spawn` waits for the new server to accept connections.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
/// Lines of the server log quoted when a server fails to start.
const LOG_LINES: usize = 10;
/// Most CSV lines sent in one `Rows` message.
const BATCH_LINES: usize = 4096;
/// Read buffer for CSV sources.
const READ_BUFFER: usize = 65536;

/// An authenticated connection to a plot server.
pub struct Connection {
  pub reader: BufReader<TcpStream>,
  pub writer: BufWriter<TcpStream>,
}

#[derive(Debug)]
pub enum ConnectError {
  /// No live server for this name (stale entries are removed).
  NotRunning,
  Failed(String),
}

/// Connects to the server of `name` and performs the `Hello` handshake.
pub fn connect(name: &str) -> Result<Connection, ConnectError> {
  let entry = registry::read_entry(name).ok_or(ConnectError::NotRunning)?;
  let addr = SocketAddr::from(([127, 0, 0, 1], entry.port));
  let stream = TcpStream::connect_timeout(&addr, TIMEOUT).map_err(|_| stale(name))?;
  stream.set_nodelay(true).map_err(|e| failed(name, e))?;
  let reader = stream.try_clone().map_err(|e| failed(name, e))?;
  let mut conn = Connection {
    reader: BufReader::new(reader),
    writer: BufWriter::new(stream),
  };

  let hello = protocol::send(&mut conn.writer, &Msg::Hello { token: entry.token })
    .and_then(|()| conn.writer.flush());
  hello.map_err(|e| handshake_error(name, e))?;

  conn
    .reader
    .get_ref()
    .set_read_timeout(Some(TIMEOUT))
    .map_err(|e| failed(name, e))?;
  let expected = format!("{GREETING} {name}");
  let mut line = String::new();
  match conn.reader.read_line(&mut line) {
    Ok(n) if n > 0 && line.trim_end_matches(['\r', '\n']) == expected => {}
    Ok(_) => return Err(stale(name)),
    Err(e) => return Err(handshake_error(name, e)),
  }
  conn
    .reader
    .get_ref()
    .set_read_timeout(None)
    .map_err(|e| failed(name, e))?;
  Ok(conn)
}

/// Drops the registry entry of a server that is gone.
fn stale(name: &str) -> ConnectError {
  registry::remove_entry(name);
  ConnectError::NotRunning
}

fn failed(name: &str, e: io::Error) -> ConnectError {
  ConnectError::Failed(format!("cannot connect to plot '{name}': {e}"))
}

/// An I/O error during the handshake: a server that went away or spoke
/// nonsense is not running; anything else is a failure.
fn handshake_error(name: &str, e: io::Error) -> ConnectError {
  use io::ErrorKind::*;
  match e.kind() {
    UnexpectedEof | InvalidData | ConnectionReset | ConnectionAborted | BrokenPipe | TimedOut
    | WouldBlock => stale(name),
    _ => failed(name, e),
  }
}

/// Spawns a detached server for `name` and waits until it accepts connections.
pub fn spawn(name: &str) -> Result<(), String> {
  let exe =
    std::env::current_exe().map_err(|e| format!("cannot find the complot executable: {e}"))?;
  let mut cmd = Command::new(exe);
  // stdout must not be inherited, or `h=$(complot)` would wait for the server.
  cmd
    .arg("--serve")
    .arg(name)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(server_log(name));
  #[cfg(unix)]
  {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
  }
  #[cfg(windows)]
  {
    use std::os::windows::process::CommandExt;
    // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
    cmd.creation_flags(0x0000_0008 | 0x0000_0200);
  }
  let mut child = cmd
    .spawn()
    .map_err(|e| format!("cannot start plot '{name}': {e}"))?;

  let deadline = Instant::now() + SPAWN_TIMEOUT;
  loop {
    if connect(name).is_ok() {
      // Dropping `child` does not kill the server.
      return Ok(());
    }
    match child.try_wait() {
      Ok(Some(status)) => {
        return Err(format!("plot '{name}' exited ({status}){}", log_tail(name)));
      }
      Ok(None) => {}
      Err(e) => return Err(format!("cannot wait for plot '{name}': {e}")),
    }
    if Instant::now() >= deadline {
      let _ = child.kill();
      let _ = child.wait();
      return Err(format!(
        "plot '{name}' did not start within {} s{}",
        SPAWN_TIMEOUT.as_secs(),
        log_tail(name)
      ));
    }
    thread::sleep(POLL_INTERVAL);
  }
}

/// Server stderr goes to the plot's log file, or nowhere if that fails.
fn server_log(name: &str) -> Stdio {
  match File::create(registry::log_path(name)) {
    Ok(file) => Stdio::from(file),
    Err(_) => Stdio::null(),
  }
}

/// The last lines of the server log of `name`, for error messages.
fn log_tail(name: &str) -> String {
  let text = fs::read_to_string(registry::log_path(name)).unwrap_or_default();
  let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
  lines[lines.len().saturating_sub(LOG_LINES)..]
    .iter()
    .map(|line| format!("\n  {line}"))
    .collect()
}

/// The first `plotN` (N = 1, 2, ...) that no server is running for.
fn auto_name() -> String {
  (1..)
    .map(|i| format!("plot{i}"))
    .find(|name| matches!(connect(name), Err(ConnectError::NotRunning)))
    .expect("an unused plot name exists")
}

/// Message for a failed connection to plot `name`.
fn connect_error(name: &str, e: ConnectError) -> String {
  match e {
    ConnectError::NotRunning => format!("plot '{name}' was closed"),
    ConnectError::Failed(msg) => msg,
  }
}

/// Message for an I/O error while writing to plot `name`.
fn write_error(name: &str, e: io::Error) -> String {
  match e.kind() {
    io::ErrorKind::BrokenPipe
    | io::ErrorKind::ConnectionReset
    | io::ErrorKind::ConnectionAborted => format!("plot '{name}' was closed"),
    _ => format!("cannot write to plot '{name}': {e}"),
  }
}

/// An open connection, with a thread that prints the server's replies so
/// that the server never blocks on a full socket.
struct Session {
  name: String,
  writer: BufWriter<TcpStream>,
  replies: JoinHandle<bool>,
}

impl Session {
  /// Starts printing the replies on `conn` right away.
  fn open(name: &str, conn: Connection) -> Session {
    let Connection { mut reader, writer } = conn;
    let replies = thread::spawn(move || {
      let mut failed = false;
      let mut raw = Vec::new();
      loop {
        raw.clear();
        match reader.read_until(b'\n', &mut raw) {
          Ok(0) | Err(_) => return failed,
          Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&raw);
        let line = text.trim_end_matches(['\r', '\n']);
        failed |= line.starts_with(ERROR);
        eprintln!("complot: {line}");
      }
    });
    Session {
      name: name.to_string(),
      writer,
      replies,
    }
  }

  /// Sends one message and flushes it.
  fn send(&mut self, msg: &Msg) -> Result<(), String> {
    protocol::send(&mut self.writer, msg)
      .and_then(|()| self.writer.flush())
      .map_err(|e| write_error(&self.name, e))
  }

  /// Ends the connection and waits for the server to close its side.
  /// Returns true if the server reported an error.
  fn finish(self) -> Result<bool, String> {
    let Session {
      name,
      mut writer,
      replies,
    } = self;
    writer.flush().map_err(|e| write_error(&name, e))?;
    // EOF tells the server this chunk is complete; it then closes the socket.
    let _ = writer.get_ref().shutdown(Shutdown::Write);
    replies
      .join()
      .map_err(|_| format!("reply reader of plot '{name}' panicked"))
  }
}

fn read_error(path: &Path, e: io::Error) -> String {
  format!("cannot read '{}': {e}", path.display())
}

/// Pushes the lines of `path` (`-` for stdin) over a connection of their own,
/// so each file is one CSV chunk. Returns true if the server reported an error.
fn push_file(name: &str, path: &Path) -> Result<bool, String> {
  let source: Box<dyn Read> = if path == Path::new("-") {
    Box::new(io::stdin())
  } else {
    Box::new(File::open(path).map_err(|e| read_error(path, e))?)
  };
  let mut lines = BufReader::with_capacity(READ_BUFFER, source);
  let conn = connect(name).map_err(|e| connect_error(name, e))?;
  let mut session = Session::open(name, conn);

  let mut batch = Vec::new();
  let mut raw = Vec::new();
  loop {
    raw.clear();
    if lines
      .read_until(b'\n', &mut raw)
      .map_err(|e| read_error(path, e))?
      == 0
    {
      break;
    }
    if raw.last() == Some(&b'\n') {
      raw.pop();
    }
    if raw.last() == Some(&b'\r') {
      raw.pop();
    }
    batch.push(String::from_utf8_lossy(&raw).into_owned());
    // Send now if the next read would block (no complete line is buffered),
    // so that a live stream such as `tail -f` is shown in real time.
    if batch.len() >= BATCH_LINES || !lines.buffer().contains(&b'\n') {
      session.send(&Msg::Rows {
        lines: std::mem::take(&mut batch),
      })?;
    }
  }
  if !batch.is_empty() {
    session.send(&Msg::Rows { lines: batch })?;
  }
  session.finish()
}

/// The `--list-types` text: one line per plot type, with its name and help.
fn types_text() -> String {
  PlotType::value_variants()
    .iter()
    .filter_map(|t| t.to_possible_value())
    .map(|v| {
      let help = v.get_help().map(ToString::to_string).unwrap_or_default();
      format!("{:<6}  {help}\n", v.get_name())
    })
    .collect()
}

/// Runs a client invocation; never called with `--serve`.
pub fn run(cli: &Cli) -> ExitCode {
  match execute(cli) {
    Ok(false) => ExitCode::SUCCESS,
    Ok(true) => ExitCode::FAILURE,
    Err(msg) => {
      eprintln!("complot: {msg}");
      ExitCode::FAILURE
    }
  }
}

/// Does the work of [`run`]. `Ok(true)` if the server reported an error.
fn execute(cli: &Cli) -> Result<bool, String> {
  if cli.list {
    for name in registry::names() {
      if connect(&name).is_ok() {
        // Ignore write errors so that `complot --list | head -1` works.
        let _ = writeln!(io::stdout(), "{name}");
      }
    }
    return Ok(false);
  }
  if cli.close_all {
    return close_plots(&registry::names());
  }
  if cli.list_types {
    // Ignore write errors so that `complot --list-types | head -1` works.
    let _ = write!(io::stdout(), "{}", types_text());
    return Ok(false);
  }

  let has_data = !cli.files.is_empty() || cli.y.is_some();
  let name = match &cli.name {
    Some(name) => name.clone(),
    None if has_data => return Err("a plot name is required to push data".into()),
    None if cli.close => return Err("a plot name is required to close a plot".into()),
    None => auto_name(),
  };
  if !registry::valid_name(&name) {
    return Err(format!("invalid plot name '{name}'"));
  }
  // Non-finite numbers have no JSON encoding.
  if [cli.x, cli.y, cli.z]
    .iter()
    .flatten()
    .any(|v| !v.is_finite())
  {
    return Err("-x, -y and -z must be finite numbers".into());
  }
  if cli.g.is_some() && cli.y.is_none() && cli.plot_type.is_none() {
    return Err("-g needs -y (a point) or -t (the group's type)".into());
  }
  let aes = cli
    .aes
    .as_deref()
    .map(Aes::parse)
    .transpose()
    .map_err(|e| format!("invalid --aes: {e}"))?;

  let first = match connect(&name) {
    Ok(conn) => conn,
    Err(ConnectError::Failed(msg)) => return Err(msg),
    Err(ConnectError::NotRunning) if cli.close => return Ok(false),
    Err(ConnectError::NotRunning) if has_data && !cli.create => {
      return Err(format!(
        "plot '{name}' is not open (create it with `complot {name}` or pass --create)"
      ));
    }
    Err(ConnectError::NotRunning) => {
      spawn(&name)?;
      connect(&name).map_err(|e| connect_error(&name, e))?
    }
  };

  let mut failed = false;
  let mut session = Session::open(&name, first);
  if let Some(aes) = aes {
    session.send(&Msg::SetAes { aes })?;
  }
  if let Some(text) = &cli.title {
    session.send(&Msg::Title { text: text.clone() })?;
  }
  if cli.clear {
    session.send(&Msg::Clear)?;
  }
  // Before the point, so that a new 3D plot accepts it.
  if let Some(plot_type) = cli.plot_type {
    session.send(&Msg::SetType {
      plot_type,
      group: cli.g.clone(),
    })?;
  }
  if let Some(y) = cli.y {
    session.send(&Msg::Point {
      x: cli.x,
      y,
      z: cli.z,
      g: cli.g.clone(),
    })?;
  }
  failed |= session.finish()?;

  for path in &cli.files {
    failed |= push_file(&name, path)?;
  }

  if cli.close {
    let conn = connect(&name).map_err(|e| connect_error(&name, e))?;
    failed |= close(&name, conn)?;
  }

  if !has_data && !cli.close {
    // The handle, for `h=$(complot)`.
    let _ = writeln!(io::stdout(), "{name}");
  }
  Ok(failed)
}

/// Sends `Close` on `conn`. Returns true if the server reported an error.
fn close(name: &str, conn: Connection) -> Result<bool, String> {
  let mut session = Session::open(name, conn);
  session.send(&Msg::Close)?;
  session.finish()
}

/// Closes every plot in `names` that is open, skipping the others.
/// Returns true if any of them could not be closed.
fn close_plots(names: &[String]) -> Result<bool, String> {
  let mut failed = false;
  for name in names {
    let result = match connect(name) {
      Ok(conn) => close(name, conn),
      Err(ConnectError::NotRunning) => Ok(false),
      Err(ConnectError::Failed(msg)) => Err(msg),
    };
    match result {
      Ok(f) => failed |= f,
      Err(msg) => {
        eprintln!("complot: {msg}");
        failed = true;
      }
    }
  }
  Ok(failed)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::net::TcpListener;

  /// Registers `name` as served by `listener`, returning the token.
  fn register(name: &str, listener: &TcpListener) -> String {
    let token = registry::new_token();
    let entry = registry::Entry {
      name: name.into(),
      port: listener.local_addr().unwrap().port(),
      token: token.clone(),
      pid: std::process::id(),
    };
    registry::write_entry(&entry).unwrap();
    token
  }

  /// Serves one connection: checks the `Hello` token, sends the greeting,
  /// returns the messages up to the client's EOF, then writes `trailer`.
  fn fake_server(
    listener: TcpListener,
    name: &str,
    token: &str,
    trailer: &'static str,
  ) -> JoinHandle<Vec<Msg>> {
    let (name, token) = (name.to_string(), token.to_string());
    thread::spawn(move || {
      let (stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
      let mut writer = stream.try_clone().unwrap();
      let mut reader = BufReader::new(stream);
      assert_eq!(
        protocol::recv(&mut reader).unwrap(),
        Some(Msg::Hello { token })
      );
      writer
        .write_all(format!("{GREETING} {name}\n").as_bytes())
        .unwrap();
      let mut msgs = Vec::new();
      while let Some(msg) = protocol::recv(&mut reader).unwrap() {
        msgs.push(msg);
      }
      writer.write_all(trailer.as_bytes()).unwrap();
      msgs
    })
  }

  #[test]
  fn connect_handshakes_and_sends_messages() {
    registry::init_test_dir();
    let name = "cl_ok";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let token = register(name, &listener);
    let server = fake_server(listener, name, &token, "");

    let mut conn = connect(name).unwrap();
    protocol::send(&mut conn.writer, &Msg::Clear).unwrap();
    conn.writer.flush().unwrap();
    drop(conn);

    assert_eq!(server.join().unwrap(), vec![Msg::Clear]);
    registry::remove_entry(name);
  }

  #[test]
  fn stale_entry_is_not_running_and_removed() {
    registry::init_test_dir();
    let name = "cl_stale";
    let port = TcpListener::bind("127.0.0.1:0")
      .unwrap()
      .local_addr()
      .unwrap()
      .port();
    let entry = registry::Entry {
      name: name.into(),
      port,
      token: "t".into(),
      pid: 0,
    };
    registry::write_entry(&entry).unwrap();

    assert!(matches!(connect(name), Err(ConnectError::NotRunning)));
    assert!(!registry::entry_path(name).exists());
  }

  #[test]
  fn wrong_greeting_is_not_running() {
    registry::init_test_dir();
    let name = "cl_bad";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    register(name, &listener);
    let server = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      let _ = protocol::recv(&mut BufReader::new(stream.try_clone().unwrap()));
      stream.write_all(b"complot ok someone_else\n").unwrap();
    });

    assert!(matches!(connect(name), Err(ConnectError::NotRunning)));
    server.join().unwrap();
    assert!(!registry::entry_path(name).exists());
  }

  #[test]
  fn silent_server_is_not_running() {
    registry::init_test_dir();
    let name = "cl_silent";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    register(name, &listener);
    let server = thread::spawn(move || {
      let (stream, _) = listener.accept().unwrap();
      let _ = protocol::recv(&mut BufReader::new(stream));
    });

    assert!(matches!(connect(name), Err(ConnectError::NotRunning)));
    server.join().unwrap();
    assert!(!registry::entry_path(name).exists());
  }

  #[test]
  fn file_lines_are_sent_in_batches() {
    registry::init_test_dir();
    let name = "cl_rows";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let token = register(name, &listener);
    let server = fake_server(listener, name, &token, "");

    let path = registry::dir().join("cl_rows.csv");
    let total = BATCH_LINES + 10;
    let text: String = (0..total)
      .map(|i| {
        if i % 2 == 0 {
          format!("{i},x\r\n")
        } else {
          format!("{i},y\n")
        }
      })
      .collect();
    fs::write(&path, text).unwrap();
    let failed = push_file(name, &path);
    fs::remove_file(&path).unwrap();
    assert_eq!(failed, Ok(false));
    registry::remove_entry(name);

    let batches: Vec<Vec<String>> = server
      .join()
      .unwrap()
      .into_iter()
      .map(|msg| match msg {
        Msg::Rows { lines } => lines,
        other => panic!("unexpected message {other:?}"),
      })
      .collect();
    assert_eq!(
      batches.iter().map(Vec::len).collect::<Vec<_>>(),
      vec![BATCH_LINES, 10]
    );
    assert_eq!(batches[0][0], "0,x");
    assert_eq!(batches[0][1], "1,y");
    assert_eq!(
      batches[0][BATCH_LINES - 1],
      format!("{},y", BATCH_LINES - 1)
    );
    assert_eq!(batches[1][9], format!("{},y", total - 1));
  }

  #[test]
  fn close_plots_closes_open_ones_and_skips_stale() {
    registry::init_test_dir();
    let mut servers = Vec::new();
    for name in ["cl_all1", "cl_all2"] {
      let listener = TcpListener::bind("127.0.0.1:0").unwrap();
      let token = register(name, &listener);
      servers.push(fake_server(listener, name, &token, ""));
    }
    let names: Vec<String> = ["cl_all1", "cl_gone", "cl_all2"].map(String::from).into();

    assert_eq!(close_plots(&names), Ok(false));
    for server in servers {
      assert_eq!(server.join().unwrap(), vec![Msg::Close]);
    }
    registry::remove_entry("cl_all1");
    registry::remove_entry("cl_all2");
  }

  #[test]
  fn server_errors_are_reported() {
    registry::init_test_dir();
    let name = "cl_err";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let token = register(name, &listener);
    let server = fake_server(listener, name, &token, "error: no such column\n");

    let path = registry::dir().join("cl_err.csv");
    fs::write(&path, "a,b\n1,2\n").unwrap();
    let failed = push_file(name, &path);
    fs::remove_file(&path).unwrap();
    assert_eq!(failed, Ok(true));
    server.join().unwrap();
    registry::remove_entry(name);
  }

  #[test]
  fn list_types_names_every_type_with_its_help() {
    let text = types_text();
    assert_eq!(text.lines().count(), 6, "{text}");
    for line in [
      "point   Points only",
      "lines   Lines only",
      "both    Lines and points",
      "hist    Histogram of y by group (whole plot)",
      "3dl     3D lines (needs x, y and z)",
      "3dp     3D points and point clouds (needs x, y and z)",
    ] {
      assert!(
        text.lines().any(|l| l == line),
        "no line {line:?} in {text:?}"
      );
    }
  }

  #[test]
  fn point_options_are_checked_before_connecting() {
    use clap::Parser;
    let parse = |args: &[&str]| Cli::try_parse_from(args).unwrap();
    assert_eq!(
      execute(&parse(&["complot", "p", "-g", "a"])),
      Err("-g needs -y (a point) or -t (the group's type)".to_string())
    );
    assert_eq!(
      execute(&parse(&["complot", "p", "-y", "1", "-z", "inf"])),
      Err("-x, -y and -z must be finite numbers".to_string())
    );
  }

  #[test]
  fn type_is_sent_before_the_point() {
    use clap::Parser;
    registry::init_test_dir();
    let name = "cl_type";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let token = register(name, &listener);
    let server = fake_server(listener, name, &token, "");

    let cli = Cli::try_parse_from([
      "complot", name, "-t", "3dp", "-g", "k", "-y", "1", "-z", "2",
    ])
    .unwrap();
    assert_eq!(execute(&cli), Ok(false));
    assert_eq!(
      server.join().unwrap(),
      vec![
        Msg::SetType {
          plot_type: PlotType::Points3d,
          group: Some("k".into()),
        },
        Msg::Point {
          x: None,
          y: 1.0,
          z: Some(2.0),
          g: Some("k".into()),
        },
      ]
    );
    registry::remove_entry(name);
  }
}
