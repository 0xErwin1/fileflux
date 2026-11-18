//! The Fileflux binary: `fileflux [PATH]` opens the explorer window.

mod explorer;
mod gui;
mod theme;

use std::path::PathBuf;
use std::process::ExitCode;

use fileflux::cli::{self, CliError, Command};

/// Exit status for a command line that cannot be honored.
const USAGE_ERROR: u8 = 2;

fn main() -> ExitCode {
    match launch() {
        Ok(Launch::Window(start)) => gui::open(start),
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
    /// Open the window, at the resolved directory when a path was passed.
    Window(Option<PathBuf>),
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
        Command::Open(None) => Ok(Launch::Window(None)),
        Command::Open(Some(path)) => {
            let cwd = std::env::current_dir()
                .map_err(|error| CliError::Unreadable(PathBuf::from("."), error.to_string()))?;
            let home = std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from);
            cli::resolve_start(&path, &cwd, home.as_deref()).map(|dir| Launch::Window(Some(dir)))
        }
    }
}
