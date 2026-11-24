//! Child processes: the one place a [`Command`] is built.
//!
//! On Linux, `main` starts on the boot GPU's Vulkan driver:
//! `gpui_linux::select_vulkan_driver` replaces the process with itself, carrying
//! `VK_LOADER_DRIVERS_SELECT` and the `GPUI_VULKAN_DRIVER_SELECT` marker. A child
//! inherits both, so an application started from the explorer — a game launched
//! with PRIME offload, a terminal — would silently run on the boot GPU's driver
//! instead of choosing its own. Every command Fileflux prepares for a child
//! therefore comes from [`command`], which hands it to
//! `gpui_linux::restore_child_environment` before it is spawned; the
//! `disallowed-methods` entry in `clippy.toml` keeps `Command::new` out of every
//! other file.

use std::ffi::OsStr;
use std::process::Command;

/// A command for `program`, with the Vulkan driver selection lifted from the
/// child's environment.
///
/// This is the only place Fileflux builds a [`Command`]: everything else goes
/// through it, so no child keeps the driver `main` selected for this process.
#[expect(
    clippy::disallowed_methods,
    reason = "the sanctioned constructor, and the one place `Command::new` is allowed: \
              the command it builds is handed to `restore_child_environment` below"
)]
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    restore_child_environment(&mut command);
    command
}

/// Removes the Vulkan driver selection `main` made from the child's environment.
#[cfg(target_os = "linux")]
fn restore_child_environment(command: &mut Command) {
    gpui_linux::restore_child_environment(command);
}

/// Nothing to restore: only Linux selects a driver, and the crate is not built
/// there.
#[cfg(not(target_os = "linux"))]
fn restore_child_environment(_command: &mut Command) {}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::env;
    use std::process::Stdio;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// What `gpui_linux::select_vulkan_driver` leaves behind when it pinned this
    /// process to the boot GPU's driver: the ICD patterns it passed to the
    /// loader.
    const MARKER: &str = gpui_linux::SELECTION_VARIABLE;
    /// The loader variable those patterns were passed in, which follows the
    /// marker.
    const LOADER: &str = "VK_LOADER_DRIVERS_SELECT";

    /// The run carrying the marker prepared a child without it.
    ///
    /// `restore_child_environment` reads *this* process's environment, which a
    /// test cannot change: `std::env::set_var` is unsafe and the crate denies
    /// unsafe code, and a parallel test would see the change anyway. So the test
    /// re-runs itself with the marker set — the device `places` uses for its
    /// environment test — and the run that carries the marker reads the command
    /// it prepared.
    #[test]
    fn a_prepared_child_does_not_inherit_the_vulkan_selection() -> TestResult {
        if env::var_os(MARKER).is_none() {
            // With no selection to lift, the child starts with the environment
            // it would have had before: not one variable is changed.
            assert_eq!(
                command("true").get_envs().count(),
                0,
                "a child of a process that selected no driver must keep its environment"
            );
            let output = command(env::current_exe()?)
                .arg("core::child::tests::a_prepared_child_does_not_inherit_the_vulkan_selection")
                .arg("--exact")
                .env(MARKER, "1002:*.json")
                .env(LOADER, "1002:*.json")
                .stdin(Stdio::null())
                .output()?;
            assert!(
                output.status.success(),
                "the run carrying the marker failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return Ok(());
        }

        // Only the two removals are recorded: the constructor sets nothing.
        let prepared: Vec<(String, Option<String>)> = command("true")
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(
            prepared,
            [(MARKER.to_owned(), None), (LOADER.to_owned(), None)],
            "a child must start without the driver selection this process made"
        );
        Ok(())
    }
}
