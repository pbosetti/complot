//! complot: real-time plots driven from the shell.
//!
//! Every plot is a separate GUI process (the server) identified by a name,
//! the handle. Short-lived `complot` invocations (clients) push data to it.

mod client;
mod gui;
mod lod;
mod model;
mod protocol;
mod registry;
mod server;

use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser, Debug)]
#[command(
  name = "complot",
  version,
  about = "Real-time plots from the shell",
  allow_negative_numbers = true
)]
pub struct Cli {
  /// Plot name (the handle). Omit to create a plot with an automatic name.
  pub name: Option<String>,

  /// CSV files to push; `-` reads stdin and streams it line by line.
  #[arg(conflicts_with = "y")]
  pub files: Vec<PathBuf>,

  /// Aesthetic mapping, e.g. "x=t y=a,b g=sensor".
  #[arg(long)]
  pub aes: Option<String>,

  /// x value of a single point (default: next sample index of the series).
  #[arg(short = 'x', requires = "y")]
  pub x: Option<f64>,

  /// y value of a single point.
  #[arg(short = 'y')]
  pub y: Option<f64>,

  /// Group (series) of a single point.
  #[arg(short = 'g', long = "group", requires = "y")]
  pub g: Option<String>,

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

  /// List open plots.
  #[arg(short = 'l', long, exclusive = true)]
  pub list: bool,

  /// Close all open plots.
  #[arg(long, exclusive = true)]
  pub close_all: bool,

  /// Internal: run the plot server for NAME.
  #[arg(long, hide = true, exclusive = true, value_name = "NAME")]
  pub serve: Option<String>,
}

fn main() -> ExitCode {
  let cli = Cli::parse();
  if let Some(name) = &cli.serve {
    return match server::run(name) {
      Ok(()) => ExitCode::SUCCESS,
      Err(e) => {
        eprintln!("complot server: {e}");
        ExitCode::FAILURE
      }
    };
  }
  client::run(&cli)
}
