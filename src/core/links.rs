//! Symbolic links: "Make Link", "Paste as link" and a drop with Ctrl+Shift.
//!
//! A link is created with `symlink(2)`, which fails instead of replacing
//! anything that is already there; the search for a free name therefore
//! tries to create each candidate rather than testing it first, so another
//! process taking a name in between costs one more try and never an
//! overwrite. Targets are absolute paths (the source as it was shown, not
//! resolved through its own links), so a link keeps working when it is
//! moved to another folder.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use super::ops::{MAX_NAME_BYTES, fit_name, numbered_name};

/// How a new link is named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Naming {
    /// "Link to <name>": the link made next to its target.
    LinkTo,
    /// The source's own name: a link pasted or dropped into another folder.
    Source,
}

/// One link that was created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Made {
    /// What the link points at.
    pub source: PathBuf,
    /// The link itself.
    pub link: PathBuf,
}

/// What a [`make_links`] run did.
#[derive(Debug, Default)]
pub struct Outcome {
    pub made: Vec<Made>,
    /// The sources that got no link, with why.
    pub failed: Vec<(PathBuf, io::Error)>,
    /// Cancelled between items; what was made stays made.
    pub cancelled: bool,
}

/// The name a link to `name` starts from, before any number: "Link to
/// <name>" or `name` itself. Cut to fit a file name, keeping an extension.
pub fn base_name(name: &OsStr, naming: Naming) -> OsString {
    match naming {
        Naming::Source => name.to_owned(),
        Naming::LinkTo => {
            let mut full = OsString::from("Link to ");
            full.push(name);
            fit_name(&full, "", false, MAX_NAME_BYTES)
        }
    }
}

/// How many numbered names are tried before giving up.
const MAX_ATTEMPTS: u32 = 10_000;

/// Creates `target`'s link inside `dir`, under the first free name from
/// `base`. `whole_name`: the number goes after the whole name ("Link to
/// a.txt (2)") instead of before the extension ("a (2).txt").
fn create_one(target: &Path, dir: &Path, base: &OsStr, whole_name: bool) -> io::Result<PathBuf> {
    for n in 1..=MAX_ATTEMPTS {
        let name = if n == 1 {
            base.to_owned()
        } else {
            numbered_name(base, n, whole_name)
        };
        let link = dir.join(name);
        match symlink(target, &link) {
            Ok(()) => return Ok(link),
            // Taken, even by a dangling link or by someone a moment ago.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free numbered name",
    ))
}

fn link_one(source: &Path, into: Option<&Path>, naming: Naming) -> io::Result<PathBuf> {
    let meta = fs::symlink_metadata(source)?;
    let name = source
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "it has no name to link"))?;
    let dir = match into {
        Some(dir) => dir,
        None => source
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "it has no folder"))?,
    };
    // The link keeps pointing at the source when it moves elsewhere.
    let target = std::path::absolute(source)?;
    let whole_name = naming == Naming::LinkTo || meta.is_dir();
    create_one(&target, dir, &base_name(name, naming), whole_name)
}

/// Creates a link to each of `sources`: next to the source when `into` is
/// `None`, else inside `into`. A taken name gets " (2)", " (3)", ... after
/// it; nothing is ever replaced. `progress` hears the index and path of each
/// source before it is linked; `cancel` is polled between sources.
pub fn make_links(
    sources: &[PathBuf],
    into: Option<&Path>,
    naming: Naming,
    mut progress: impl FnMut(usize, &Path),
    cancel: &AtomicBool,
) -> Outcome {
    let mut outcome = Outcome::default();
    for (index, source) in sources.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            outcome.cancelled = true;
            break;
        }
        progress(index, source);
        match link_one(source, into, naming) {
            Ok(link) => outcome.made.push(Made {
                source: source.clone(),
                link,
            }),
            Err(error) => outcome.failed.push((source.clone(), error)),
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt;
    use std::sync::atomic::AtomicU64;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> io::Result<Scratch> {
            let dir = std::env::temp_dir().join(format!(
                "fileflux-links-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir)?;
            Ok(Scratch(dir))
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn run(sources: &[PathBuf], into: Option<&Path>, naming: Naming) -> Outcome {
        make_links(sources, into, naming, |_, _| {}, &AtomicBool::new(false))
    }

    #[test]
    fn the_base_name_says_link_to_or_keeps_the_name() {
        assert_eq!(
            base_name(OsStr::new("a.txt"), Naming::LinkTo),
            "Link to a.txt"
        );
        assert_eq!(base_name(OsStr::new("a.txt"), Naming::Source), "a.txt");
    }

    #[test]
    fn a_long_name_is_cut_to_fit_and_keeps_its_extension() {
        let long = format!("{}.txt", "x".repeat(255));
        let name = base_name(OsStr::new(&long), Naming::LinkTo);
        assert!(name.len() <= 255);
        assert!(name.as_bytes().starts_with(b"Link to x"));
        assert!(name.as_bytes().ends_with(b".txt"));
    }

    #[test]
    fn make_link_creates_link_to_name_next_to_the_target() -> TestResult {
        let s = Scratch::new()?;
        let target = s.path("report.pdf");
        fs::write(&target, b"data")?;
        let out = run(std::slice::from_ref(&target), None, Naming::LinkTo);
        let link = s.path("Link to report.pdf");
        assert_eq!(
            out.made,
            vec![Made {
                source: target.clone(),
                link: link.clone()
            }]
        );
        assert!(out.failed.is_empty() && !out.cancelled);
        assert_eq!(fs::read_link(&link)?, target);
        assert!(fs::symlink_metadata(&link)?.file_type().is_symlink());
        Ok(())
    }

    #[test]
    fn a_taken_name_gets_a_number_after_the_whole_name() -> TestResult {
        let s = Scratch::new()?;
        let target = s.path("report.pdf");
        fs::write(&target, b"data")?;
        for expected in [
            "Link to report.pdf",
            "Link to report.pdf (2)",
            "Link to report.pdf (3)",
        ] {
            let out = run(std::slice::from_ref(&target), None, Naming::LinkTo);
            assert_eq!(
                out.made.first().map(|m| m.link.clone()),
                Some(s.path(expected))
            );
        }
        Ok(())
    }

    #[test]
    fn a_dangling_link_holds_its_name_too() -> TestResult {
        let s = Scratch::new()?;
        let target = s.path("a");
        fs::write(&target, b"")?;
        symlink(s.path("missing"), s.path("Link to a"))?;
        let out = run(std::slice::from_ref(&target), None, Naming::LinkTo);
        assert_eq!(
            out.made.first().map(|m| m.link.clone()),
            Some(s.path("Link to a (2)"))
        );
        assert_eq!(fs::read_link(s.path("Link to a"))?, s.path("missing"));
        Ok(())
    }

    #[test]
    fn links_into_a_folder_take_the_source_name_and_never_overwrite() -> TestResult {
        let s = Scratch::new()?;
        let source = s.path("one/data.txt");
        fs::create_dir_all(s.path("one"))?;
        fs::create_dir_all(s.path("dest"))?;
        fs::write(&source, b"source")?;
        fs::write(s.path("dest/data.txt"), b"mine")?;
        let out = run(
            std::slice::from_ref(&source),
            Some(&s.path("dest")),
            Naming::Source,
        );
        assert_eq!(
            out.made.first().map(|m| m.link.clone()),
            Some(s.path("dest/data (2).txt"))
        );
        assert_eq!(fs::read(s.path("dest/data.txt"))?, b"mine");
        assert_eq!(fs::read_link(s.path("dest/data (2).txt"))?, source);
        Ok(())
    }

    #[test]
    fn several_sources_each_get_a_link_and_a_failure_does_not_stop_the_rest() -> TestResult {
        let s = Scratch::new()?;
        let (a, b) = (s.path("a"), s.path("b"));
        fs::write(&a, b"")?;
        fs::write(&b, b"")?;
        let gone = s.path("gone");
        let seen = std::cell::RefCell::new(Vec::new());
        let out = make_links(
            &[a.clone(), gone.clone(), b.clone()],
            None,
            Naming::LinkTo,
            |index, path| seen.borrow_mut().push((index, path.to_path_buf())),
            &AtomicBool::new(false),
        );
        assert_eq!(out.made.len(), 2);
        assert_eq!(out.failed.len(), 1);
        assert_eq!(out.failed.first().map(|f| f.0.clone()), Some(gone.clone()));
        assert_eq!(seen.borrow().len(), 3);
        assert!(!s.path("Link to gone").exists());
        Ok(())
    }

    #[test]
    fn a_directory_is_linked_by_its_absolute_path() -> TestResult {
        let s = Scratch::new()?;
        let dir = s.path("Photos");
        fs::create_dir_all(&dir)?;
        let out = run(std::slice::from_ref(&dir), None, Naming::LinkTo);
        assert_eq!(out.made.len(), 1);
        let link = s.path("Link to Photos");
        assert!(fs::read_link(&link)?.is_absolute());
        assert!(link.is_dir());
        Ok(())
    }

    #[test]
    fn a_link_to_a_link_points_at_the_link_not_through_it() -> TestResult {
        let s = Scratch::new()?;
        fs::write(s.path("real"), b"")?;
        symlink(s.path("real"), s.path("first"))?;
        run(&[s.path("first")], None, Naming::LinkTo);
        assert_eq!(fs::read_link(s.path("Link to first"))?, s.path("first"));
        Ok(())
    }

    #[test]
    fn cancel_stops_before_the_next_source() -> TestResult {
        let s = Scratch::new()?;
        fs::write(s.path("a"), b"")?;
        let cancel = AtomicBool::new(true);
        let out = make_links(&[s.path("a")], None, Naming::LinkTo, |_, _| {}, &cancel);
        assert!(out.cancelled && out.made.is_empty());
        assert!(!s.path("Link to a").exists());
        Ok(())
    }

    #[test]
    fn the_filesystem_root_has_no_name_to_link() {
        let out = run(
            &[PathBuf::from("/")],
            Some(Path::new("/nonexistent-fileflux")),
            Naming::Source,
        );
        assert!(out.made.is_empty());
        assert_eq!(out.failed.len(), 1);
    }
}
