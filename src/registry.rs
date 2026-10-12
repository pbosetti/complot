//! Per-user registry of open plots.
//!
//! Each running plot server owns `<dir>/<name>.plot`, a one-line text file
//! `"<port> <token> <pid>"`, and logs to `<dir>/<name>.log`.

use std::collections::hash_map::RandomState;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A registry entry: how to reach the server of plot `name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
  pub name: String,
  pub port: u16,
  pub token: String,
  pub pid: u32,
}

/// Registry directory, created on demand. `COMPLOT_DIR` is used as-is; the
/// default directory is private (mode 0700 on Unix).
pub fn dir() -> PathBuf {
  if let Some(custom) = std::env::var_os("COMPLOT_DIR").filter(|d| !d.is_empty()) {
    let path = PathBuf::from(custom);
    let _ = fs::create_dir_all(&path);
    return path;
  }
  let base = std::env::var_os("XDG_RUNTIME_DIR")
    .map(PathBuf::from)
    .filter(|p| !p.as_os_str().is_empty() && p.is_dir())
    .unwrap_or_else(std::env::temp_dir);
  let user: String = user_name()
    .chars()
    .map(|c| {
      if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
        c
      } else {
        '_'
      }
    })
    .collect();
  let path = base.join(format!("complot-{user}"));
  let _ = fs::create_dir_all(&path);
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    // In a shared temp dir another user may have created the directory
    // first; then use one under $HOME, which only we can create.
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
      && !same_owner(&path, &home)
    {
      let path = home.join(".complot");
      let _ = fs::create_dir_all(&path);
      let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o700));
      return path;
    }
    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o700));
  }
  path
}

/// True if both paths exist and have the same owner.
#[cfg(unix)]
fn same_owner(a: &Path, b: &Path) -> bool {
  use std::os::unix::fs::MetadataExt;
  match (fs::metadata(a), fs::metadata(b)) {
    (Ok(a), Ok(b)) => a.uid() == b.uid(),
    _ => false,
  }
}

/// `$USER`, else `$USERNAME`, else `user`.
fn user_name() -> String {
  ["USER", "USERNAME"]
    .into_iter()
    .find_map(|var| std::env::var(var).ok().filter(|u| !u.is_empty()))
    .unwrap_or_else(|| "user".to_string())
}

pub fn entry_path(name: &str) -> PathBuf {
  dir().join(format!("{name}.plot"))
}

pub fn log_path(name: &str) -> PathBuf {
  dir().join(format!("{name}.log"))
}

/// Plot names are non-empty, at most 64 chars of `[A-Za-z0-9_.-]`, and do
/// not start with `-` or `.`.
pub fn valid_name(name: &str) -> bool {
  name.len() <= 64
    && !name.is_empty()
    && !name.starts_with(['-', '.'])
    && name
      .bytes()
      .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

pub fn read_entry(name: &str) -> Option<Entry> {
  if !valid_name(name) {
    return None;
  }
  let text = fs::read_to_string(entry_path(name)).ok()?;
  let mut fields = text.split_whitespace();
  let port = fields.next()?.parse().ok()?;
  let token = fields.next()?.to_string();
  let pid = fields.next()?.parse().ok()?;
  if fields.next().is_some() {
    return None;
  }
  Some(Entry {
    name: name.to_string(),
    port,
    token,
    pid,
  })
}

/// Writes the entry atomically (temporary file, then rename), replacing
/// any existing one. Servers use [`claim_entry`].
#[cfg(test)]
pub fn write_entry(entry: &Entry) -> io::Result<()> {
  let tmp = write_temp(entry)?;
  let result = fs::rename(&tmp, entry_path(&entry.name));
  if result.is_err() {
    let _ = fs::remove_file(&tmp);
  }
  result
}

/// Writes the entry atomically unless one already exists for its name.
/// Returns false, writing nothing, if an entry exists (live or stale).
pub fn claim_entry(entry: &Entry) -> io::Result<bool> {
  let tmp = write_temp(entry)?;
  // Unlike a rename, a hard link fails if the target exists, so only one of
  // two servers starting at once can claim the name.
  let result = match fs::hard_link(&tmp, entry_path(&entry.name)) {
    Ok(()) => Ok(true),
    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
    Err(e) => Err(e),
  };
  let _ = fs::remove_file(&tmp);
  result
}

/// Writes the entry to a private temporary file in the registry directory.
fn write_temp(entry: &Entry) -> io::Result<PathBuf> {
  if !valid_name(&entry.name) {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("invalid plot name '{}'", entry.name),
    ));
  }
  let tmp = dir().join(format!(".{}.plot.{}.tmp", entry.name, std::process::id()));
  let result = create_private(&tmp)
    .and_then(|mut file| writeln!(file, "{} {} {}", entry.port, entry.token, entry.pid));
  match result {
    Ok(()) => Ok(tmp),
    Err(e) => {
      let _ = fs::remove_file(&tmp);
      Err(e)
    }
  }
}

/// Creates or truncates `path` for writing; owner-only on Unix, since the
/// file holds the token.
fn create_private(path: &Path) -> io::Result<File> {
  let mut opts = OpenOptions::new();
  opts.write(true).create(true).truncate(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    opts.mode(0o600);
  }
  opts.open(path)
}

/// Removes the entry file of `name`, ignoring errors.
pub fn remove_entry(name: &str) {
  if valid_name(name) {
    let _ = fs::remove_file(entry_path(name));
  }
}

/// Names of all registry entries, sorted. They may be stale.
pub fn names() -> Vec<String> {
  let Ok(entries) = fs::read_dir(dir()) else {
    return Vec::new();
  };
  let mut names: Vec<String> = entries
    .filter_map(Result::ok)
    .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
    .filter_map(|e| e.file_name().into_string().ok())
    .filter(|f| !f.starts_with('.'))
    .filter_map(|f| f.strip_suffix(".plot").map(str::to_string))
    .filter(|n| valid_name(n))
    .collect();
  names.sort();
  names
}

/// Counter that keeps tokens distinct even within one clock tick.
static TOKEN_SEQ: AtomicU64 = AtomicU64::new(0);

/// A fresh random token (128 bits as 32 lowercase hex digits).
pub fn new_token() -> String {
  let nanos = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map_or(0, |d| d.as_nanos());
  let seq = TOKEN_SEQ.fetch_add(1, Ordering::Relaxed);
  let mut token = String::with_capacity(32);
  // Each RandomState is keyed from OS randomness, so the hashes are unpredictable.
  for _ in 0..2 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u32(std::process::id());
    hasher.write_u64(seq);
    hasher.write_u128(nanos);
    let _ = write!(token, "{:016x}", hasher.finish());
  }
  token
}

/// Points `COMPLOT_DIR` at one temporary directory for this test process.
#[cfg(test)]
pub(crate) fn init_test_dir() {
  static INIT: std::sync::Once = std::sync::Once::new();
  INIT.call_once(|| {
    let dir = std::env::temp_dir().join(format!("complot-test-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    // SAFETY: runs once, and every test calls this before touching the registry.
    unsafe { std::env::set_var("COMPLOT_DIR", dir) };
  });
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::HashSet;

  #[test]
  fn valid_name_rules() {
    init_test_dir();
    let long = "x".repeat(64);
    for ok in ["a", "plot1", "My-plot_2.v3", "_x", long.as_str()] {
      assert!(valid_name(ok), "{ok:?} should be valid");
    }
    let too_long = "x".repeat(65);
    for bad in [
      "",
      "-a",
      ".a",
      ".",
      "..",
      "a/b",
      "a b",
      "a\\b",
      "héllo",
      "a\n",
      too_long.as_str(),
    ] {
      assert!(!valid_name(bad), "{bad:?} should be invalid");
    }
  }

  #[test]
  fn paths_are_in_registry_dir() {
    init_test_dir();
    let expected = std::env::var_os("COMPLOT_DIR").map(PathBuf::from).unwrap();
    assert_eq!(dir(), expected);
    assert_eq!(entry_path("x"), dir().join("x.plot"));
    assert_eq!(log_path("x"), dir().join("x.log"));
  }

  #[test]
  fn entry_round_trip() {
    init_test_dir();
    let name = "rg_round";
    let entry = Entry {
      name: name.into(),
      port: 4321,
      token: "abc123".into(),
      pid: 77,
    };
    write_entry(&entry).unwrap();
    assert_eq!(read_entry(name), Some(entry.clone()));
    let newer = Entry {
      port: 5000,
      ..entry
    };
    write_entry(&newer).unwrap();
    assert_eq!(read_entry(name), Some(newer));
    let tmp = dir().join(format!(".{name}.plot.{}.tmp", std::process::id()));
    assert!(!tmp.exists());
    remove_entry(name);
    assert_eq!(read_entry(name), None);
    remove_entry(name);
  }

  #[test]
  fn malformed_entries_are_none() {
    init_test_dir();
    let name = "rg_bad";
    for text in ["", "1234 tok", "x tok 1", "1234 tok 1 extra", "70000 tok 1"] {
      fs::write(entry_path(name), text).unwrap();
      assert_eq!(read_entry(name), None, "{text:?}");
    }
    remove_entry(name);
  }

  #[test]
  fn names_are_sorted_and_ignore_other_files() {
    init_test_dir();
    for name in ["nm_b", "nm_a"] {
      let entry = Entry {
        name: name.into(),
        port: 1,
        token: "t".into(),
        pid: 1,
      };
      write_entry(&entry).unwrap();
    }
    let extra = [
      ".nm_hidden.plot",
      "nm_other.log",
      "nm_bad name.plot",
      "nm_c.plot.1.tmp",
    ];
    for file in extra {
      fs::write(dir().join(file), "1 t 1\n").unwrap();
    }
    fs::create_dir_all(dir().join("nm_dir.plot")).unwrap();

    let all = names();
    assert!(all.windows(2).all(|w| w[0] < w[1]), "{all:?} not sorted");
    // Other tests share the directory, so only look at this test's names.
    let mine: Vec<String> = all.into_iter().filter(|n| n.starts_with("nm_")).collect();
    assert_eq!(mine, ["nm_a", "nm_b"]);

    for file in extra {
      fs::remove_file(dir().join(file)).unwrap();
    }
    fs::remove_dir(dir().join("nm_dir.plot")).unwrap();
    remove_entry("nm_a");
    remove_entry("nm_b");
  }

  #[test]
  fn tokens_are_long_and_unique() {
    init_test_dir();
    let mut seen = HashSet::new();
    for _ in 0..1000 {
      let token = new_token();
      assert!(token.len() >= 32, "{token} is too short");
      assert!(
        token
          .bytes()
          .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "{token} is not lowercase hex"
      );
      assert!(seen.insert(token), "duplicate token");
    }
  }
}
