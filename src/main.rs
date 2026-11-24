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
use fileflux::core::{admin, owner_helper};
use fileflux::dbus;

/// Exit status for a command line that cannot be honored.
const USAGE_ERROR: u8 = 2;

fn main() -> ExitCode {
    // Before any thread exists (Linux only): the Vulkan loader reads its driver
    // variables while it initialises, so the boot GPU's driver is chosen by
    // replacing this process with itself and one extra variable, never by
    // changing this process's environment. It must stay the first statement.
    #[cfg(target_os = "linux")]
    gpui_linux::select_vulkan_driver();

    match launch() {
        Ok(Launch::Window {
            target,
            forward,
            new_instance,
        }) => {
            gui::init_logging();
            // Only the explorer is a single instance; a viewer launch stays
            // its own process, as before.
            // `--new-instance` and any root process stay out of it: the
            // administrator instance never forwards to or claims the
            // user's name.
            let claims = admin::claims_session_name(new_instance, owner_helper::is_root());
            let desktop = match &target {
                Target::Explorer(_) if claims => {
                    match dbus::claim(&dbus::Endpoint::Session, &forward) {
                        dbus::Role::Primary(primary) => Some(primary),
                        dbus::Role::Forwarded => return ExitCode::SUCCESS,
                        dbus::Role::Standalone => None,
                    }
                }
                Target::Explorer(_) | Target::Viewer(_) => None,
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
        /// `--new-instance`: never forward to or claim the running one.
        new_instance: bool,
    },
    /// Help or version was printed; no window opens.
    Done,
}

fn launch() -> Result<Launch, CliError> {
    let (command, new_instance) = cli::parse_with_flags(std::env::args_os().skip(1))?;
    match command {
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
                new_instance,
            })
        }
    }
}
