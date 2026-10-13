//! complot: real-time plots driven from the shell.
//!
//! Every plot is a separate GUI process (the server) identified by a name,
//! the handle. Short-lived `complot` invocations (clients) push data to it.

mod args;
mod client;
mod gui;
mod hist;
mod lod;
mod model;
mod protocol;
mod registry;
mod route;
mod scale;
mod server;
#[cfg(test)]
mod snapshot;
mod view3d;

use args::Invocation;
use clap::Parser;
use model::PlotType;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser, Debug, Clone)]
#[command(
  name = "complot",
  version = env!("COMPLOT_VERSION"),
  long_version = concat!(env!("COMPLOT_VERSION"), "\ntarget: ", env!("COMPLOT_TARGET")),
  about = "Real-time plots from the shell",
  allow_negative_numbers = true,
  after_help = "\
Several plots at once: name each with a trailing colon. Options and files before
the first NAME: apply to every plot; every other argument belongs to the NAME:
before it, and all positionals are then files.
  complot -x 3 temp: -y 21.5 pres: -y 1013
  tail -f log.csv | complot - temp: --aes \"x=t y=temp\" pres: --aes \"x=t y=pres\"

Routing: --by COL sends each CSV row to the plot named by its value in column
COL (that column is not forwarded).
  sensor_log | complot --by sensor --create -"
)]
pub struct Cli {
  /// Plot name (the handle). Omit to create a plot with an automatic name.
  pub name: Option<String>,

  /// CSV files to push; `-` reads stdin and streams it line by line.
  pub files: Vec<PathBuf>,

  /// Aesthetic mapping, e.g. "x=t y=a,b g=sensor".
  #[arg(long)]
  pub aes: Option<String>,

  /// x value of a single point (default: next sample index of the series).
  // `-x` and `-z` need `-y`, but possibly from another part of the command
  // line (`-x 3 temp: -y 21`), so `args` checks that after merging.
  #[arg(short = 'x')]
  pub x: Option<f64>,

  /// y value of a single point.
  #[arg(short = 'y')]
  pub y: Option<f64>,

  /// z value of a single point, for 3D plots.
  #[arg(short = 'z')]
  pub z: Option<f64>,

  /// Group (series) of a single point, or the group `-t` applies to.
  #[arg(short = 'g', long = "group")]
  pub g: Option<String>,

  /// Plot type (see --list-types); with -g, of that group only.
  #[arg(short = 't', long = "type", value_enum, value_name = "TYPE")]
  pub plot_type: Option<PlotType>,

  /// Create the plot if it is not open (otherwise pushing to it is an error).
  #[arg(short = 'c', long)]
  pub create: bool,

  /// Remove all data from the plot.
  #[arg(long)]
  pub clear: bool,

  /// Close the plot window.
  #[arg(long)]
  pub close: bool,

  /// Set the window title.
  #[arg(long)]
  pub title: Option<String>,

  /// Route each CSV row to the plot named by column COL (header name or 1-based index).
  #[arg(long, value_name = "COL")]
  pub by: Option<String>,

  /// List open plots.
  #[arg(short = 'l', long, exclusive = true)]
  pub list: bool,

  /// Close all open plots.
  #[arg(long, exclusive = true)]
  pub close_all: bool,

  /// List the plot types accepted by -t.
  #[arg(long, exclusive = true, alias = "list_types")]
  pub list_types: bool,

  /// Internal: run the plot server for NAME.
  #[arg(long, hide = true, exclusive = true, value_name = "NAME")]
  pub serve: Option<String>,
}

fn main() -> ExitCode {
  let invocation = args::parse(std::env::args_os()).unwrap_or_else(|e| e.exit());
  if let Invocation::Single(cli) = &invocation
    && let Some(name) = &cli.serve
  {
    return match server::run(name) {
      Ok(()) => ExitCode::SUCCESS,
      Err(e) => {
        eprintln!("complot server: {e}");
        ExitCode::FAILURE
      }
    };
  }
  client::run(&invocation)
}

#[cfg(test)]
mod tests {
  #[test]
  fn version_starts_with_digit() {
    let v = env!("COMPLOT_VERSION");
    assert!(
      v.chars().next().is_some_and(|c| c.is_ascii_digit()),
      "COMPLOT_VERSION must start with a digit, got {v:?}"
    );
  }
}
