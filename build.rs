//! Sets `COMPLOT_VERSION` and `COMPLOT_TARGET` for the binary.
//!
//! The version comes from the first source that works: the `COMPLOT_VERSION`
//! env var (CI and packagers), the nearest `v*` git tag, the commit hash when
//! there is no tag, and finally the Cargo package version.

use std::env;
use std::path::Path;
use std::process::Command;

fn main() {
  let dir = env::var("CARGO_MANIFEST_DIR").expect("set by cargo");
  let pkg = env::var("CARGO_PKG_VERSION").expect("set by cargo");

  // Without this, cargo would not rerun the script when the env var changes.
  println!("cargo:rerun-if-env-changed=COMPLOT_VERSION");
  watch(&dir);

  let version = env::var("COMPLOT_VERSION")
    .ok()
    .and_then(|v| clean_env(&v))
    .or_else(|| git_version(&dir, &pkg))
    .unwrap_or(pkg);

  println!("cargo:rustc-env=COMPLOT_VERSION={version}");
  println!(
    "cargo:rustc-env=COMPLOT_TARGET={}",
    env::var("TARGET").expect("set by cargo")
  );
}

/// Trims an env value and drops a leading `v`; `None` if nothing is left.
fn clean_env(raw: &str) -> Option<String> {
  let v = raw.trim();
  let v = v.strip_prefix('v').unwrap_or(v);
  (!v.is_empty()).then(|| v.to_string())
}

/// Runs `git -C dir ARGS`, returning trimmed stdout on success.
/// Any failure (no git binary, not a repo, non-zero exit) gives `None`.
fn git(dir: &str, args: &[&str]) -> Option<String> {
  let out = Command::new("git")
    .arg("-C")
    .arg(dir)
    .args(args)
    .output()
    .ok()?;
  if !out.status.success() {
    return None;
  }
  let stdout = String::from_utf8(out.stdout).ok()?;
  let s = stdout.trim();
  (!s.is_empty()).then(|| s.to_string())
}

/// Version derived from git, or `None` when git cannot describe HEAD.
fn git_version(dir: &str, pkg: &str) -> Option<String> {
  // A tag: `0.2.0`, `0.2.0-3-gabc1234`, optionally with `-dirty`.
  if let Some(tag) = git(
    dir,
    &["describe", "--tags", "--match", "v[0-9]*", "--dirty"],
  ) {
    return Some(tag.strip_prefix('v').unwrap_or(&tag).to_string());
  }
  // No tag: `f6da575` or `f6da575-dirty`. Semver build metadata (`+...`) is
  // ignored for precedence, so this still sorts as the package version.
  // `--match` keeps an annotated non-version tag (e.g. `foo-1-gabc`) out.
  let desc = git(
    dir,
    &["describe", "--always", "--dirty", "--match", "v[0-9]*"],
  )?;
  let (sha, dirty) = match desc.strip_suffix("-dirty") {
    Some(sha) => (sha, true),
    None => (desc.as_str(), false),
  };
  let suffix = if dirty { ".dirty" } else { "" };
  Some(format!("{pkg}+g{sha}{suffix}"))
}

/// Tells cargo what to watch, so the script reruns when the version can change.
///
/// Cargo reruns the script on every build when a listed path is missing, so
/// only existing paths are listed. In a git worktree `.git` is a file, so the
/// `.git/...` paths are skipped there.
fn watch(dir: &str) {
  let root = Path::new(dir);
  // `src` keeps the `-dirty` flag current after source edits. The `.git`
  // entries catch commits and tags. `logs/HEAD` is the reflog, which also
  // catches branch-only moves such as `git reset --soft`.
  for path in [
    "src",
    ".git/HEAD",
    ".git/index",
    ".git/logs/HEAD",
    ".git/refs/tags",
    ".git/packed-refs",
  ] {
    if root.join(path).exists() {
      println!("cargo:rerun-if-changed={path}");
    }
  }
}
