//! Client side: connecting to plot servers, spawning them, pushing data.

use crate::Cli;
use crate::args::{self, Invocation};
use crate::model::{Aes, PlotType};
use crate::protocol::{self, ERROR, GREETING, Msg};
use crate::registry;
use crate::route::{Routed, Router};
use clap::ValueEnum;
use std::collections::{BTreeMap, HashSet};
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

/// Opens a CSV source (`-` for stdin) with a read buffer.
fn open_source(path: &Path) -> Result<BufReader<Box<dyn Read>>, String> {
  let source: Box<dyn Read> = if path == Path::new("-") {
    Box::new(io::stdin())
  } else {
    Box::new(File::open(path).map_err(|e| read_error(path, e))?)
  };
  Ok(BufReader::with_capacity(READ_BUFFER, source))
}

/// Removes the line break that ends `raw`: a `\n`, then a `\r`.
fn strip_newline(raw: &mut Vec<u8>) {
  if raw.last() == Some(&b'\n') {
    raw.pop();
  }
  if raw.last() == Some(&b'\r') {
    raw.pop();
  }
}

/// What pushing one source did.
#[derive(Debug, Default)]
struct Push {
  /// The server reported an error, or a plot was lost.
  failed: bool,
  /// The plots that were lost: their windows closed, or they could not be reached.
  closed: Vec<String>,
}

impl Push {
  /// Reports that `plot` gets nothing more from this source.
  fn lose(&mut self, plot: &str, msg: String) {
    eprintln!("complot: {msg}");
    self.failed = true;
    self.closed.push(plot.to_string());
  }
}

/// Pushes the lines of `path` (`-` for stdin) to every plot in `plots`, each
/// over a connection of its own, so that each file is one CSV chunk per plot.
/// The source is read once, and each batch goes to every plot still open. A
/// plot that is lost is reported and dropped; the others go on, and the
/// source is not read any further once none is left.
fn push_file(plots: &[String], path: &Path) -> Result<Push, String> {
  let mut lines = open_source(path)?;
  let mut push = Push::default();
  let mut sessions = Vec::new();
  for plot in plots {
    match connect(plot) {
      Ok(conn) => sessions.push(Session::open(plot, conn)),
      Err(e) => push.lose(plot, connect_error(plot, e)),
    }
  }

  let mut batch = Vec::new();
  let mut raw = Vec::new();
  while !sessions.is_empty() {
    raw.clear();
    if lines
      .read_until(b'\n', &mut raw)
      .map_err(|e| read_error(path, e))?
      == 0
    {
      break;
    }
    strip_newline(&mut raw);
    batch.push(String::from_utf8_lossy(&raw).into_owned());
    // Send now if the next read would block (no complete line is buffered),
    // so that a live stream such as `tail -f` is shown in real time.
    if batch.len() >= BATCH_LINES || !lines.buffer().contains(&b'\n') {
      fan_out(&mut sessions, &batch, &mut push);
      batch.clear();
    }
  }
  if !batch.is_empty() {
    fan_out(&mut sessions, &batch, &mut push);
  }
  for session in sessions {
    let plot = session.name.clone();
    match session.finish() {
      Ok(failed) => push.failed |= failed,
      Err(msg) => push.lose(&plot, msg),
    }
  }
  Ok(push)
}

/// Sends `batch` to every session. A session whose send fails is dropped.
fn fan_out(sessions: &mut Vec<Session>, batch: &[String], push: &mut Push) {
  sessions.retain_mut(|session| {
    match session.send(&Msg::Rows {
      lines: batch.to_vec(),
    }) {
      Ok(()) => true,
      Err(msg) => {
        push.lose(&session.name, msg);
        false
      }
    }
  });
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
pub fn run(inv: &Invocation) -> ExitCode {
  let result = match inv {
    Invocation::Single(cli) if cli.by.is_some() => route_rows(cli),
    Invocation::Single(cli) => execute(cli),
    Invocation::Multi { shared, plots } => execute_multi(shared, plots),
  };
  match result {
    Ok(false) => ExitCode::SUCCESS,
    Ok(true) => ExitCode::FAILURE,
    Err(msg) => {
      eprintln!("complot: {msg}");
      ExitCode::FAILURE
    }
  }
}

/// Does the work of [`run`] for one plot. `Ok(true)` if the server reported an error.
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
  check_options(cli)?;
  let aes = parse_aes(cli)?;

  let first = match connect(&name) {
    Ok(conn) => conn,
    Err(ConnectError::Failed(msg)) => return Err(msg),
    Err(ConnectError::NotRunning) if cli.close => return Ok(false),
    Err(ConnectError::NotRunning) if has_data && !cli.create => {
      return Err(missing_error(&[name.as_str()]));
    }
    Err(ConnectError::NotRunning) => start(&name)?,
  };

  let mut failed = send_all(&name, first, &setup_messages(cli, aes.as_ref(), true))?;
  for path in &cli.files {
    let push = push_file(std::slice::from_ref(&name), path)?;
    failed |= push.failed;
    if !push.closed.is_empty() {
      // The window is gone: nothing more to send, not even `--close`.
      return Ok(true);
    }
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

/// Checks the options that place a point.
fn check_options(cli: &Cli) -> Result<(), String> {
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
  Ok(())
}

/// The `--aes` of `cli`, parsed.
fn parse_aes(cli: &Cli) -> Result<Option<Aes>, String> {
  cli
    .aes
    .as_deref()
    .map(Aes::parse)
    .transpose()
    .map_err(|e| format!("invalid --aes: {e}"))
}

/// The messages that set a plot up: aesthetic, title, clear and type, then
/// the point of `cli` if `point` is set.
fn setup_messages(cli: &Cli, aes: Option<&Aes>, point: bool) -> Vec<Msg> {
  let mut msgs = Vec::new();
  if let Some(aes) = aes {
    msgs.push(Msg::SetAes { aes: aes.clone() });
  }
  if let Some(text) = &cli.title {
    msgs.push(Msg::Title { text: text.clone() });
  }
  if cli.clear {
    msgs.push(Msg::Clear);
  }
  // Before the point, so that a new 3D plot accepts it.
  if let Some(plot_type) = cli.plot_type {
    msgs.push(Msg::SetType {
      plot_type,
      group: cli.g.clone(),
    });
  }
  if let Some(y) = cli.y
    && point
  {
    msgs.push(Msg::Point {
      x: cli.x,
      y,
      z: cli.z,
      g: cli.g.clone(),
    });
  }
  msgs
}

/// Sends `msgs` over `conn` as one chunk. Returns true if the server reported an error.
fn send_all(name: &str, conn: Connection, msgs: &[Msg]) -> Result<bool, String> {
  let mut session = Session::open(name, conn);
  for msg in msgs {
    session.send(msg)?;
  }
  session.finish()
}

/// Starts the server of `name`, and connects to it.
fn start(name: &str) -> Result<Connection, String> {
  spawn(name)?;
  connect(name).map_err(|e| connect_error(name, e))
}

/// The error for plots that have data, are not open, and may not be created.
fn missing_error(names: &[&str]) -> String {
  match names {
    [name] => {
      format!("plot '{name}' is not open (create it with `complot {name}` or pass --create)")
    }
    _ => {
      let list: Vec<String> = names.iter().map(|name| format!("'{name}'")).collect();
      format!(
        "plots {} are not open (create them with `complot NAME` or pass --create)",
        list.join(", ")
      )
    }
  }
}

/// A plot of a `NAME:` invocation, with its effective options.
struct Plan {
  name: String,
  /// Its own options where given, else the shared ones.
  cli: Cli,
  aes: Option<Aes>,
  /// Has files (its own or shared) or a point, so it is not a handle only.
  has_data: bool,
  /// Nothing more is sent to it: it is skipped (not open, with `--close`),
  /// or its window was closed.
  gone: bool,
}

/// How a plot of a `NAME:` invocation gets its connection.
enum Link {
  /// Open: the connection to use.
  Open(Connection),
  /// Not open, and started now.
  Spawn,
  /// Not open, with `--close`: left alone.
  Skip,
}

/// Runs a `NAME:` invocation. The shared options apply to every plot, and the
/// shared files are read once and sent to each plot. `Ok(true)` if a server
/// reported an error, or a write failed.
fn execute_multi(shared: &Cli, plots: &[(String, Cli)]) -> Result<bool, String> {
  // Everything is checked before any plot is touched.
  let mut plans = Vec::with_capacity(plots.len());
  for (name, own) in plots {
    if !registry::valid_name(name) {
      return Err(format!("invalid plot name '{name}'"));
    }
    let cli = args::merged(shared, own);
    check_options(&cli)?;
    let aes = parse_aes(&cli)?;
    let has_data = !shared.files.is_empty() || !cli.files.is_empty() || cli.y.is_some();
    plans.push(Plan {
      name: name.clone(),
      cli,
      aes,
      has_data,
      gone: false,
    });
  }

  // Connect to every open plot first. A plot with data that is not open is an
  // error, reported before anything is started or sent.
  let mut links = Vec::with_capacity(plans.len());
  let mut missing = Vec::new();
  for plan in &plans {
    links.push(match connect(&plan.name) {
      Ok(conn) => Link::Open(conn),
      Err(ConnectError::Failed(msg)) => return Err(msg),
      Err(ConnectError::NotRunning) if plan.cli.close => Link::Skip,
      Err(ConnectError::NotRunning) if plan.cli.create || !plan.has_data => Link::Spawn,
      Err(ConnectError::NotRunning) => {
        missing.push(plan.name.as_str());
        Link::Skip
      }
    });
  }
  if !missing.is_empty() {
    return Err(missing_error(&missing));
  }

  let mut conns = Vec::with_capacity(plans.len());
  for (plan, link) in plans.iter_mut().zip(links) {
    conns.push(match link {
      Link::Open(conn) => Some(conn),
      Link::Spawn => Some(start(&plan.name)?),
      Link::Skip => {
        plan.gone = true;
        None
      }
    });
  }

  // Each plot's first connection: its aesthetic, title, clear, type and point.
  let mut failed = false;
  for (plan, conn) in plans.iter_mut().zip(conns) {
    let Some(conn) = conn else { continue };
    let msgs = setup_messages(&plan.cli, plan.aes.as_ref(), true);
    match send_all(&plan.name, conn, &msgs) {
      Ok(f) => failed |= f,
      Err(msg) => {
        eprintln!("complot: {msg}");
        failed = true;
        plan.gone = true;
      }
    }
  }

  // Each shared file is read once, and goes to every plot that is still there.
  for path in &shared.files {
    let targets: Vec<String> = plans
      .iter()
      .filter(|plan| !plan.gone)
      .map(|plan| plan.name.clone())
      .collect();
    if targets.is_empty() {
      break;
    }
    let push = push_file(&targets, path)?;
    failed |= push.failed;
    for plan in plans
      .iter_mut()
      .filter(|plan| push.closed.contains(&plan.name))
    {
      plan.gone = true;
    }
  }

  // Each plot's own files go to that plot alone.
  for plan in plans.iter_mut().filter(|plan| !plan.gone) {
    for path in &plan.cli.files {
      let push = push_file(std::slice::from_ref(&plan.name), path)?;
      failed |= push.failed;
      if !push.closed.is_empty() {
        plan.gone = true;
        break;
      }
    }
  }

  // `--close` comes last, per plot.
  for plan in plans.iter().filter(|plan| plan.cli.close && !plan.gone) {
    let result = connect(&plan.name)
      .map_err(|e| connect_error(&plan.name, e))
      .and_then(|conn| close(&plan.name, conn));
    match result {
      Ok(f) => failed |= f,
      Err(msg) => {
        eprintln!("complot: {msg}");
        failed = true;
      }
    }
  }

  // The handles of the plots that had no data, for `read a b < <(complot a: b:)`.
  for plan in plans
    .iter()
    .filter(|plan| !plan.has_data && !plan.cli.close)
  {
    // Ignore write errors so that `complot a: b: | head -1` works.
    let _ = writeln!(io::stdout(), "{}", plan.name);
  }
  Ok(failed)
}

/// A plot that is being fed the rows of a `--by` source: its connection for
/// that source, and the rows not sent yet.
struct Feed {
  session: Session,
  batch: Vec<String>,
}

/// What a `--by` run has done so far, across its sources.
struct Routing {
  /// Sent first on each new connection: the shared configuration.
  setup: Vec<Msg>,
  /// Opens a plot that is not running.
  create: bool,
  /// Plots that are not open, or whose window was closed. Never sent to again.
  dead: Vec<String>,
  /// Plots that were routed rows, in order of first use. `--close` closes them.
  used: Vec<String>,
  /// The rejection messages printed so far.
  printed: HashSet<String>,
  /// Rows rejected, in all.
  rejected: u64,
  /// The server reported an error, or a write failed.
  failed: bool,
}

impl Routing {
  fn is_dead(&self, plot: &str) -> bool {
    self.dead.iter().any(|name| name == plot)
  }

  /// Prints a rejection the first time its message appears.
  fn reject(&mut self, msg: String) {
    self.rejected += 1;
    if !self.printed.contains(&msg) {
      eprintln!("complot: warn: {msg}");
      self.printed.insert(msg);
    }
  }

  /// Prints why `plot` gets no more rows, and remembers it for the run.
  fn drop_plot(&mut self, plot: &str, msg: String) {
    eprintln!("complot: {msg}");
    self.failed = true;
    self.dead.push(plot.to_string());
  }

  fn use_plot(&mut self, plot: &str) {
    if !self.used.iter().any(|name| name == plot) {
      self.used.push(plot.to_string());
    }
  }
}

/// `--by COL`: sends each row of the CSV sources to the plot named by its
/// value in column COL, without that column. Each source is one chunk, so
/// every plot it reaches gets a connection of its own for it: the shared
/// configuration, then the header, then the rows. `Ok(true)` if a server
/// reported an error, a write failed, or a plot was not open.
fn route_rows(cli: &Cli) -> Result<bool, String> {
  if cli.files.is_empty() {
    return Err("--by needs CSV input (files or -)".into());
  }
  check_options(cli)?;
  let aes = parse_aes(cli)?;
  let col = cli.by.as_deref().unwrap_or_default();
  let mut run = Routing {
    setup: setup_messages(cli, aes.as_ref(), false),
    create: cli.create,
    dead: Vec::new(),
    used: Vec::new(),
    printed: HashSet::new(),
    rejected: 0,
    failed: false,
  };
  for path in &cli.files {
    route_source(&mut run, col, path)?;
  }
  if run.rejected > run.printed.len() as u64 {
    eprintln!("complot: warn: {} rows rejected in all", run.rejected);
  }
  if cli.close {
    let names: Vec<String> = run
      .used
      .iter()
      .filter(|name| !run.is_dead(name))
      .cloned()
      .collect();
    run.failed |= close_plots(&names)?;
  }
  Ok(run.failed)
}

/// Routes the rows of one CSV source (`-` for stdin) on column `col`.
fn route_source(run: &mut Routing, col: &str, path: &Path) -> Result<(), String> {
  let mut lines = open_source(path)?;
  let mut router = Router::new(col);
  let mut feeds: BTreeMap<String, Feed> = BTreeMap::new();
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
    strip_newline(&mut raw);
    match router.route(&String::from_utf8_lossy(&raw)) {
      Routed::Skip | Routed::Header(_) => {}
      Routed::Reject(msg) => run.reject(msg),
      Routed::Row { plot, line } => {
        if !run.is_dead(&plot) {
          if !feeds.contains_key(&plot)
            && let Some(feed) = open_feed(run, &plot, router.header())
          {
            feeds.insert(plot.clone(), feed);
          }
          if let Some(feed) = feeds.get_mut(&plot) {
            run.use_plot(&plot);
            // A row that is only the routing column leaves nothing to send.
            if !line.is_empty() {
              feed.batch.push(line);
            }
          }
        }
      }
    }
    // The same rule as push_file: send when the next read would block, or
    // when a batch is full.
    let full = feeds.values().any(|feed| feed.batch.len() >= BATCH_LINES);
    if full || !lines.buffer().contains(&b'\n') {
      flush_feeds(&mut feeds, run);
    }
  }
  flush_feeds(&mut feeds, run);
  for (plot, feed) in feeds {
    match feed.session.finish() {
      Ok(failed) => run.failed |= failed,
      Err(msg) => run.drop_plot(&plot, msg),
    }
  }
  Ok(())
}

/// Opens the connection to `plot` for the rows of the current source: the
/// shared configuration, then the header. `None` if the plot cannot take
/// rows, which is reported and remembered.
fn open_feed(run: &mut Routing, plot: &str, header: Option<&str>) -> Option<Feed> {
  let conn = match connect(plot) {
    Ok(conn) => conn,
    Err(ConnectError::NotRunning) if run.create => match start(plot) {
      Ok(conn) => conn,
      Err(msg) => {
        run.drop_plot(plot, msg);
        return None;
      }
    },
    Err(ConnectError::NotRunning) => {
      run.drop_plot(plot, format!("plot '{plot}' is not open (pass --create)"));
      return None;
    }
    Err(ConnectError::Failed(msg)) => {
      run.drop_plot(plot, msg);
      return None;
    }
  };
  let mut session = Session::open(plot, conn);
  let header = header.filter(|h| !h.is_empty()).map(|h| Msg::Rows {
    lines: vec![h.to_string()],
  });
  let sent = run
    .setup
    .iter()
    .chain(&header)
    .try_for_each(|msg| session.send(msg));
  match sent {
    Ok(()) => Some(Feed {
      session,
      batch: Vec::new(),
    }),
    Err(msg) => {
      run.drop_plot(plot, msg);
      None
    }
  }
}

/// Sends the rows waiting for each plot. A plot whose window is closed is dropped.
fn flush_feeds(feeds: &mut BTreeMap<String, Feed>, run: &mut Routing) {
  feeds.retain(|plot, feed| {
    if feed.batch.is_empty() {
      return true;
    }
    let lines = std::mem::take(&mut feed.batch);
    match feed.session.send(&Msg::Rows { lines }) {
      Ok(()) => true,
      Err(msg) => {
        run.drop_plot(plot, msg);
        false
      }
    }
  });
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

  /// Serves `chunks` connections on `listener`, each with its own handshake,
  /// and returns the messages of each one. With `drop_after`, a connection
  /// that has that many messages is dropped at once, with the client's rest
  /// unread, and the server stops there: as if its window was closed.
  fn fake_chunks(
    listener: TcpListener,
    name: &str,
    token: &str,
    chunks: usize,
    drop_after: Option<usize>,
  ) -> JoinHandle<Vec<Vec<Msg>>> {
    let (name, token) = (name.to_string(), token.to_string());
    thread::spawn(move || {
      let mut all = Vec::new();
      for _ in 0..chunks {
        let (stream, _) = listener.accept().unwrap();
        stream
          .set_read_timeout(Some(Duration::from_secs(5)))
          .unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        assert_eq!(
          protocol::recv(&mut reader).unwrap(),
          Some(Msg::Hello {
            token: token.clone()
          })
        );
        writer
          .write_all(format!("{GREETING} {name}\n").as_bytes())
          .unwrap();
        let mut msgs = Vec::new();
        while drop_after != Some(msgs.len()) {
          let Some(msg) = protocol::recv(&mut reader).unwrap() else {
            break;
          };
          msgs.push(msg);
        }
        let dropped = drop_after == Some(msgs.len());
        all.push(msgs);
        if dropped {
          // Returning closes the socket with the client's rest unread.
          return all;
        }
      }
      all
    })
  }

  /// The lines of every `Rows` message in `msgs`, in order.
  fn rows_of(msgs: &[Msg]) -> Vec<String> {
    msgs
      .iter()
      .filter_map(|msg| match msg {
        Msg::Rows { lines } => Some(lines.clone()),
        _ => None,
      })
      .flatten()
      .collect()
  }

  /// A `Rows` message of `lines`.
  fn rows(lines: &[&str]) -> Msg {
    Msg::Rows {
      lines: lines.iter().map(|line| line.to_string()).collect(),
    }
  }

  /// Parses a command line that names several plots.
  fn multi(line: &[&str]) -> (Cli, Vec<(String, Cli)>) {
    match args::parse(std::iter::once("complot").chain(line.iter().copied())).unwrap() {
      Invocation::Multi { shared, plots } => (shared, plots),
      other => panic!("expected several plots, got {other:?}"),
    }
  }

  /// Parses a command line that names one plot, or none.
  fn single(line: &[&str]) -> Cli {
    match args::parse(std::iter::once("complot").chain(line.iter().copied())).unwrap() {
      Invocation::Single(cli) => cli,
      other => panic!("expected one plot, got {other:?}"),
    }
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
    let pushed = push_file(&[name.to_string()], &path).map(|push| push.failed);
    fs::remove_file(&path).unwrap();
    assert_eq!(pushed, Ok(false));
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
    let pushed = push_file(&[name.to_string()], &path).map(|push| push.failed);
    fs::remove_file(&path).unwrap();
    assert_eq!(pushed, Ok(true));
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

  #[test]
  fn multi_points_go_to_each_plot_with_the_shared_x() {
    registry::init_test_dir();
    let mut servers = Vec::new();
    for name in ["mp_a", "mp_b"] {
      let listener = TcpListener::bind("127.0.0.1:0").unwrap();
      let token = register(name, &listener);
      servers.push(fake_chunks(listener, name, &token, 1, None));
    }
    let (shared, plots) = multi(&["-x", "3", "mp_a:", "-y", "1", "mp_b:", "-y", "2"]);

    assert_eq!(execute_multi(&shared, &plots), Ok(false));
    let point = |y: f64| Msg::Point {
      x: Some(3.0),
      y,
      z: None,
      g: None,
    };
    let got: Vec<Vec<Vec<Msg>>> = servers.into_iter().map(|s| s.join().unwrap()).collect();
    assert_eq!(got, vec![vec![vec![point(1.0)]], vec![vec![point(2.0)]]]);
    registry::remove_entry("mp_a");
    registry::remove_entry("mp_b");
  }

  #[test]
  fn a_plot_that_is_not_open_stops_the_whole_command() {
    registry::init_test_dir();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let token = register("aon_a", &listener);
    let server = fake_chunks(listener, "aon_a", &token, 1, None);
    let (shared, plots) = multi(&["aon_a:", "-y", "1", "aon_missing:", "-y", "2"]);

    let err = execute_multi(&shared, &plots).unwrap_err();
    assert!(err.starts_with("plot 'aon_missing' is not open"), "{err}");
    // The connection to aon_a was made, but nothing was sent on it.
    assert_eq!(server.join().unwrap(), vec![Vec::<Msg>::new()]);
    registry::remove_entry("aon_a");
  }

  #[test]
  fn shared_file_reaches_every_plot_and_own_file_only_its_plot() {
    registry::init_test_dir();
    let dir = registry::dir();
    let shared_path = dir.join("fo_shared.csv");
    let own_path = dir.join("fo_own.csv");
    fs::write(&shared_path, "t,v\n0,1\n1,2\n").unwrap();
    fs::write(&own_path, "t,w\n5,6\n").unwrap();
    let mut servers = Vec::new();
    // fo_a: the first connection, the shared file and its own file.
    // fo_b: the first connection and the shared file.
    for (name, chunks) in [("fo_a", 3), ("fo_b", 2)] {
      let listener = TcpListener::bind("127.0.0.1:0").unwrap();
      let token = register(name, &listener);
      servers.push(fake_chunks(listener, name, &token, chunks, None));
    }
    let (shared_arg, own_arg) = (shared_path.to_str().unwrap(), own_path.to_str().unwrap());
    let (shared, plots) = multi(&[shared_arg, "fo_a:", own_arg, "fo_b:"]);

    let result = execute_multi(&shared, &plots);
    fs::remove_file(&shared_path).unwrap();
    fs::remove_file(&own_path).unwrap();
    let b = servers.remove(1).join().unwrap();
    let a = servers.remove(0).join().unwrap();
    assert_eq!(result, Ok(false));

    assert_eq!(a.len(), 3);
    assert!(a[0].is_empty());
    assert_eq!(rows_of(&a[1]), ["t,v", "0,1", "1,2"]);
    assert_eq!(rows_of(&a[2]), ["t,w", "5,6"]);
    assert_eq!(b.len(), 2);
    assert!(b[0].is_empty());
    assert_eq!(rows_of(&b[1]), ["t,v", "0,1", "1,2"]);
    registry::remove_entry("fo_a");
    registry::remove_entry("fo_b");
  }

  #[test]
  fn the_others_get_every_row_when_one_plot_closes() {
    registry::init_test_dir();
    let path = registry::dir().join("fc_stream.csv");
    // About 9 MB, far more than the socket buffers hold, so that the writes to
    // the plot that has gone must fail while rows are still being sent.
    let text: String = (0..600_000).map(|i| format!("{i},{}\n", i * 3)).collect();
    fs::write(&path, &text).unwrap();
    let mut servers = Vec::new();
    // fc_a reads one batch and then closes its window.
    for (name, drop_after) in [("fc_a", Some(1)), ("fc_b", None)] {
      let listener = TcpListener::bind("127.0.0.1:0").unwrap();
      let token = register(name, &listener);
      servers.push(fake_chunks(listener, name, &token, 2, drop_after));
    }
    let (shared, plots) = multi(&[path.to_str().unwrap(), "fc_a:", "fc_b:"]);

    let result = execute_multi(&shared, &plots);
    fs::remove_file(&path).unwrap();
    let b = servers.remove(1).join().unwrap();
    let a = servers.remove(0).join().unwrap();
    assert_eq!(result, Ok(true));

    assert_eq!(a.len(), 2);
    assert!(a[0].is_empty());
    assert_eq!(a[1].len(), 1);
    assert_eq!(b.len(), 2);
    let sent = rows_of(&b[1]);
    assert!(
      sent.iter().map(String::as_str).eq(text.lines()),
      "fc_b got {} rows, not {}",
      sent.len(),
      text.lines().count()
    );
    registry::remove_entry("fc_a");
    registry::remove_entry("fc_b");
  }

  #[test]
  fn by_routes_each_row_to_its_plot_after_the_shared_setup_and_header() {
    registry::init_test_dir();
    let mut servers = Vec::new();
    for name in ["by_a", "by_b"] {
      let listener = TcpListener::bind("127.0.0.1:0").unwrap();
      let token = register(name, &listener);
      servers.push(fake_chunks(listener, name, &token, 1, None));
    }
    let path = registry::dir().join("by_routes.csv");
    fs::write(&path, "t,sensor,v\n0,by_a,1\n1,by_b,2\n2,by_a,3").unwrap();
    let cli = single(&["--by", "sensor", "--aes", "x=t y=v", path.to_str().unwrap()]);

    let result = route_rows(&cli);
    fs::remove_file(&path).unwrap();
    let b = servers.remove(1).join().unwrap();
    let a = servers.remove(0).join().unwrap();
    assert_eq!(result, Ok(false));

    // The file has no final newline, so the rows are sent as they are routed:
    // "0,1" goes out when the row for by_b is read, and "2,3" at the end.
    let aes = Msg::SetAes {
      aes: Aes::parse("x=t y=v").unwrap(),
    };
    assert_eq!(
      a,
      vec![vec![
        aes.clone(),
        rows(&["t,v"]),
        rows(&["0,1"]),
        rows(&["2,3"])
      ]]
    );
    assert_eq!(b, vec![vec![aes, rows(&["t,v"]), rows(&["1,2"])]]);
    registry::remove_entry("by_a");
    registry::remove_entry("by_b");
  }

  #[test]
  fn by_skips_a_plot_that_is_not_open_and_goes_on() {
    registry::init_test_dir();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let token = register("by_ok", &listener);
    let server = fake_chunks(listener, "by_ok", &token, 1, None);
    let path = registry::dir().join("by_missing.csv");
    fs::write(&path, "t,sensor,v\n0,by_ok,1\n1,by_nope,2\n2,by_ok,3").unwrap();
    let cli = single(&["--by", "sensor", path.to_str().unwrap()]);

    let result = route_rows(&cli);
    fs::remove_file(&path).unwrap();
    assert_eq!(result, Ok(true));
    // As above, the rows go out when the rejected line leaves no complete line
    // buffered, so "0,1" is a message of its own.
    assert_eq!(
      server.join().unwrap(),
      vec![vec![rows(&["t,v"]), rows(&["0,1"]), rows(&["2,3"])]]
    );
    registry::remove_entry("by_ok");
  }
}
