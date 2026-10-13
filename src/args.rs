//! The command line, split at `NAME:` markers into a part shared by every plot
//! and one part per plot.

use crate::Cli;
use crate::registry::valid_name;
use clap::error::ErrorKind;
use clap::{CommandFactory, Parser};
use std::ffi::OsString;
use std::fmt::Display;
use std::mem::take;
use std::path::PathBuf;

/// What one invocation asks for.
#[derive(Debug)]
pub enum Invocation {
  /// No `NAME:` marker. The first positional is the plot name, as always,
  /// except with `--by`, where every positional is a file.
  Single(Cli),
  /// `[SHARED...] NAME: [ARGS...] NAME: [ARGS...]`. The shared part applies to
  /// every plot; every positional is a file. Plots are in command-line order.
  Multi {
    shared: Cli,
    plots: Vec<(String, Cli)>,
  },
}

/// Parses a full command line, program name first.
pub fn parse<I, T>(args: I) -> Result<Invocation, clap::Error>
where
  I: IntoIterator<Item = T>,
  T: Into<OsString> + Clone,
{
  let mut args: Vec<OsString> = args.into_iter().map(Into::into).collect();
  if args.is_empty() {
    args.push("complot".into());
  }
  let (shared_tokens, plot_tokens) = split(&args[1..]);
  if plot_tokens.is_empty() {
    let mut cli = Cli::try_parse_from(args)?;
    if cli.by.is_some() {
      if cli.y.is_some() {
        return Err(usage_error(
          ErrorKind::ArgumentConflict,
          "--by routes CSV rows, so -y cannot send a single point",
        ));
      }
      name_to_files(&mut cli);
    }
    check_point(&cli)?;
    return Ok(Invocation::Single(cli));
  }

  let program = args[0].clone();
  let shared = parse_part(&program, &shared_tokens)?;
  let mut plots = Vec::with_capacity(plot_tokens.len());
  for (name, tokens) in plot_tokens {
    plots.push((name, parse_part(&program, &tokens)?));
  }

  let exclusive = |cli: &Cli| cli.list || cli.close_all || cli.list_types || cli.serve.is_some();
  if exclusive(&shared) || plots.iter().any(|(_, cli)| exclusive(cli)) {
    return Err(usage_error(
      ErrorKind::ArgumentConflict,
      "--list, --close-all, --list-types and --serve take no plot names",
    ));
  }
  if shared.by.is_some() || plots.iter().any(|(_, cli)| cli.by.is_some()) {
    return Err(usage_error(
      ErrorKind::ArgumentConflict,
      "--by routes rows by itself; don't name plots with NAME:",
    ));
  }
  for (i, (name, _)) in plots.iter().enumerate() {
    if plots[..i].iter().any(|(other, _)| other == name) {
      return Err(usage_error(
        ErrorKind::ValueValidation,
        format!("plot '{name}' is named more than once"),
      ));
    }
  }
  for (_, own) in &plots {
    check_point(&merged(&shared, own))?;
  }
  Ok(Invocation::Multi { shared, plots })
}

/// The options of one plot: its own where given, else the shared ones. Files
/// are its own only, since shared files are sent to every plot separately.
pub fn merged(shared: &Cli, own: &Cli) -> Cli {
  Cli {
    name: None,
    files: own.files.clone(),
    aes: pick(&own.aes, &shared.aes),
    x: own.x.or(shared.x),
    y: own.y.or(shared.y),
    z: own.z.or(shared.z),
    g: pick(&own.g, &shared.g),
    plot_type: pick(&own.plot_type, &shared.plot_type),
    create: own.create || shared.create,
    clear: own.clear || shared.clear,
    close: own.close || shared.close,
    frames: own.frames || shared.frames,
    title: pick(&own.title, &shared.title),
    by: pick(&own.by, &shared.by),
    list: false,
    close_all: false,
    list_types: false,
    serve: None,
  }
}

/// `own` if set, else `shared`.
fn pick<T: Clone>(own: &Option<T>, shared: &Option<T>) -> Option<T> {
  own.clone().or_else(|| shared.clone())
}

/// Cuts the tokens after the program name at markers. Returns the shared
/// tokens, and each plot's name with its tokens. A token that is the value of
/// an option is never a marker, and after `--` (within a part) every token is
/// positional, as clap reads each part on its own.
fn split(tokens: &[OsString]) -> (Vec<OsString>, Vec<(String, Vec<OsString>)>) {
  let opts = ValueOpts::new();
  let (mut shared, mut plots) = (Vec::new(), Vec::new());
  let (mut current, mut name) = (Vec::new(), None::<String>);
  let (mut pending, mut after_dashdash) = (false, false);
  for token in tokens {
    let text = token.to_string_lossy();
    if pending {
      pending = false;
    } else if let Some(plot) = marker(&text) {
      match name.replace(plot.to_string()) {
        Some(done) => plots.push((done, take(&mut current))),
        None => shared = take(&mut current),
      }
      after_dashdash = false;
      continue;
    } else if !after_dashdash {
      if text == "--" {
        after_dashdash = true;
      } else {
        pending = opts.takes_next(&text);
      }
    }
    current.push(token.clone());
  }
  match name {
    Some(last) => plots.push((last, current)),
    None => shared = current,
  }
  (shared, plots)
}

/// The plot name of a `NAME:` marker: a valid name and one trailing colon.
fn marker(text: &str) -> Option<&str> {
  text.strip_suffix(':').filter(|name| valid_name(name))
}

/// The options that take a value, by short and long name, aliases included.
struct ValueOpts {
  shorts: Vec<char>,
  longs: Vec<String>,
}

impl ValueOpts {
  fn new() -> Self {
    let cmd = Cli::command();
    let mut opts = Self {
      shorts: Vec::new(),
      longs: Vec::new(),
    };
    for arg in cmd.get_arguments() {
      if arg.is_positional() || !arg.get_action().takes_values() {
        continue;
      }
      let shorts = arg.get_short().into_iter();
      opts
        .shorts
        .extend(shorts.chain(arg.get_all_short_aliases().into_iter().flatten()));
      let longs = arg.get_long().into_iter();
      opts.longs.extend(
        longs
          .chain(arg.get_all_aliases().into_iter().flatten())
          .map(str::to_string),
      );
    }
    opts
  }

  /// True if `text`, an option without its value, takes the next token as
  /// the value. Clap reads `-xVALUE` and `-x=VALUE` with the value attached.
  fn takes_next(&self, text: &str) -> bool {
    if let Some(long) = text.strip_prefix("--") {
      return !long.contains('=') && self.longs.iter().any(|name| name == long);
    }
    let Some(cluster) = text.strip_prefix('-') else {
      return false;
    };
    // In a cluster such as `-cx`, the first value option takes the rest of the
    // token, or else the next token.
    for (i, c) in cluster.char_indices() {
      if self.shorts.contains(&c) {
        return i + c.len_utf8() == cluster.len();
      }
    }
    false
  }
}

/// Parses one part on its own, with the program name in front. Its
/// positionals are files.
fn parse_part(program: &OsString, tokens: &[OsString]) -> Result<Cli, clap::Error> {
  let line = std::iter::once(program.clone()).chain(tokens.iter().cloned());
  let mut cli = Cli::try_parse_from(line)?;
  name_to_files(&mut cli);
  Ok(cli)
}

/// Moves the positional name, if any, to the front of the files.
fn name_to_files(cli: &mut Cli) {
  if let Some(name) = cli.name.take() {
    cli.files.insert(0, PathBuf::from(name));
  }
}

/// `-x` and `-z` place a point, which also needs `-y`.
fn check_point(cli: &Cli) -> Result<(), clap::Error> {
  if (cli.x.is_some() || cli.z.is_some()) && cli.y.is_none() {
    return Err(usage_error(
      ErrorKind::MissingRequiredArgument,
      "-x and -z need -y",
    ));
  }
  Ok(())
}

/// A clap error of `kind`, with the usage line.
fn usage_error(kind: ErrorKind, message: impl Display) -> clap::Error {
  Cli::command().error(kind, message)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn run(line: &[&str]) -> Result<Invocation, clap::Error> {
    parse(std::iter::once("complot").chain(line.iter().copied()))
  }

  fn single(line: &[&str]) -> Cli {
    match run(line).unwrap() {
      Invocation::Single(cli) => cli,
      other => panic!("expected one plot, got {other:?}"),
    }
  }

  fn multi(line: &[&str]) -> (Cli, Vec<(String, Cli)>) {
    match run(line).unwrap() {
      Invocation::Multi { shared, plots } => (shared, plots),
      other => panic!("expected several plots, got {other:?}"),
    }
  }

  fn kind(line: &[&str]) -> ErrorKind {
    run(line).unwrap_err().kind()
  }

  #[test]
  fn no_marker_is_single() {
    let cli = single(&["temp", "-y", "1"]);
    assert_eq!(cli.name.as_deref(), Some("temp"));
    assert_eq!(cli.y, Some(1.0));
  }

  #[test]
  fn empty_line_is_single() {
    let cli = single(&[]);
    assert!(cli.name.is_none() && cli.files.is_empty());
  }

  #[test]
  fn by_moves_name_to_files() {
    let cli = single(&["--by", "s", "-c", "log.csv"]);
    assert_eq!(cli.files, [PathBuf::from("log.csv")]);
    assert!(cli.name.is_none());
  }

  #[test]
  fn markers_split_plots() {
    let (shared, plots) = multi(&["-x", "3", "temp:", "-y", "21.5", "pres:", "-y", "1013"]);
    assert_eq!(shared.x, Some(3.0));
    let names: Vec<&str> = plots.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["temp", "pres"]);
    let temp = merged(&shared, &plots[0].1);
    let pres = merged(&shared, &plots[1].1);
    assert_eq!((temp.x, temp.y), (Some(3.0), Some(21.5)));
    assert_eq!((pres.x, pres.y), (Some(3.0), Some(1013.0)));
  }

  #[test]
  fn shared_and_plot_files() {
    let (shared, plots) = multi(&["-", "a:", "--aes", "x=t y=a", "b:", "other.csv"]);
    assert_eq!(shared.files, [PathBuf::from("-")]);
    assert!(plots[0].1.files.is_empty());
    assert_eq!(plots[0].1.aes.as_deref(), Some("x=t y=a"));
    assert_eq!(plots[1].1.files, [PathBuf::from("other.csv")]);
  }

  #[test]
  fn plot_name_then_files_all_become_files() {
    let (_, plots) = multi(&["a:", "x.csv", "y.csv"]);
    assert_eq!(
      plots[0].1.files,
      [PathBuf::from("x.csv"), PathBuf::from("y.csv")]
    );
  }

  #[test]
  fn option_values_are_not_markers() {
    let (_, plots) = multi(&["a:", "--title", "ratio:"]);
    assert_eq!(plots.len(), 1);
    assert_eq!(plots[0].1.title.as_deref(), Some("ratio:"));

    let (shared, plots) = multi(&["--aes=x:", "-gx:", "a:"]);
    assert_eq!(shared.aes.as_deref(), Some("x:"));
    assert_eq!(shared.g.as_deref(), Some("x:"));
    assert_eq!(plots[0].0, "a");

    let (shared, plots) = multi(&["-cg", "grp:", "a:"]);
    assert!(shared.create);
    assert_eq!(shared.g.as_deref(), Some("grp:"));
    assert_eq!(plots.len(), 1);
  }

  #[test]
  fn negative_and_attached_values() {
    let (shared, plots) = multi(&["-x", "-1.5", "a:", "-y", "-1.5"]);
    assert_eq!(shared.x, Some(-1.5));
    assert_eq!(plots[0].1.y, Some(-1.5));

    let (shared, plots) = multi(&["-x3", "a:", "-y=-2", "b:", "-y1"]);
    assert_eq!(shared.x, Some(3.0));
    assert_eq!(plots[0].1.y, Some(-2.0));
    assert_eq!(plots[1].1.y, Some(1.0));
  }

  #[test]
  fn non_names_are_not_markers() {
    assert_eq!(single(&["a/b:"]).name.as_deref(), Some("a/b:"));
    let cli = single(&["temp", "x y:", ":"]);
    assert_eq!(cli.files, [PathBuf::from("x y:"), PathBuf::from(":")]);
    let (_, plots) = multi(&["a:", "a/b:", "b:"]);
    assert_eq!(plots[0].1.files, [PathBuf::from("a/b:")]);
  }

  #[test]
  fn drive_letter_is_a_marker() {
    let (_, plots) = multi(&["C:", "-y", "1"]);
    assert_eq!(plots[0].0, "C");
    assert_eq!(plots[0].1.y, Some(1.0));
  }

  #[test]
  fn dashdash_keeps_markers() {
    let (_, plots) = multi(&["a:", "--", "-weird.csv"]);
    assert_eq!(plots[0].1.files, [PathBuf::from("-weird.csv")]);

    let (_, plots) = multi(&["a:", "--", "-weird.csv", "b:", "other.csv"]);
    assert_eq!(plots[0].1.files, [PathBuf::from("-weird.csv")]);
    assert_eq!(plots[1].1.files, [PathBuf::from("other.csv")]);

    let (_, plots) = multi(&["a:", "--", "-y", "b:", "-y", "1"]);
    assert_eq!(plots[0].1.files, [PathBuf::from("-y")]);
    assert_eq!(plots[0].1.y, None);
    assert_eq!(plots[1].1.y, Some(1.0));
  }

  #[test]
  fn duplicate_plot_name_is_error() {
    assert_eq!(
      kind(&["a:", "-y", "1", "a:", "-y", "2"]),
      ErrorKind::ValueValidation
    );
  }

  #[test]
  fn exclusive_options_are_errors_with_markers() {
    let lines: [&[&str]; 4] = [
      &["--list", "a:"],
      &["a:", "--close-all"],
      &["--list-types", "a:"],
      &["--serve", "x", "a:"],
    ];
    for line in lines {
      assert_eq!(kind(line), ErrorKind::ArgumentConflict, "{line:?}");
    }
  }

  #[test]
  fn by_with_markers_is_error() {
    assert_eq!(
      kind(&["--by", "s", "a:", "-y", "1"]),
      ErrorKind::ArgumentConflict
    );
    assert_eq!(kind(&["a:", "--by", "s"]), ErrorKind::ArgumentConflict);
  }

  #[test]
  fn point_needs_y() {
    assert_eq!(
      kind(&["temp", "-x", "1"]),
      ErrorKind::MissingRequiredArgument
    );
    assert_eq!(
      kind(&["temp", "-z", "1"]),
      ErrorKind::MissingRequiredArgument
    );
    assert_eq!(
      kind(&["-x", "3", "temp:", "-y", "1", "pres:"]),
      ErrorKind::MissingRequiredArgument
    );
  }

  #[test]
  fn by_with_y_is_error() {
    assert_eq!(
      kind(&["--by", "s", "-y", "1", "log.csv"]),
      ErrorKind::ArgumentConflict
    );
  }

  #[test]
  fn clap_errors_pass_through() {
    assert!(run(&["a:", "-y", "nope"]).is_err());
  }

  #[test]
  fn merged_prefers_own_options() {
    let (shared, plots) = multi(&[
      "-x", "3", "-c", "--title", "shared", "-", "p:", "-x", "4", "-y", "2", "--aes", "x=t y=b",
      "--clear", "f.csv",
    ]);
    assert_eq!(shared.files, [PathBuf::from("-")]);
    let cli = merged(&shared, &plots[0].1);
    assert_eq!(cli.x, Some(4.0));
    assert_eq!(cli.y, Some(2.0));
    assert_eq!(cli.aes.as_deref(), Some("x=t y=b"));
    assert_eq!(cli.title.as_deref(), Some("shared"));
    assert!(cli.create && cli.clear && !cli.close);
    assert_eq!(cli.files, [PathBuf::from("f.csv")]);
    assert!(cli.name.is_none());
  }

  #[test]
  fn merged_clears_exclusive_flags() {
    let listing = Cli::try_parse_from(["complot", "--list"]).unwrap();
    let cli = merged(&listing, &listing);
    assert!(!cli.list && !cli.close_all && !cli.list_types && cli.serve.is_none());
  }
}
