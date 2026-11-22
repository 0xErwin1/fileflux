//! Failure reasons in plain words.
//!
//! The OS and the standard library describe a failure as "Permission denied
//! (os error 13)". That is right for a log and wrong for a person, so every
//! notice and detail line shows [`plain_reason`] instead; the original
//! `Display` stays for logs.

use std::io;

use rustix::io::Errno;

use super::archive::ArchiveError;
use super::ops::{OpError, TransferError};
use super::trash::TrashError;

const NO_PERMISSION: &str = "you don't have permission to do that";
const NO_SPACE: &str = "there is no space left on the device";
const QUOTA: &str = "your disk quota is used up";
const READ_ONLY: &str = "the location is read-only";
const EXISTS: &str = "an item with that name already exists";
const NOT_EMPTY: &str = "the folder is not empty";
const OTHER_DEVICE: &str = "the source and the destination are on different devices";
const NAME_TOO_LONG: &str = "the name is too long";
const BUSY: &str = "the item is in use by the system or another program";
const RUNNING: &str = "the file is a program that is running";
const GONE: &str = "it no longer exists";
const DEVICE_ERROR: &str = "the device reported a read or write error";
const NO_DEVICE: &str = "the device was removed or is not available";
const IS_FOLDER: &str = "it is a folder, not a file";
const NOT_FOLDER: &str = "part of the path is not a folder";
const LOOP: &str = "there are too many levels of symbolic links";
const TOO_MANY_LINKS: &str = "the file has too many links";
const TOO_LARGE: &str = "the file is too large for this filesystem";
const STALE: &str = "the network location is no longer valid";
const NETWORK: &str = "the network connection was lost";
const NO_MEMORY: &str = "the system is out of memory";
const UNSUPPORTED: &str = "this filesystem does not support that";
const TOO_MANY_OPEN: &str = "too many files are open";

/// The reason `error` is shown to the user, in plain English: the raw OS
/// error first (it is the most precise), then the error kind, and for
/// anything else the OS text without its " (os error N)" suffix.
pub fn plain_reason(error: &io::Error) -> String {
    if let Some(raw) = error.raw_os_error()
        && let Some(words) = by_errno(Errno::from_raw_os_error(raw))
    {
        return words.to_owned();
    }
    if let Some(words) = by_kind(error.kind()) {
        return words.to_owned();
    }
    let text = error.to_string();
    match text.rfind(" (os error ") {
        Some(at) if text.ends_with(')') => text[..at].to_owned(),
        _ => text,
    }
}

fn by_errno(errno: Errno) -> Option<&'static str> {
    Some(match errno {
        Errno::ACCESS | Errno::PERM => NO_PERMISSION,
        Errno::NOSPC => NO_SPACE,
        Errno::DQUOT => QUOTA,
        Errno::ROFS => READ_ONLY,
        Errno::EXIST => EXISTS,
        Errno::NOTEMPTY => NOT_EMPTY,
        Errno::XDEV => OTHER_DEVICE,
        Errno::NAMETOOLONG => NAME_TOO_LONG,
        Errno::BUSY => BUSY,
        Errno::TXTBSY => RUNNING,
        Errno::NOENT => GONE,
        Errno::IO => DEVICE_ERROR,
        Errno::NODEV | Errno::NXIO => NO_DEVICE,
        Errno::ISDIR => IS_FOLDER,
        Errno::NOTDIR => NOT_FOLDER,
        Errno::LOOP => LOOP,
        Errno::MLINK => TOO_MANY_LINKS,
        Errno::FBIG => TOO_LARGE,
        Errno::STALE => STALE,
        Errno::CONNRESET
        | Errno::CONNABORTED
        | Errno::CONNREFUSED
        | Errno::TIMEDOUT
        | Errno::HOSTUNREACH
        | Errno::HOSTDOWN
        | Errno::NETDOWN
        | Errno::NETUNREACH
        | Errno::NOTCONN => NETWORK,
        Errno::NOMEM => NO_MEMORY,
        Errno::NOTSUP => UNSUPPORTED,
        Errno::MFILE | Errno::NFILE => TOO_MANY_OPEN,
        _ => return None,
    })
}

fn by_kind(kind: io::ErrorKind) -> Option<&'static str> {
    use io::ErrorKind as K;
    Some(match kind {
        K::PermissionDenied => NO_PERMISSION,
        K::NotFound => GONE,
        K::AlreadyExists => EXISTS,
        K::StorageFull => NO_SPACE,
        K::QuotaExceeded => QUOTA,
        K::ReadOnlyFilesystem => READ_ONLY,
        K::DirectoryNotEmpty => NOT_EMPTY,
        K::CrossesDevices => OTHER_DEVICE,
        K::InvalidFilename => "the name is too long or not allowed here",
        K::ResourceBusy | K::ExecutableFileBusy => BUSY,
        K::IsADirectory => IS_FOLDER,
        K::NotADirectory => NOT_FOLDER,
        K::TooManyLinks => TOO_MANY_LINKS,
        K::FileTooLarge => TOO_LARGE,
        K::StaleNetworkFileHandle => STALE,
        K::ConnectionReset
        | K::ConnectionAborted
        | K::ConnectionRefused
        | K::TimedOut
        | K::HostUnreachable
        | K::NetworkDown
        | K::NetworkUnreachable
        | K::NotConnected => NETWORK,
        K::OutOfMemory => NO_MEMORY,
        K::Unsupported => UNSUPPORTED,
        _ => return None,
    })
}

impl TransferError {
    /// Why this item failed, in plain words (`Display` stays for logs).
    pub fn plain(&self) -> String {
        match self {
            TransferError::Io(error) => plain_reason(error),
            TransferError::Unsupported(_) => {
                "it is a device, socket or pipe and can't be copied".to_owned()
            }
            TransferError::IntoItself => "a folder can't be placed inside itself".to_owned(),
            TransferError::VerifyFailed(_) => "the copy did not match the original".to_owned(),
            TransferError::Cancelled => "cancelled".to_owned(),
            TransferError::SourceNotRemoved(error) => format!(
                "copied, but the original could not be removed: {}",
                plain_reason(error)
            ),
        }
    }
}

impl TrashError {
    /// Why the Trash operation failed, in plain words.
    pub fn plain(&self) -> String {
        match self {
            TrashError::NoTrash(_) => "there is no usable Trash for it".to_owned(),
            TrashError::Invalid(_) => "the Trash entry is damaged".to_owned(),
            TrashError::Io(error) => plain_reason(error),
        }
    }
}

impl OpError {
    /// Why the create or rename failed, in plain words.
    pub fn plain(&self) -> String {
        match self {
            OpError::Name(error) => error.to_string(),
            OpError::Io(error) => plain_reason(error),
        }
    }
}

impl ArchiveError {
    /// Why the compress or extract failed, in plain words. `bsdtar`'s own
    /// message is kept as it is.
    pub fn plain(&self) -> String {
        match self {
            ArchiveError::Io(error) => plain_reason(error),
            other => other.to_string(),
        }
    }
}

/// The reasons of several failures in one phrase: each distinct reason once,
/// most common first, with its count when there is more than one reason
/// ("no permission (2), no space (1)").
pub fn group_reasons<'a>(reasons: impl IntoIterator<Item = &'a str>) -> String {
    let mut groups: Vec<(&str, usize)> = Vec::new();
    for reason in reasons {
        match groups.iter_mut().find(|(seen, _)| *seen == reason) {
            Some((_, count)) => *count += 1,
            None => groups.push((reason, 1)),
        }
    }
    // Stable: equally common reasons keep the order they first appeared in.
    groups.sort_by_key(|group| std::cmp::Reverse(group.1));
    match groups.as_slice() {
        [(only, _)] => (*only).to_owned(),
        many => many
            .iter()
            .map(|(reason, count)| format!("{reason} ({count})"))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use io::ErrorKind;
    use rustix::io::Errno;

    fn raw(errno: Errno) -> io::Error {
        io::Error::from_raw_os_error(errno.raw_os_error())
    }

    #[test]
    fn every_known_errno_has_its_own_words() {
        let table: &[(Errno, &str)] = &[
            (Errno::ACCESS, "you don't have permission to do that"),
            (Errno::PERM, "you don't have permission to do that"),
            (Errno::NOSPC, "there is no space left on the device"),
            (Errno::DQUOT, "your disk quota is used up"),
            (Errno::ROFS, "the location is read-only"),
            (Errno::EXIST, "an item with that name already exists"),
            (Errno::NOTEMPTY, "the folder is not empty"),
            (
                Errno::XDEV,
                "the source and the destination are on different devices",
            ),
            (Errno::NAMETOOLONG, "the name is too long"),
            (
                Errno::BUSY,
                "the item is in use by the system or another program",
            ),
            (Errno::TXTBSY, "the file is a program that is running"),
            (Errno::NOENT, "it no longer exists"),
            (Errno::IO, "the device reported a read or write error"),
            (Errno::NODEV, "the device was removed or is not available"),
            (Errno::NXIO, "the device was removed or is not available"),
            (Errno::ISDIR, "it is a folder, not a file"),
            (Errno::NOTDIR, "part of the path is not a folder"),
            (Errno::LOOP, "there are too many levels of symbolic links"),
            (Errno::MLINK, "the file has too many links"),
            (Errno::FBIG, "the file is too large for this filesystem"),
            (Errno::STALE, "the network location is no longer valid"),
            (Errno::CONNRESET, "the network connection was lost"),
            (Errno::CONNABORTED, "the network connection was lost"),
            (Errno::TIMEDOUT, "the network connection was lost"),
            (Errno::HOSTUNREACH, "the network connection was lost"),
            (Errno::NETDOWN, "the network connection was lost"),
            (Errno::NETUNREACH, "the network connection was lost"),
            (Errno::NOTCONN, "the network connection was lost"),
            (Errno::NOMEM, "the system is out of memory"),
            (Errno::NOTSUP, "this filesystem does not support that"),
            (Errno::MFILE, "too many files are open"),
        ];
        for (errno, expected) in table {
            assert_eq!(plain_reason(&raw(*errno)), *expected, "{errno:?}");
        }
    }

    #[test]
    fn every_known_kind_has_its_own_words() {
        let table: &[(ErrorKind, &str)] = &[
            (
                ErrorKind::PermissionDenied,
                "you don't have permission to do that",
            ),
            (ErrorKind::NotFound, "it no longer exists"),
            (
                ErrorKind::AlreadyExists,
                "an item with that name already exists",
            ),
            (
                ErrorKind::StorageFull,
                "there is no space left on the device",
            ),
            (ErrorKind::QuotaExceeded, "your disk quota is used up"),
            (ErrorKind::ReadOnlyFilesystem, "the location is read-only"),
            (ErrorKind::DirectoryNotEmpty, "the folder is not empty"),
            (
                ErrorKind::CrossesDevices,
                "the source and the destination are on different devices",
            ),
            (
                ErrorKind::InvalidFilename,
                "the name is too long or not allowed here",
            ),
            (
                ErrorKind::ResourceBusy,
                "the item is in use by the system or another program",
            ),
            (ErrorKind::IsADirectory, "it is a folder, not a file"),
            (ErrorKind::NotADirectory, "part of the path is not a folder"),
            (ErrorKind::TooManyLinks, "the file has too many links"),
            (
                ErrorKind::FileTooLarge,
                "the file is too large for this filesystem",
            ),
            (
                ErrorKind::StaleNetworkFileHandle,
                "the network location is no longer valid",
            ),
            (
                ErrorKind::ConnectionReset,
                "the network connection was lost",
            ),
            (ErrorKind::TimedOut, "the network connection was lost"),
            (ErrorKind::OutOfMemory, "the system is out of memory"),
            (
                ErrorKind::Unsupported,
                "this filesystem does not support that",
            ),
        ];
        for (kind, expected) in table {
            assert_eq!(plain_reason(&io::Error::from(*kind)), *expected, "{kind:?}");
        }
    }

    #[test]
    fn an_unknown_error_keeps_the_os_text_without_the_code() {
        assert_eq!(
            plain_reason(&raw(Errno::DOM)),
            "Numerical argument out of domain"
        );
        assert_eq!(
            plain_reason(&io::Error::other("it is a symbolic link now")),
            "it is a symbolic link now"
        );
    }

    #[test]
    fn no_reason_shows_an_os_error_code() {
        for code in 1..=133 {
            let reason = plain_reason(&io::Error::from_raw_os_error(code));
            assert!(!reason.contains("os error"), "{code}: {reason}");
        }
    }

    #[test]
    fn reasons_are_grouped_with_counts_most_common_first() {
        let grouped = group_reasons(["no space", "no permission", "no permission"]);
        assert_eq!(grouped, "no permission (2), no space (1)");
        assert_eq!(group_reasons(["x", "x"]), "x");
        assert_eq!(group_reasons(["b", "a"]), "b (1), a (1)");
        assert_eq!(group_reasons([]), "");
    }
}
