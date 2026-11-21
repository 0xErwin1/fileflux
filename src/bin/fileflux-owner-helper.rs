//! `fileflux-owner-helper`: changes the owner of one file or folder as root,
//! started by Fileflux through `pkexec`. It checks that the path is still the
//! file the panel showed and changes the owner on that same open descriptor.
//! No GUI, no stdin, no other program; see `core/owner_helper.rs`.
//!
//! Usage: `fileflux-owner-helper PATH DEV INO BIRTH|none OWNER_UID NEW_UID`
//! (BIRTH is `SECONDS:NANOSECONDS`). Silent on stdout; a failure writes one
//! line to stderr and exits with the code of `Exit`.

#[path = "../core/owner_helper.rs"]
#[allow(
    dead_code,
    unreachable_pub,
    reason = "the module is shared with the library, which uses the rest of it"
)]
mod owner_helper;

use std::process::ExitCode;

fn main() -> ExitCode {
    let result = owner_helper::Request::parse(std::env::args_os().skip(1))
        .and_then(|request| owner_helper::run(&request, owner_helper::is_root()));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            eprintln!("{}: {}", owner_helper::HELPER_NAME, failure.reason);
            ExitCode::from(failure.exit.code())
        }
    }
}
