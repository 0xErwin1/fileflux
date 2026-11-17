//! The Fileflux binary: the read-only explorer window.

mod explorer;
mod gui;
mod theme;

use std::process::ExitCode;

fn main() -> ExitCode {
    gui::open()
}
