//! The Fileflux binary: for now, the setup window only. File browsing comes
//! next.

mod gui;
mod setup;
mod theme;

use std::process::ExitCode;

fn main() -> ExitCode {
    gui::open()
}
