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
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, ExitCode, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
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
/// How often the watcher of an `--exec` command checks its plots.
const WATCH_INTERVAL: Duration = Duration::from_millis(200);

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
  /// Cleared by the reply thread once the server has closed the connection.
  alive: Arc<AtomicBool>,
  replies: JoinHandle<bool>,
}

impl Session {
  /// Starts printing the replies on `conn` right away.
  fn open(name: &str, conn: Connection) -> Session {
    let Connection { mut reader, writer } = conn;
    let alive = Arc::new(AtomicBool::new(true));
    let flag = Arc::clone(&alive);
    let replies = thread::spawn(move || {
      let mut failed = false;
      let mut raw = Vec::new();
      loop {
        raw.clear();
        match reader.read_until(b'\n', &mut raw) {
          Ok(0) | Err(_) => break,
          Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&raw);
        let line = text.trim_end_matches(['\r', '\n']);
        failed |= line.starts_with(ERROR);
        eprintln!("complot: {line}");
      }
      flag.store(false, Ordering::SeqCst);
      failed
    });
    Session {
      name: name.to_string(),
      writer,
      alive,
      replies,
    }
  }

  /// Sends one message and flushes it.
  fn send(&mut self, msg: &Msg) -> Result<(), String> {
    protocol::send(&mut self.writer, msg)
      .and_then(|()| self.writer.flush())
      .map_err(|e| write_error(&self.name, e))
  }

  /// False once the server has closed the connection.
  fn is_alive(&self) -> bool {
    self.alive.load(Ordering::SeqCst)
  }

  /// Ends the connection and waits for the server to close its side.
  /// Returns true if the server reported an error.
  fn finish(self) -> Result<bool, String> {
    let Session {
      name,
      mut writer,
      replies,
      ..
    } = self;
    writer.flush().map_err(|e| write_error(&name, e))?;
    // EOF tells the server this chunk is complete; it then closes the socket.
    let _ = writer.get_ref().shutdown(Shutdown::Write);
    replies
      .join()
      .map_err(|_| format!("reply reader of plot '{name}' panicked"))
  }
}

/// Ends `session` once its chunk is complete. A server that closed the
/// connection before then has closed the plot's window, which is an error.
fn end(session: Session) -> Result<bool, String> {
  let name = session.name.clone();
  let closed = !session.is_alive();
  let result = session.finish();
  if closed {
    Err(format!("plot '{name}' was closed"))
  } else {
    result
  }
}

/// Message for an I/O error while reading `source`.
fn read_error(source: &Source, e: io::Error) -> String {
  match source {
    Source::Stdin => format!("cannot read '-': {e}"),
    Source::File(path) => format!("cannot read '{}': {e}", path.display()),
    Source::Exec(cmd) => format!("cannot read the output of '{cmd}': {e}"),
  }
}

/// Where CSV lines come from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
  /// `-`: the standard input.
  Stdin,
  /// A file.
  File(PathBuf),
  /// `--exec CMD`: the standard output of CMD.
  Exec(String),
}

/// The sources of `cli`: its files in order, then its `--exec` command.
fn sources(cli: &Cli) -> Vec<Source> {
  let files = cli.files.iter().map(|path| {
    if path == Path::new("-") {
      Source::Stdin
    } else {
      Source::File(path.clone())
    }
  });
  let exec = cli.exec.iter().map(|cmd| Source::Exec(cmd.clone()));
  files.chain(exec).collect()
}

/// True if `cli` has CSV input: files, `-` or `--exec`.
fn has_input(cli: &Cli) -> bool {
  !cli.files.is_empty() || cli.exec.is_some()
}

/// Opens `source` with a read buffer. An `--exec` command is started too, and
/// returned as its producer.
fn open_source(source: &Source) -> Result<Input, String> {
  let mut producer = None;
  let reader: Box<dyn Read> = match source {
    Source::Stdin => Box::new(io::stdin()),
    Source::File(path) => Box::new(File::open(path).map_err(|e| read_error(source, e))?),
    Source::Exec(cmd) => {
      let (started, stdout) = Producer::start(cmd)?;
      producer = Some(started);
      Box::new(stdout)
    }
  };
  Ok(Input {
    lines: BufReader::with_capacity(READ_BUFFER, reader),
    producer,
  })
}

/// A source being read: its lines, and for `--exec` the command that makes them.
struct Input {
  lines: BufReader<Box<dyn Read>>,
  producer: Option<Producer>,
}

/// A running `--exec` command. complot reads its standard output, and a
/// watcher thread kills the command once every session it feeds is closed, so
/// that an idle command does not outlive its plots.
struct Producer {
  running: Arc<Running>,
  /// Dropping the sender ends the watcher.
  watcher: Option<(Sender<()>, JoinHandle<()>)>,
}

/// The command of a [`Producer`], as its watcher shares it.
struct Running {
  child: Mutex<Child>,
  /// Set once complot has killed the command.
  killed: AtomicBool,
  /// The `alive` flags of the sessions the command feeds.
  feeds: Mutex<Vec<Arc<AtomicBool>>>,
}

impl Producer {
  /// Runs `cmd` through the shell, with its standard output piped. Returns the
  /// producer and the pipe to read.
  fn start(cmd: &str) -> Result<(Producer, ChildStdout), String> {
    let mut child = shell_command(cmd)
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::inherit())
      .spawn()
      .map_err(|e| format!("cannot run '{cmd}': {e}"))?;
    let stdout = child
      .stdout
      .take()
      .ok_or_else(|| format!("cannot read the output of '{cmd}'"))?;
    let running = Arc::new(Running {
      child: Mutex::new(child),
      killed: AtomicBool::new(false),
      feeds: Mutex::new(Vec::new()),
    });
    let watcher = watch(Arc::clone(&running));
    let producer = Producer {
      running,
      watcher: Some(watcher),
    };
    Ok((producer, stdout))
  }

  /// Makes the watcher count `session` among the sessions it feeds.
  fn feed(&self, session: &Session) {
    lock(&self.running.feeds).push(Arc::clone(&session.alive));
  }

  /// Kills the command now, unless it has exited.
  fn kill(&self) {
    self.running.kill();
  }

  /// Waits for the command, once its output has ended. Returns the message for
  /// a failure of its own: a non-zero status, or a death by signal.
  fn finish(&mut self) -> Option<String> {
    self.stop_watcher();
    let status = lock(&self.running.child).wait();
    match status {
      Ok(status) if status.success() || self.running.was_killed() => None,
      Ok(status) => Some(exit_message(status)),
      Err(e) => Some(format!("cannot wait for the producer: {e}")),
    }
  }

  /// Stops the watcher, and waits for its thread to end.
  fn stop_watcher(&mut self) {
    if let Some((stop, handle)) = self.watcher.take() {
      drop(stop);
      let _ = handle.join();
    }
  }
}

impl Drop for Producer {
  /// Stops the command if it is still there, as after an error.
  fn drop(&mut self) {
    self.stop_watcher();
    self.running.kill();
    let _ = lock(&self.running.child).wait();
  }
}

impl Running {
  /// Kills the command, unless it has exited.
  fn kill(&self) {
    let mut child = lock(&self.child);
    if matches!(child.try_wait(), Ok(None)) {
      self.killed.store(true, Ordering::SeqCst);
      kill_process(&mut child);
    }
  }

  /// True if complot killed the command.
  fn was_killed(&self) -> bool {
    self.killed.load(Ordering::SeqCst)
  }

  /// True if at least one session is fed and all of them are closed.
  fn feeds_closed(&self) -> bool {
    let feeds = lock(&self.feeds);
    !feeds.is_empty() && feeds.iter().all(|alive| !alive.load(Ordering::SeqCst))
  }
}

/// Starts the watcher of `running`. Every `WATCH_INTERVAL` it kills the command
/// once its plots are all closed, and it ends when the returned sender is dropped.
fn watch(running: Arc<Running>) -> (Sender<()>, JoinHandle<()>) {
  let (stop, stopped) = mpsc::channel::<()>();
  let handle = thread::spawn(move || {
    while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(WATCH_INTERVAL) {
      if running.feeds_closed() {
        running.kill();
        break;
      }
    }
  });
  (stop, handle)
}

/// The command that runs `cmd` through the shell.
#[cfg(unix)]
fn shell_command(cmd: &str) -> Command {
  let mut command = Command::new("sh");
  command.arg("-c").arg(cmd);
  command
}

/// The command that runs `cmd` through cmd. The text is passed verbatim, and
/// `/S` strips just the outer quotes.
#[cfg(windows)]
fn shell_command(cmd: &str) -> Command {
  use std::os::windows::process::CommandExt;
  let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
  let mut command = Command::new(shell);
  command
    .args(["/D", "/S", "/C"])
    .raw_arg(format!("\"{cmd}\""));
  command
}

/// On Windows a child process inherits every inheritable handle of its parent,
/// whatever its own stdio is. A plot window started from
/// `$h = complot …` in PowerShell would so keep PowerShell's capture pipe open,
/// and the assignment would hang until the window is closed. Our own standard
/// handles are therefore made non-inheritable: children get only the handles
/// passed to them explicitly (Rust passes an inherited stderr as a duplicate).
#[cfg(windows)]
fn keep_std_handles_private() {
  type Handle = *mut std::ffi::c_void;
  const STD_HANDLES: [u32; 3] = [-10i32 as u32, -11i32 as u32, -12i32 as u32];
  const HANDLE_FLAG_INHERIT: u32 = 1;
  #[link(name = "kernel32")]
  unsafe extern "system" {
    fn GetStdHandle(which: u32) -> Handle;
    fn SetHandleInformation(handle: Handle, mask: u32, flags: u32) -> i32;
  }
  for which in STD_HANDLES {
    // SAFETY: plain Win32 calls on this process's own standard handles;
    // a missing or invalid handle is skipped, and failures are harmless.
    unsafe {
      let handle = GetStdHandle(which);
      if !handle.is_null() && handle as isize != -1 {
        SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
      }
    }
  }
}

/// Nothing to do on Unix, where `Command` closes everything but the child's stdio.
#[cfg(unix)]
fn keep_std_handles_private() {}

/// Kills `child` and every process below it. The shell may run the command as
/// a child of its own (dash, the Linux `sh`, always does; any shell does for
/// `a; b` or `a | b`), and those processes would keep the output pipe open
/// after the shell dies. On Windows the processes that cmd started go too.
#[cfg(unix)]
fn kill_process(child: &mut Child) {
  unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
  }
  const SIGKILL: i32 = 9;
  // Collected before anything dies: an orphan is reparented and lost to us.
  for pid in descendants(child.id()) {
    // SAFETY: kill(2) on processes this complot started, by pid.
    unsafe {
      kill(pid as i32, SIGKILL);
    }
  }
  let _ = child.kill();
}

/// The processes below `pid`, at any depth.
#[cfg(unix)]
fn descendants(pid: u32) -> Vec<u32> {
  let mut found = Vec::new();
  let mut queue = vec![pid];
  while let Some(parent) = queue.pop() {
    for c in children(parent) {
      found.push(c);
      queue.push(c);
    }
  }
  found
}

/// The direct children of `pid`, from /proc: no external tool needed.
#[cfg(target_os = "linux")]
fn children(pid: u32) -> Vec<u32> {
  let Ok(dir) = fs::read_dir("/proc") else {
    return Vec::new();
  };
  dir
    .filter_map(Result::ok)
    .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
    .filter(|&p| {
      // /proc/P/stat is "P (comm) state ppid …"; comm may hold spaces and
      // parentheses, so the fields are read after the last ')'.
      fs::read_to_string(format!("/proc/{p}/stat")).is_ok_and(|stat| {
        stat
          .rsplit_once(')')
          .and_then(|(_, rest)| rest.split_whitespace().nth(1)?.parse::<u32>().ok())
          == Some(pid)
      })
    })
    .collect()
}

/// The direct children of `pid`, from `pgrep -P` (macOS and the BSDs).
#[cfg(all(unix, not(target_os = "linux")))]
fn children(pid: u32) -> Vec<u32> {
  Command::new("pgrep")
    .args(["-P", &pid.to_string()])
    .stderr(Stdio::null())
    .output()
    .map(|out| {
      String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect()
    })
    .unwrap_or_default()
}

#[cfg(windows)]
fn kill_process(child: &mut Child) {
  let pid = child.id().to_string();
  let _ = Command::new("taskkill")
    .args(["/T", "/F", "/PID", pid.as_str()])
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status();
  let _ = child.kill();
}

/// The message for a producer that failed on its own.
fn exit_message(status: ExitStatus) -> String {
  match status.code() {
    Some(code) => format!("producer exited with status {code}"),
    None => format!("producer exited with status {status}"),
  }
}

/// Locks `m`. A poisoned lock still holds valid data, so it is taken anyway.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
  m.lock().unwrap_or_else(PoisonError::into_inner)
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

/// Starts the chunk of `conn` for plot `name`, with `Frames` first if `frames`.
fn open_chunk(name: &str, conn: Connection, frames: bool) -> Result<Session, String> {
  let mut session = Session::open(name, conn);
  if frames {
    session.send(&Msg::Frames)?;
  }
  Ok(session)
}

/// Pushes the CSV of `source` to every plot in `plots`, each over a connection
/// of its own, so that each source is one CSV chunk per plot. The plots are
/// `(name, frames)` pairs: with `frames`, the chunk starts with `Frames`. The
/// source is read once, and each batch goes to every plot still open. A plot
/// that is lost is reported and dropped; the others go on, and the source is
/// not read any further once none is left. An `--exec` command is stopped then.
fn push_source(plots: &[(String, bool)], source: &Source) -> Result<Push, String> {
  let Input {
    mut lines,
    mut producer,
  } = open_source(source)?;
  let mut push = Push::default();
  let mut sessions = Vec::new();
  for (plot, frames) in plots {
    match connect(plot) {
      Ok(conn) => match open_chunk(plot, conn, *frames) {
        Ok(session) => {
          if let Some(producer) = &producer {
            producer.feed(&session);
          }
          sessions.push(session);
        }
        Err(msg) => push.lose(plot, msg),
      },
      Err(e) => push.lose(plot, connect_error(plot, e)),
    }
  }

  let mut batch = Vec::new();
  let mut raw = Vec::new();
  let mut eof = false;
  while !sessions.is_empty() {
    raw.clear();
    if lines
      .read_until(b'\n', &mut raw)
      .map_err(|e| read_error(source, e))?
      == 0
    {
      eof = true;
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
  if let Some(producer) = producer.as_mut() {
    // Reading is over, so the watcher stops here. Ending the sessions below
    // makes their servers close, which must not look like every plot is gone.
    producer.stop_watcher();
    // No plot is left to read for, so the command is stopped before its output ends.
    if !eof {
      producer.kill();
    }
  }
  for session in sessions {
    let plot = session.name.clone();
    match end(session) {
      Ok(failed) => push.failed |= failed,
      Err(msg) => push.lose(&plot, msg),
    }
  }
  if let Some(producer) = producer.as_mut()
    && let Some(msg) = producer.finish()
  {
    eprintln!("complot: {msg}");
    push.failed = true;
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
  keep_std_handles_private();
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

  let has_data = has_input(cli) || cli.y.is_some();
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
  for source in sources(cli) {
    let push = push_source(&[(name.clone(), cli.frames)], &source)?;
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
    let has_data = has_input(shared) || has_input(&cli) || cli.y.is_some();
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

  // Each shared source is read once, and goes to every plot that is still there.
  for source in sources(shared) {
    let targets: Vec<(String, bool)> = plans
      .iter()
      .filter(|plan| !plan.gone)
      .map(|plan| (plan.name.clone(), plan.cli.frames))
      .collect();
    if targets.is_empty() {
      break;
    }
    let push = push_source(&targets, &source)?;
    failed |= push.failed;
    for plan in plans
      .iter_mut()
      .filter(|plan| push.closed.contains(&plan.name))
    {
      plan.gone = true;
    }
  }

  // Each plot's own sources go to that plot alone.
  for plan in plans.iter_mut().filter(|plan| !plan.gone) {
    for source in sources(&plan.cli) {
      let push = push_source(&[(plan.name.clone(), plan.cli.frames)], &source)?;
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
  /// `--frames`: each new connection starts with `Frames`, and blank lines end frames.
  frames: bool,
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
  if !has_input(cli) {
    return Err("--by needs CSV input (files, - or --exec)".into());
  }
  check_options(cli)?;
  let aes = parse_aes(cli)?;
  let col = cli.by.as_deref().unwrap_or_default();
  let mut run = Routing {
    setup: setup_messages(cli, aes.as_ref(), false),
    create: cli.create,
    frames: cli.frames,
    dead: Vec::new(),
    used: Vec::new(),
    printed: HashSet::new(),
    rejected: 0,
    failed: false,
  };
  for source in sources(cli) {
    route_source(&mut run, col, &source)?;
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

/// Routes the rows of one CSV source on column `col`.
fn route_source(run: &mut Routing, col: &str, source: &Source) -> Result<(), String> {
  let Input {
    mut lines,
    mut producer,
  } = open_source(source)?;
  let mut router = Router::new(col);
  let mut feeds: BTreeMap<String, Feed> = BTreeMap::new();
  let mut raw = Vec::new();
  loop {
    raw.clear();
    if lines
      .read_until(b'\n', &mut raw)
      .map_err(|e| read_error(source, e))?
      == 0
    {
      break;
    }
    strip_newline(&mut raw);
    match router.route(&String::from_utf8_lossy(&raw)) {
      Routed::Skip => {}
      Routed::Blank => {
        // Ends the frame of every plot open to this source.
        if run.frames {
          for feed in feeds.values_mut() {
            feed.batch.push(String::new());
          }
        }
      }
      Routed::Clear(names) => {
        // Restarts the open plots. Plots opened later get the new header.
        let directive = match names {
          Some(names) if !names.is_empty() => format!("#clear {names}"),
          _ => "#clear".to_string(),
        };
        for feed in feeds.values_mut() {
          feed.batch.push(directive.clone());
        }
      }
      Routed::Header(header) => {
        // A header after a `#clear` without names: the open plots need it too.
        if !header.is_empty() {
          for feed in feeds.values_mut() {
            feed.batch.push(header.clone());
          }
        }
      }
      Routed::Reject(msg) => run.reject(msg),
      Routed::Row { plot, line } => {
        if !run.is_dead(&plot) {
          if !feeds.contains_key(&plot)
            && let Some(feed) = open_feed(run, &plot, router.header())
          {
            if let Some(producer) = &producer {
              producer.feed(&feed.session);
            }
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
    // The same rule as push_source: send when the next read would block, or
    // when a batch is full.
    let full = feeds.values().any(|feed| feed.batch.len() >= BATCH_LINES);
    if full || !lines.buffer().contains(&b'\n') {
      flush_feeds(&mut feeds, run);
    }
  }
  flush_feeds(&mut feeds, run);
  if let Some(producer) = producer.as_mut() {
    // As in push_source: ending the feeds below must not stop the command.
    producer.stop_watcher();
  }
  for (plot, feed) in feeds {
    match end(feed.session) {
      Ok(failed) => run.failed |= failed,
      Err(msg) => run.drop_plot(&plot, msg),
    }
  }
  if let Some(producer) = producer.as_mut()
    && let Some(msg) = producer.finish()
  {
    eprintln!("complot: {msg}");
    run.failed = true;
  }
  Ok(())
}

/// Opens the connection to `plot` for the rows of the current source: `Frames`
/// with `--frames`, the shared configuration, then the header. `None` if the
/// plot cannot take rows, which is reported and remembered.
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
  let frames = run.frames.then_some(Msg::Frames);
  let header = header.filter(|h| !h.is_empty()).map(|h| Msg::Rows {
    lines: vec![h.to_string()],
  });
  let sent = frames
    .iter()
    .chain(&run.setup)
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
    let source = Source::File(path.clone());
    let pushed = push_source(&[(name.to_string(), false)], &source).map(|push| push.failed);
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
    let source = Source::File(path.clone());
    let pushed = push_source(&[(name.to_string(), false)], &source).map(|push| push.failed);
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

  #[test]
  fn frames_start_a_single_plots_file_and_blank_lines_go_through() {
    registry::init_test_dir();
    let name = "frs_one";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let token = register(name, &listener);
    let server = fake_chunks(listener, name, &token, 2, None);
    let path = registry::dir().join("frs_one.csv");
    fs::write(&path, "t,v\n0,1\n\n1,2\n").unwrap();
    let cli = single(&["--frames", name, path.to_str().unwrap()]);

    let result = execute(&cli);
    fs::remove_file(&path).unwrap();
    let chunks = server.join().unwrap();
    assert_eq!(result, Ok(false));
    // The first chunk is the configuration, which carries no rows.
    assert!(chunks[0].is_empty());
    assert_eq!(chunks[1][0], Msg::Frames);
    assert_eq!(rows_of(&chunks[1]), ["t,v", "0,1", "", "1,2"]);
    registry::remove_entry(name);
  }

  #[test]
  fn frames_go_only_to_the_plots_that_asked_for_them() {
    registry::init_test_dir();
    let mut servers = Vec::new();
    for name in ["frm_a", "frm_b"] {
      let listener = TcpListener::bind("127.0.0.1:0").unwrap();
      let token = register(name, &listener);
      servers.push(fake_chunks(listener, name, &token, 2, None));
    }
    let path = registry::dir().join("frm_shared.csv");
    fs::write(&path, "t,v\n0,1\n\n1,2\n").unwrap();
    // `--frames` after `frm_a:` is its own option; the shared file goes to both.
    let (shared, plots) = multi(&[path.to_str().unwrap(), "frm_a:", "--frames", "frm_b:"]);

    let result = execute_multi(&shared, &plots);
    fs::remove_file(&path).unwrap();
    let b = servers.remove(1).join().unwrap();
    let a = servers.remove(0).join().unwrap();
    assert_eq!(result, Ok(false));

    assert_eq!(a[1][0], Msg::Frames);
    assert_eq!(rows_of(&a[1]), ["t,v", "0,1", "", "1,2"]);
    assert!(!b[1].contains(&Msg::Frames));
    assert_eq!(rows_of(&b[1]), ["t,v", "0,1", "", "1,2"]);
    registry::remove_entry("frm_a");
    registry::remove_entry("frm_b");
  }

  #[test]
  fn by_with_frames_starts_each_plot_with_frames_and_passes_blank_lines() {
    registry::init_test_dir();
    let mut servers = Vec::new();
    for name in ["byf_a", "byf_b"] {
      let listener = TcpListener::bind("127.0.0.1:0").unwrap();
      let token = register(name, &listener);
      servers.push(fake_chunks(listener, name, &token, 1, None));
    }
    let path = registry::dir().join("byf_frames.csv");
    fs::write(&path, "t,sensor,v\n0,byf_a,1\n0,byf_b,2\n\n1,byf_a,3\n").unwrap();
    let cli = single(&["--by", "sensor", "--frames", path.to_str().unwrap()]);

    let result = route_rows(&cli);
    fs::remove_file(&path).unwrap();
    let b = servers.remove(1).join().unwrap();
    let a = servers.remove(0).join().unwrap();
    assert_eq!(result, Ok(false));

    // byf_b was open when the blank line came, so it ends its frame too.
    assert_eq!(a.len(), 1);
    assert_eq!(a[0][0], Msg::Frames);
    assert_eq!(rows_of(&a[0]), ["t,v", "0,1", "", "1,3"]);
    assert_eq!(b.len(), 1);
    assert_eq!(b[0][0], Msg::Frames);
    assert_eq!(rows_of(&b[0]), ["t,v", "0,2", ""]);
    registry::remove_entry("byf_a");
    registry::remove_entry("byf_b");
  }

  #[test]
  fn by_without_frames_ignores_blank_lines() {
    registry::init_test_dir();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let token = register("byn_a", &listener);
    let server = fake_chunks(listener, "byn_a", &token, 1, None);
    let path = registry::dir().join("byn_blank.csv");
    fs::write(&path, "t,sensor,v\n0,byn_a,1\n\n1,byn_a,2\n").unwrap();
    let cli = single(&["--by", "sensor", path.to_str().unwrap()]);

    let result = route_rows(&cli);
    fs::remove_file(&path).unwrap();
    let a = server.join().unwrap();
    assert_eq!(result, Ok(false));

    assert_eq!(a.len(), 1);
    assert!(!a[0].contains(&Msg::Frames));
    assert_eq!(rows_of(&a[0]), ["t,v", "0,1", "1,2"]);
    registry::remove_entry("byn_a");
  }

  #[test]
  fn by_clear_with_names_reaches_every_open_plot_in_order() {
    registry::init_test_dir();
    let mut servers = Vec::new();
    for name in ["byc_a", "byc_b", "byc_c"] {
      let listener = TcpListener::bind("127.0.0.1:0").unwrap();
      let token = register(name, &listener);
      servers.push(fake_chunks(listener, name, &token, 1, None));
    }
    let path = registry::dir().join("byc_clear.csv");
    // byc_c is opened after the clear, so it gets the new header instead.
    fs::write(
      &path,
      "t,sensor,v\n0,byc_a,1\n0,byc_b,2\n#clear t,sensor,v\n1,byc_a,3\n2,byc_c,4\n",
    )
    .unwrap();
    let cli = single(&["--by", "sensor", path.to_str().unwrap()]);

    let result = route_rows(&cli);
    fs::remove_file(&path).unwrap();
    let c = servers.remove(2).join().unwrap();
    let b = servers.remove(1).join().unwrap();
    let a = servers.remove(0).join().unwrap();
    assert_eq!(result, Ok(false));

    assert_eq!(rows_of(&a[0]), ["t,v", "0,1", "#clear t,v", "1,3"]);
    assert_eq!(rows_of(&b[0]), ["t,v", "0,2", "#clear t,v"]);
    assert_eq!(rows_of(&c[0]), ["t,v", "2,4"]);
    registry::remove_entry("byc_a");
    registry::remove_entry("byc_b");
    registry::remove_entry("byc_c");
  }

  #[test]
  fn by_clear_without_names_sends_the_next_header_to_open_plots() {
    registry::init_test_dir();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let token = register("byh_a", &listener);
    let server = fake_chunks(listener, "byh_a", &token, 1, None);
    let path = registry::dir().join("byh_clear.csv");
    fs::write(
      &path,
      "t,sensor,v\n0,byh_a,1\n#clear\nu,sensor,w\n1,byh_a,2\n",
    )
    .unwrap();
    let cli = single(&["--by", "sensor", path.to_str().unwrap()]);

    let result = route_rows(&cli);
    fs::remove_file(&path).unwrap();
    let a = server.join().unwrap();
    assert_eq!(result, Ok(false));

    assert_eq!(rows_of(&a[0]), ["t,v", "0,1", "#clear", "u,w", "1,2"]);
    registry::remove_entry("byh_a");
  }

  #[test]
  fn sources_are_the_files_in_order_then_the_exec() {
    let cli = single(&["--by", "s", "-", "a.csv", "--exec", "cat"]);
    assert_eq!(
      sources(&cli),
      [
        Source::Stdin,
        Source::File("a.csv".into()),
        Source::Exec("cat".into()),
      ]
    );
  }

  /// True if a process with `pid` exists.
  #[cfg(unix)]
  fn process_alive(pid: &str) -> bool {
    let script = format!("kill -0 {pid} 2>/dev/null");
    Command::new("sh")
      .arg("-c")
      .arg(script)
      .status()
      .is_ok_and(|status| status.success())
  }

  #[cfg(unix)]
  #[test]
  fn exec_rows_reach_the_plot() {
    registry::init_test_dir();
    let name = "ex_rows";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let token = register(name, &listener);
    let server = fake_chunks(listener, name, &token, 2, None);

    let cli = single(&[name, "--exec", "printf 't,v\\n0,1\\n1,2\\n'"]);
    let result = execute(&cli);
    let chunks = server.join().unwrap();
    registry::remove_entry(name);

    assert_eq!(result, Ok(false));
    assert!(chunks[0].is_empty());
    assert_eq!(rows_of(&chunks[1]), ["t,v", "0,1", "1,2"]);
  }

  #[cfg(unix)]
  #[test]
  fn shared_exec_fans_out_to_every_plot_and_runs_once() {
    registry::init_test_dir();
    let runs = registry::dir().join("fx_runs.txt");
    let script = format!(
      "printf 't,v\\n0,1\\n1,2\\n'; echo run >> '{}'",
      runs.display()
    );
    let mut servers = Vec::new();
    for name in ["fx_a", "fx_b"] {
      let listener = TcpListener::bind("127.0.0.1:0").unwrap();
      let token = register(name, &listener);
      servers.push(fake_chunks(listener, name, &token, 2, None));
    }
    let (shared, plots) = multi(&["--exec", script.as_str(), "fx_a:", "fx_b:"]);

    let result = execute_multi(&shared, &plots);
    let b = servers.remove(1).join().unwrap();
    let a = servers.remove(0).join().unwrap();
    let runs_count = fs::read_to_string(&runs).unwrap().lines().count();
    fs::remove_file(&runs).unwrap();
    registry::remove_entry("fx_a");
    registry::remove_entry("fx_b");

    assert_eq!(result, Ok(false));
    assert_eq!(runs_count, 1);
    for chunks in [&a, &b] {
      assert!(chunks[0].is_empty());
      assert_eq!(rows_of(&chunks[1]), ["t,v", "0,1", "1,2"]);
    }
  }

  #[cfg(unix)]
  #[test]
  fn own_exec_goes_to_its_plot_alone() {
    registry::init_test_dir();
    let listener_a = TcpListener::bind("127.0.0.1:0").unwrap();
    let token_a = register("oe_a", &listener_a);
    let server_a = fake_chunks(listener_a, "oe_a", &token_a, 2, None);
    let listener_b = TcpListener::bind("127.0.0.1:0").unwrap();
    let token_b = register("oe_b", &listener_b);
    let server_b = fake_chunks(listener_b, "oe_b", &token_b, 1, None);
    let (shared, plots) = multi(&["oe_a:", "--exec", "printf 't,v\\n0,1\\n'", "oe_b:"]);

    let result = execute_multi(&shared, &plots);
    let b = server_b.join().unwrap();
    let a = server_a.join().unwrap();
    registry::remove_entry("oe_a");
    registry::remove_entry("oe_b");

    assert_eq!(result, Ok(false));
    assert_eq!(rows_of(&a[1]), ["t,v", "0,1"]);
    assert_eq!(b.len(), 1);
    assert!(b[0].is_empty());
  }

  #[cfg(unix)]
  #[test]
  fn by_routes_exec_rows_to_their_plots() {
    registry::init_test_dir();
    let mut servers = Vec::new();
    for name in ["rx_a", "rx_b"] {
      let listener = TcpListener::bind("127.0.0.1:0").unwrap();
      let token = register(name, &listener);
      servers.push(fake_chunks(listener, name, &token, 1, None));
    }
    let cli = single(&[
      "--by",
      "s",
      "--exec",
      "printf 't,s,v\\n0,rx_a,1\\n1,rx_b,2\\n2,rx_a,3\\n'",
    ]);

    let result = route_rows(&cli);
    let b = servers.remove(1).join().unwrap();
    let a = servers.remove(0).join().unwrap();
    registry::remove_entry("rx_a");
    registry::remove_entry("rx_b");

    assert_eq!(result, Ok(false));
    assert_eq!(rows_of(&a[0]), ["t,v", "0,1", "2,3"]);
    assert_eq!(rows_of(&b[0]), ["t,v", "1,2"]);
  }

  #[cfg(unix)]
  #[test]
  fn exec_that_fails_makes_the_run_fail() {
    registry::init_test_dir();
    let name = "ex_fail";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let token = register(name, &listener);
    let server = fake_chunks(listener, name, &token, 2, None);

    let result = execute(&single(&[name, "--exec", "exit 3"]));
    let chunks = server.join().unwrap();
    registry::remove_entry(name);

    assert_eq!(result, Ok(true));
    assert!(chunks[1].is_empty());
  }

  #[cfg(unix)]
  #[test]
  fn exec_failure_names_the_status_or_signal() {
    let (mut producer, mut out) = Producer::start("exit 3").unwrap();
    let mut text = Vec::new();
    out.read_to_end(&mut text).unwrap();
    assert!(text.is_empty());
    assert_eq!(
      producer.finish(),
      Some("producer exited with status 3".to_string())
    );

    let (mut producer, mut out) = Producer::start("kill -9 $$").unwrap();
    out.read_to_end(&mut text).unwrap();
    let message = producer.finish().unwrap();
    assert!(
      message.starts_with("producer exited with status signal: 9"),
      "{message}"
    );
  }

  #[cfg(unix)]
  #[test]
  fn exec_is_killed_when_its_plot_closes() {
    registry::init_test_dir();
    let name = "ex_kill";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let token = register(name, &listener);
    // The server reads one batch, then closes its window.
    let server = fake_chunks(listener, name, &token, 2, Some(1));
    let pidfile = registry::dir().join("ex_kill.pid");
    let script = format!(
      "echo $$ > '{}'; while :; do echo 0,1; sleep 0.05; done",
      pidfile.display()
    );

    let started = Instant::now();
    let result = execute(&single(&[name, "--exec", script.as_str()]));
    let elapsed = started.elapsed();
    server.join().unwrap();
    let pid = fs::read_to_string(&pidfile).unwrap();
    fs::remove_file(&pidfile).unwrap();
    registry::remove_entry(name);

    assert_eq!(result, Ok(true));
    assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
    assert!(!process_alive(pid.trim()), "sh {} still runs", pid.trim());
  }

  #[cfg(unix)]
  #[test]
  fn idle_exec_is_killed_when_its_plot_closes() {
    registry::init_test_dir();
    let name = "ex_idle";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let token = register(name, &listener);
    // The server closes its window right after the handshake, and the
    // command never writes anything.
    let server = fake_chunks(listener, name, &token, 1, Some(0));
    // A compound command, so that the shell forks it on every Unix, as dash
    // (Linux's sh) does even for a single command.
    let source = Source::Exec("sleep 30; true".to_string());

    let started = Instant::now();
    let result = push_source(&[(name.to_string(), false)], &source);
    let elapsed = started.elapsed();
    server.join().unwrap();
    registry::remove_entry(name);

    let push = result.unwrap();
    assert!(push.failed);
    assert_eq!(push.closed, [name]);
    assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
  }

  #[cfg(unix)]
  #[test]
  fn exit_status_after_the_output_ends_is_reported() {
    registry::init_test_dir();
    let name = "ex_late";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let token = register(name, &listener);
    // The server closes its side 0.6 s after the chunk ends, as a plot does
    // when the chunk is finished and its window is still open for a while.
    let server = thread::spawn(move || {
      let (stream, _) = listener.accept().unwrap();
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
      thread::sleep(Duration::from_millis(600));
      msgs
    });
    // The output ends after 0.3 s, and the command fails a second later. Its
    // plot ending normally must not make the watcher stop the command.
    let source = Source::Exec("echo 0,1; sleep 0.3; exec 1>&-; sleep 1; exit 2".to_string());

    let push = push_source(&[(name.to_string(), false)], &source);
    let msgs = server.join().unwrap();
    registry::remove_entry(name);

    let push = push.unwrap();
    assert!(push.failed, "the exit status was lost");
    assert!(push.closed.is_empty());
    assert_eq!(rows_of(&msgs), ["0,1"]);
  }
}
