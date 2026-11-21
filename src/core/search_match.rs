//! What a recursive search matches: the query options, the matcher built
//! once per query, and the content scan of one file.
//!
//! Names match on their display name; with contents on, a file also matches
//! when any one of its lines does (a pattern never spans lines). The scan
//! reads only regular files of at most [`MAX_CONTENT_BYTES`], never follows a
//! symlink, skips binary files (a NUL in the first [`BINARY_PROBE`] bytes),
//! reads line by line as lossy UTF-8 and stops at the first matching line.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufRead as _, BufReader, Cursor, Read as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use regex::{Regex, RegexBuilder};

/// The largest file whose content is searched.
pub const MAX_CONTENT_BYTES: u64 = 10 * 1024 * 1024;
/// How much of a file is looked at for a NUL byte.
pub const BINARY_PROBE: usize = 8 * 1024;
/// Lines read between two looks at the cancel flag.
pub const CANCEL_EVERY_LINES: usize = 1024;

/// How a query is read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryOptions {
    /// Letter case matters.
    pub match_case: bool,
    /// The query is a regular expression, not plain text.
    pub regex: bool,
    /// Files also match by the lines they hold.
    pub contents: bool,
}

/// The query is not a valid regular expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatternError(String);

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PatternError {}

#[derive(Debug)]
enum Kind {
    /// Matches nothing: the empty query.
    Nothing,
    /// A substring; lowercase already when case is ignored.
    Plain {
        needle: String,
        fold: bool,
    },
    Pattern(Regex),
}

/// A query compiled once, tested against many strings.
#[derive(Debug)]
pub struct Matcher(Kind);

impl Matcher {
    /// Compiles `query`. An empty query matches nothing; an invalid regular
    /// expression is an error carrying the parser's own message.
    pub fn new(query: &str, options: QueryOptions) -> Result<Self, PatternError> {
        if query.is_empty() {
            return Ok(Self(Kind::Nothing));
        }
        if options.regex {
            return RegexBuilder::new(query)
                .case_insensitive(!options.match_case)
                .build()
                .map(|regex| Self(Kind::Pattern(regex)))
                .map_err(|error| PatternError(one_line(&error.to_string())));
        }
        let fold = !options.match_case;
        let needle = if fold {
            query.to_lowercase()
        } else {
            query.to_owned()
        };
        Ok(Self(Kind::Plain { needle, fold }))
    }

    /// A matcher that matches nothing.
    #[must_use]
    pub const fn nothing() -> Self {
        Self(Kind::Nothing)
    }

    /// Whether this matches nothing at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        matches!(self.0, Kind::Nothing)
    }

    /// Whether `text` contains a match.
    #[must_use]
    pub fn is_match(&self, text: &str) -> bool {
        match &self.0 {
            Kind::Nothing => false,
            Kind::Plain { needle, fold: true } => text.to_lowercase().contains(needle.as_str()),
            Kind::Plain {
                needle,
                fold: false,
            } => text.contains(needle.as_str()),
            Kind::Pattern(regex) => regex.is_match(text),
        }
    }
}

/// How a content scan ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scan {
    /// A line matched.
    Found,
    /// Nothing matched, or the file was not eligible or not readable.
    NotFound,
    /// The cancel flag stopped the read.
    Cancelled,
}

/// Whether a line of the regular file at `path` matches. Eligibility is
/// decided on the opened handle (not followed through a symlink, never a
/// pipe or device, at most [`MAX_CONTENT_BYTES`], no NUL early on), so a
/// file swapped for something else after listing is not read.
pub fn scan_file(path: &Path, matcher: &Matcher, cancel: &AtomicBool) -> Scan {
    let Some(file) = open_regular(path) else {
        return Scan::NotFound;
    };
    let mut file = file;
    let mut head = Vec::with_capacity(BINARY_PROBE);
    match (&mut file).take(BINARY_PROBE as u64).read_to_end(&mut head) {
        Ok(_) if !head.contains(&0) => {}
        _ => return Scan::NotFound,
    }
    let mut reader = BufReader::with_capacity(BINARY_PROBE, Cursor::new(head).chain(file));
    let mut line = Vec::new();
    let mut lines = 0usize;
    loop {
        lines += 1;
        if lines.is_multiple_of(CANCEL_EVERY_LINES) && cancel.load(Ordering::Relaxed) {
            return Scan::Cancelled;
        }
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return Scan::NotFound,
            Ok(_) => {}
        }
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        if matcher.is_match(&String::from_utf8_lossy(&line)) {
            return Scan::Found;
        }
    }
}

fn open_regular(path: &Path) -> Option<File> {
    // O_NOFOLLOW: a symlink is not opened; O_NONBLOCK: a fifo swapped in
    // after listing cannot hang the open.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(
            rustix::fs::OFlags::NOFOLLOW.bits() as i32 | rustix::fs::OFlags::NONBLOCK.bits() as i32,
        )
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    (meta.is_file() && meta.len() <= MAX_CONTENT_BYTES).then_some(file)
}

/// The parser's message without its multi-line diagram: its last line, minus
/// the "error: " lead.
fn one_line(message: &str) -> String {
    let last = message
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(message);
    last.trim().trim_start_matches("error: ").to_owned()
}
