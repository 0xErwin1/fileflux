//! The Fileflux binary: `fileflux [PATH]` opens the explorer window and
//! `fileflux IMAGE...` the image viewer.

mod explorer;
mod gui;
mod theme;
mod ui_automation;
mod viewer;

use std::path::PathBuf;
use std::process::ExitCode;

use fileflux::cli::{self, CliError, Command, Target};
use fileflux::dbus;

/// Exit status for a command line that cannot be honored.
const USAGE_ERROR: u8 = 2;

fn main() -> ExitCode {
    match launch() {
        Ok(Launch::Window { target, forward }) => {
            gui::init_logging();
            // Only the explorer is a single instance; a viewer launch stays
            // its own process, as before.
            let desktop = match &target {
                Target::Explorer(_) => match dbus::claim(&dbus::Endpoint::Session, &forward) {
                    dbus::Role::Primary(primary) => Some(primary),
                    dbus::Role::Forwarded => return ExitCode::SUCCESS,
                    dbus::Role::Standalone => None,
                },
                Target::Viewer(_) => None,
            };
            gui::open(target, desktop)
        }
        Ok(Launch::Done) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fileflux: {error}");
            if error.suggests_help() {
                eprintln!("Try 'fileflux --help' for more information.");
            }
            ExitCode::from(USAGE_ERROR)
        }
    }
}

/// What the binary does after reading its command line.
enum Launch {
    /// Open the explorer or the viewer, as the resolved command line asks.
    /// `forward` is what a running instance is handed instead: the absolute
    /// command-line paths.
    Window {
        target: Target,
        forward: Vec<PathBuf>,
    },
    /// Help or version was printed; no window opens.
    Done,
}

fn launch() -> Result<Launch, CliError> {
    match cli::parse(std::env::args_os().skip(1))? {
        Command::Help => {
            print!("{}", cli::USAGE);
            Ok(Launch::Done)
        }
        Command::Version => {
            println!("fileflux {}", env!("CARGO_PKG_VERSION"));
            Ok(Launch::Done)
        }
        Command::Open(paths) => {
            let home = std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from);
            let resolved = cli::resolve(&paths, std::env::current_dir, home.as_deref())?;
            for note in &resolved.notes {
                eprintln!("fileflux: {note}");
            }
            let forward = paths
                .iter()
                .map(|path| cli::absolute_path(path, std::env::current_dir, home.as_deref()))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Launch::Window {
                target: resolved.target,
                forward,
            })
        }
    }
}
