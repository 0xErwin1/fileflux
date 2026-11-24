//! The search matcher and the content search, against scratch trees in a
//! fresh temporary directory. Nothing here reads outside its scratch.

use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use fileflux::core::child;
use fileflux::core::search::{SearchOptions, SearchSummary, search};
use fileflux::core::search_match::{Matcher, QueryOptions};

type TestResult = Result<(), Box<dyn Error>>;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Result<Self, Box<dyn Error>> {
        let dir =
            std::env::temp_dir().join(format!("fileflux-content-{name}-{}", std::process::id()));
        drop(fs::remove_dir_all(&dir));
        fs::create_dir_all(dir.join("root"))?;
        Ok(Self(fs::canonicalize(dir)?))
    }

    fn root(&self) -> PathBuf {
        self.0.join("root")
    }

    fn write(&self, relative: &str, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
        let path = self.root().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, bytes)?;
        Ok(())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        drop(fs::remove_dir_all(&self.0));
    }
}

fn opts(match_case: bool, regex: bool, contents: bool) -> QueryOptions {
    QueryOptions {
        match_case,
        regex,
        contents,
    }
}

fn matcher(query: &str, options: QueryOptions) -> Result<Matcher, Box<dyn Error>> {
    Ok(Matcher::new(query, options)?)
}

fn find(root: &Path, query: &str, options: QueryOptions) -> (BTreeSet<String>, SearchSummary) {
    let mut search_options = SearchOptions::new(query);
    search_options.query_options = options;
    find_with(root, &search_options)
}

fn find_with(root: &Path, options: &SearchOptions) -> (BTreeSet<String>, SearchSummary) {
    let cancel = AtomicBool::new(false);
    let mut names = BTreeSet::new();
    let summary = search(root, options, &cancel, |batch| {
        for entry in batch {
            names.insert(entry.name.to_string_lossy().into_owned());
        }
    });
    (names, summary)
}

#[test]
fn defaults_are_names_only_case_insensitive_plain() {
    assert_eq!(QueryOptions::default(), opts(false, false, false));
}

#[test]
fn substring_ignores_case_unless_match_case() -> TestResult {
    let loose = matcher("Report", opts(false, false, false))?;
    assert!(loose.is_match("my-REPORT.txt"));
    let exact = matcher("Report", opts(true, false, false))?;
    assert!(!exact.is_match("my-REPORT.txt"));
    assert!(exact.is_match("my-Report.txt"));
    Ok(())
}

#[test]
fn substring_folds_unicode_when_case_is_ignored() -> TestResult {
    let loose = matcher("ÁRBOL", opts(false, false, false))?;
    assert!(loose.is_match("el árbol.txt"));
    let exact = matcher("ÁRBOL", opts(true, false, false))?;
    assert!(!exact.is_match("el árbol.txt"));
    Ok(())
}

#[test]
fn regex_honours_the_case_flag() -> TestResult {
    let loose = matcher(r"^re\w+t$", opts(false, true, false))?;
    assert!(loose.is_match("REPORT"));
    let exact = matcher(r"^re\w+t$", opts(true, true, false))?;
    assert!(!exact.is_match("REPORT"));
    assert!(exact.is_match("report"));
    Ok(())
}

#[test]
fn an_invalid_regex_is_an_error_with_text() {
    let error = Matcher::new("(unclosed", opts(false, true, false)).err();
    assert!(error.is_some_and(|error| {
        let text = error.to_string();
        !text.is_empty() && !text.contains('\n')
    }));
    // The same text is a valid plain substring.
    assert!(Matcher::new("(unclosed", opts(false, false, false)).is_ok());
}

#[test]
fn an_empty_query_matches_nothing() -> TestResult {
    for regex in [false, true] {
        let matcher = matcher("", opts(false, regex, false))?;
        assert!(!matcher.is_match("anything"));
        assert!(!matcher.is_match(""));
    }
    Ok(())
}

#[test]
fn a_content_match_in_a_middle_line_finds_the_file() -> TestResult {
    let scratch = Scratch::new("middle")?;
    scratch.write("a.txt", b"one\ntwo NEEDLE here\nthree\n")?;
    scratch.write("b.txt", b"nothing\n")?;
    let root = scratch.root();
    let (names, summary) = find(&root, "needle", opts(false, false, true));
    assert_eq!(names, BTreeSet::from(["a.txt".to_owned()]));
    assert_eq!(summary.found, 1);
    // Without the option, content is not read.
    let (names, _) = find(&root, "needle", opts(false, false, false));
    assert!(names.is_empty());
    // Match case applies to content.
    let (names, _) = find(&root, "needle", opts(true, false, true));
    assert!(names.is_empty());
    Ok(())
}

#[test]
fn a_name_match_still_counts_with_contents_on() -> TestResult {
    let scratch = Scratch::new("name-or")?;
    scratch.write("needle.txt", b"x\n")?;
    scratch.write("other.txt", b"has needle\n")?;
    scratch.write("plain.txt", b"x\n")?;
    let (names, _) = find(&scratch.root(), "needle", opts(false, false, true));
    assert_eq!(
        names,
        BTreeSet::from(["needle.txt".to_owned(), "other.txt".to_owned()])
    );
    Ok(())
}

#[test]
fn regex_content_is_matched_per_line_with_crlf_stripped() -> TestResult {
    let scratch = Scratch::new("crlf")?;
    scratch.write("win.txt", b"first\r\nsecond line\r\nthird\r\n")?;
    scratch.write("multi.txt", b"first\nsecond\n")?;
    let root = scratch.root();
    let (names, _) = find(&root, r"line$", opts(false, true, true));
    assert_eq!(names, BTreeSet::from(["win.txt".to_owned()]));
    // A pattern cannot span lines.
    let (names, _) = find(&root, r"first\nsecond", opts(false, true, true));
    assert!(names.is_empty());
    Ok(())
}

#[test]
fn invalid_utf8_is_read_lossily() -> TestResult {
    let scratch = Scratch::new("utf8")?;
    scratch.write("latin.txt", b"caf\xe9 au lait\nNEEDLE\n")?;
    scratch.write("before.txt", b"\xff\xfe needle\n")?;
    let (names, _) = find(&scratch.root(), "needle", opts(false, false, true));
    assert_eq!(
        names,
        BTreeSet::from(["latin.txt".to_owned(), "before.txt".to_owned()])
    );
    Ok(())
}

#[test]
fn binary_files_are_skipped() -> TestResult {
    let scratch = Scratch::new("binary")?;
    scratch.write("blob.bin", b"needle\0needle")?;
    // A NUL past the first 8 KiB does not make a file binary.
    let mut late = vec![b'a'; 9000];
    late.extend_from_slice(b"\nneedle\n\0");
    scratch.write("late.txt", &late)?;
    let (names, _) = find(&scratch.root(), "needle", opts(false, false, true));
    assert_eq!(names, BTreeSet::from(["late.txt".to_owned()]));
    Ok(())
}

#[test]
fn files_over_ten_mebibytes_are_skipped() -> TestResult {
    let scratch = Scratch::new("big")?;
    let limit = 10 * 1024 * 1024;
    let mut at_limit = b"needle\n".to_vec();
    at_limit.resize(limit, b'a');
    scratch.write("at_limit.txt", &at_limit)?;
    let mut over = b"needle\n".to_vec();
    over.resize(limit + 1, b'a');
    scratch.write("over.txt", &over)?;
    let (names, _) = find(&scratch.root(), "needle", opts(false, false, true));
    assert_eq!(names, BTreeSet::from(["at_limit.txt".to_owned()]));
    Ok(())
}

#[test]
fn symlinked_files_are_not_read() -> TestResult {
    let scratch = Scratch::new("link")?;
    let outside = scratch.0.join("outside.txt");
    fs::write(&outside, b"needle\n")?;
    let root = scratch.root();
    symlink(&outside, root.join("link.txt"))?;
    scratch.write("real.txt", b"needle\n")?;
    let (names, _) = find(&root, "needle", opts(false, false, true));
    assert_eq!(names, BTreeSet::from(["real.txt".to_owned()]));
    Ok(())
}

#[test]
fn a_fifo_is_not_opened() -> TestResult {
    let scratch = Scratch::new("fifo")?;
    let root = scratch.root();
    let status = child::command("mkfifo").arg(root.join("pipe")).status()?;
    assert!(status.success());
    scratch.write("real.txt", b"needle\n")?;
    let (names, _) = find(&root, "needle", opts(false, false, true));
    assert_eq!(names, BTreeSet::from(["real.txt".to_owned()]));
    Ok(())
}

#[test]
fn cancel_stops_the_content_walk() -> TestResult {
    let scratch = Scratch::new("cancel")?;
    for index in 0..50 {
        scratch.write(&format!("f{index}.txt"), b"needle\n")?;
    }
    let mut options = SearchOptions::new("needle");
    options.query_options = opts(false, false, true);
    options.batch_size = 1;
    let cancel = AtomicBool::new(false);
    let mut seen = 0;
    let summary = search(&scratch.root(), &options, &cancel, |batch| {
        seen += batch.len();
        cancel.store(true, Ordering::Relaxed);
    });
    assert!(summary.cancelled);
    assert!(seen < 50, "stopped early, saw {seen}");
    Ok(())
}

#[test]
fn a_cancelled_walk_reads_nothing_more() -> TestResult {
    let scratch = Scratch::new("precancel")?;
    scratch.write("a.txt", b"needle\n")?;
    let mut options = SearchOptions::new("needle");
    options.query_options = opts(false, false, true);
    let cancel = AtomicBool::new(true);
    let mut seen = 0;
    let summary = search(&scratch.root(), &options, &cancel, |batch| {
        seen += batch.len();
    });
    assert!(summary.cancelled);
    assert_eq!(seen, 0);
    Ok(())
}

#[test]
fn the_cap_holds_for_content_matches() -> TestResult {
    let scratch = Scratch::new("cap")?;
    for index in 0..20 {
        scratch.write(&format!("f{index}.txt"), b"needle\n")?;
    }
    let mut options = SearchOptions::new("needle");
    options.query_options = opts(false, false, true);
    options.cap = 5;
    let (names, summary) = find_with(&scratch.root(), &options);
    assert_eq!(names.len(), 5);
    assert_eq!(summary.found, 5);
    assert!(summary.capped);
    Ok(())
}

#[test]
fn name_search_with_a_regex_matches_the_display_name() -> TestResult {
    let scratch = Scratch::new("regex-name")?;
    scratch.write("img_001.png", b"x")?;
    scratch.write("img_abc.png", b"x")?;
    let (names, _) = find(&scratch.root(), r"^img_\d+\.png$", opts(false, true, false));
    assert_eq!(names, BTreeSet::from(["img_001.png".to_owned()]));
    Ok(())
}

#[test]
fn an_invalid_regex_walks_nothing() -> TestResult {
    let scratch = Scratch::new("badregex")?;
    scratch.write("a.txt", b"(x\n")?;
    let (names, summary) = find(&scratch.root(), "(x", opts(false, true, true));
    assert!(names.is_empty());
    assert_eq!(summary, SearchSummary::default());
    Ok(())
}
