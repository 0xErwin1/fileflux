//! The file-clipboard conventions of Linux file managers, as pure functions.
//!
//! GNOME Files, Nemo, Caja, PCManFM and Dolphin exchange copied and cut files
//! through `x-special/gnome-copied-files` (`copy` or `cut`, a newline, then
//! one `file://` URI per line) and `text/uri-list` (`file://` URIs separated
//! by CRLF). This module formats and parses those bodies and the plain-text
//! forms of them; it touches neither the clipboard nor, except through the
//! predicate its caller passes, the filesystem.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use super::trash::{decode_percent, encode_percent};

/// What the source of a clipboard asked the paste to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardAction {
    Copy,
    Cut,
}

/// Files read from a clipboard text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardFiles {
    pub action: ClipboardAction,
    pub paths: Vec<PathBuf>,
}

/// The `file://` URI of an absolute path, percent-encoded; `None` for a
/// relative path, which has no URI.
#[must_use]
pub fn path_to_uri(path: &Path) -> Option<String> {
    path.is_absolute()
        .then(|| format!("file://{}", encode_percent(path.as_os_str().as_bytes())))
}

/// The local path of a `file:` URI; `None` for any other scheme, a host other
/// than `localhost`, a relative path or a malformed escape. A `#` that was
/// not encoded stays part of the name: file managers always encode it.
#[must_use]
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let (scheme, rest) = uri.split_once(':')?;
    if !scheme.eq_ignore_ascii_case("file") {
        return None;
    }
    let rest = rest.strip_prefix("//")?;
    let (host, path) = rest.split_at(rest.find('/')?);
    if !(host.is_empty() || host.eq_ignore_ascii_case("localhost")) {
        return None;
    }
    let bytes = decode_percent(path.as_bytes())?;
    if bytes.contains(&0) {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

/// The `x-special/gnome-copied-files` body: the action, then the URIs, one
/// per line, without a trailing newline. Paths without a URI are skipped.
#[must_use]
pub fn format_gnome_copied_files(action: ClipboardAction, paths: &[PathBuf]) -> String {
    let mut out = match action {
        ClipboardAction::Copy => "copy",
        ClipboardAction::Cut => "cut",
    }
    .to_owned();
    for uri in paths.iter().filter_map(|path| path_to_uri(path)) {
        out.push('\n');
        out.push_str(&uri);
    }
    out
}

/// The `text/uri-list` body: each URI ends with CRLF (RFC 2483).
#[must_use]
pub fn format_uri_list(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .filter_map(|path| path_to_uri(path))
        .map(|uri| format!("{uri}\r\n"))
        .collect()
}

/// The `text/plain` form: the paths, one per line.
#[must_use]
pub fn format_plain_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Reads files out of a clipboard text: an `x-special/gnome-copied-files`
/// body, a `text/uri-list` body, or the text fallback of either (what a
/// file manager that offers only text writes).
///
/// Every line must be a `file:` URI or an absolute path, after an optional
/// first line `copy` or `cut`; a `#` line (a `text/uri-list` comment) and
/// empty lines are ignored. One line that is anything else (prose, a relative
/// path, a URI of another scheme) makes the whole text not a file list, so
/// ordinary text is never taken for files. Paths for which `exists` says no
/// are dropped; `None` when none is left.
#[must_use]
pub fn parse_clipboard_files(text: &str, exists: impl Fn(&Path) -> bool) -> Option<ClipboardFiles> {
    let mut lines = text
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .peekable();
    let action = match lines.peek().map(|line| line.trim()) {
        Some("copy") => {
            lines.next();
            ClipboardAction::Copy
        }
        Some("cut") => {
            lines.next();
            ClipboardAction::Cut
        }
        _ => ClipboardAction::Copy,
    };
    let mut paths = Vec::new();
    for line in lines {
        let line = line.trim_end();
        let path = if line
            .get(..5)
            .is_some_and(|s| s.eq_ignore_ascii_case("file:"))
        {
            uri_to_path(line)?
        } else if line.starts_with('/') {
            PathBuf::from(OsStr::from_bytes(line.as_bytes()))
        } else {
            return None;
        };
        if exists(&path) {
            paths.push(path);
        }
    }
    (!paths.is_empty()).then_some(ClipboardFiles { action, paths })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(list: &[&str]) -> Vec<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    fn all_exist(_: &Path) -> bool {
        true
    }

    #[test]
    fn a_path_becomes_an_encoded_file_uri() {
        assert_eq!(
            path_to_uri(Path::new("/a b/c#d%e/é.txt")).as_deref(),
            Some("file:///a%20b/c%23d%25e/%C3%A9.txt")
        );
        assert_eq!(path_to_uri(Path::new("relative/x")), None);
    }

    #[test]
    fn a_file_uri_decodes_back_to_the_path() {
        for original in ["/a b/c#d%e/é.txt", "/plain", "/x/ü ö/100%"] {
            let uri = path_to_uri(Path::new(original)).unwrap_or_default();
            assert_eq!(uri_to_path(&uri), Some(PathBuf::from(original)), "{uri}");
        }
        assert_eq!(
            uri_to_path("file://localhost/tmp/a%20b"),
            Some(PathBuf::from("/tmp/a b"))
        );
        assert_eq!(uri_to_path("FILE:///tmp/a"), Some(PathBuf::from("/tmp/a")));
        assert_eq!(
            uri_to_path("file:///tmp/a#b"),
            Some(PathBuf::from("/tmp/a#b"))
        );
    }

    #[test]
    fn uris_that_are_not_local_files_are_rejected() {
        assert_eq!(uri_to_path("http://example.com/a"), None);
        assert_eq!(uri_to_path("smb://host/share/a"), None);
        assert_eq!(uri_to_path("file://otherhost/tmp/a"), None);
        assert_eq!(uri_to_path("file:relative"), None);
        assert_eq!(uri_to_path("file:///tmp/%zz"), None);
        assert_eq!(uri_to_path("file:///tmp/a%00b"), None);
        assert_eq!(uri_to_path("/tmp/a"), None);
    }

    #[test]
    fn gnome_copied_files_has_the_action_then_one_uri_per_line() {
        let list = paths(&["/a b", "/c"]);
        assert_eq!(
            format_gnome_copied_files(ClipboardAction::Copy, &list),
            "copy\nfile:///a%20b\nfile:///c"
        );
        assert_eq!(
            format_gnome_copied_files(ClipboardAction::Cut, &list),
            "cut\nfile:///a%20b\nfile:///c"
        );
        assert_eq!(
            format_gnome_copied_files(ClipboardAction::Cut, &paths(&["rel", "/c"])),
            "cut\nfile:///c"
        );
    }

    #[test]
    fn a_uri_list_ends_every_line_with_crlf() {
        assert_eq!(
            format_uri_list(&paths(&["/a b", "/c"])),
            "file:///a%20b\r\nfile:///c\r\n"
        );
        assert_eq!(format_uri_list(&[]), "");
    }

    #[test]
    fn plain_text_is_the_paths_one_per_line() {
        assert_eq!(
            format_plain_paths(&paths(&["/x/a b", "/x/c"])),
            "/x/a b\n/x/c"
        );
        assert_eq!(format_plain_paths(&[]), "");
    }

    #[test]
    fn parsing_reads_what_formatting_writes() {
        let list = paths(&["/a b", "/c#d", "/100%", "/é"]);
        for action in [ClipboardAction::Copy, ClipboardAction::Cut] {
            let text = format_gnome_copied_files(action, &list);
            assert_eq!(
                parse_clipboard_files(&text, all_exist),
                Some(ClipboardFiles {
                    action,
                    paths: list.clone()
                })
            );
        }
        assert_eq!(
            parse_clipboard_files(&format_uri_list(&list), all_exist),
            Some(ClipboardFiles {
                action: ClipboardAction::Copy,
                paths: list
            })
        );
    }

    #[test]
    fn a_header_decides_copy_or_cut_and_no_header_means_copy() {
        let action = |text: &str| parse_clipboard_files(text, all_exist).map(|f| f.action);
        assert_eq!(action("cut\nfile:///a"), Some(ClipboardAction::Cut));
        assert_eq!(action("copy\nfile:///a"), Some(ClipboardAction::Copy));
        assert_eq!(action("file:///a"), Some(ClipboardAction::Copy));
        assert_eq!(action("/a"), Some(ClipboardAction::Copy));
        assert_eq!(action("cut\n/a"), Some(ClipboardAction::Cut));
    }

    #[test]
    fn line_endings_empty_lines_and_comments_are_tolerated() {
        let parsed =
            parse_clipboard_files("# a comment\r\nfile:///a\r\n\r\nfile:///b\r\n", all_exist);
        assert_eq!(parsed.map(|f| f.paths), Some(paths(&["/a", "/b"])));
        let lf = parse_clipboard_files("cut\n\nfile:///a\n\n", all_exist);
        assert_eq!(
            lf,
            Some(ClipboardFiles {
                action: ClipboardAction::Cut,
                paths: paths(&["/a"])
            })
        );
    }

    #[test]
    fn text_that_is_not_entirely_files_is_not_a_file_list() {
        for text in [
            "",
            "\n\n",
            "hello world",
            "relative/path",
            "cut",
            "cut\n",
            "file:///a\nsome prose",
            "http://example.com/a",
            "file:///a\nhttp://example.com/a",
            "file:///a\nrelative",
            "file://otherhost/a",
        ] {
            assert_eq!(parse_clipboard_files(text, all_exist), None, "{text:?}");
        }
    }

    #[test]
    fn paths_that_do_not_exist_are_dropped() {
        let only_a = |path: &Path| path == Path::new("/a");
        assert_eq!(
            parse_clipboard_files("file:///a\nfile:///gone\n/also/gone", only_a).map(|f| f.paths),
            Some(paths(&["/a"]))
        );
        assert_eq!(parse_clipboard_files("file:///gone\n/gone", only_a), None);
    }
}
