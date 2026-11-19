//! The recursive name search walker, against scratch trees in a fresh
//! temporary directory. Nothing here reads outside its scratch.

use std::collections::BTreeSet;
use std::error::Error;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use fileflux::core::search::{SearchOptions, SearchSummary, search, search_with_device};
use fileflux::core::{Entry, EntryKind};

type TestResult = Result<(), Box<dyn Error>>;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Result<Self, Box<dyn Error>> {
        let dir =
            std::env::temp_dir().join(format!("fileflux-search-{name}-{}", std::process::id()));
        drop(fs::remove_dir_all(&dir));
        fs::create_dir_all(&dir)?;
        Ok(Self(fs::canonicalize(dir)?))
    }

    fn root(&self) -> PathBuf {
        self.0.join("root")
    }

    fn file(&self, relative: &str) -> Result<PathBuf, Box<dyn Error>> {
        let path = self.root().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, b"x")?;
        Ok(path)
    }

    fn dir(&self, relative: &str) -> Result<PathBuf, Box<dyn Error>> {
        let path = self.root().join(relative);
        fs::create_dir_all(&path)?;
        Ok(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Restore any permission a test took away so the cleanup can run.
        if let Ok(unreadable) = fs::metadata(self.0.join("root/locked")) {
            let mut mode = unreadable.permissions();
            mode.set_mode(0o755);
            drop(fs::set_permissions(self.0.join("root/locked"), mode));
        }
        drop(fs::remove_dir_all(&self.0));
    }
}

/// A path with its size and modification time.
type TreeFact = (PathBuf, u64, i64);

struct Run {
    entries: Vec<Entry>,
    batches: Vec<usize>,
    summary: SearchSummary,
}

impl Run {
    fn relative(&self, root: &Path) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| {
                entry
                    .path
                    .strip_prefix(root)
                    .unwrap_or(&entry.path)
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    fn names(&self, root: &Path) -> BTreeSet<String> {
        self.relative(root).into_iter().collect()
    }
}

fn run(root: &Path, options: &SearchOptions) -> Run {
    let cancel = AtomicBool::new(false);
    let mut entries = Vec::new();
    let mut batches = Vec::new();
    let summary = search(root, options, &cancel, |batch| {
        batches.push(batch.len());
        entries.extend(batch);
    });
    Run {
        entries,
        batches,
        summary,
    }
}

#[test]
fn shallow_matches_come_before_deep_ones() -> TestResult {
    let scratch = Scratch::new("depth")?;
    scratch.file("a/b/c/hit_deep.txt")?;
    scratch.file("a/b/hit_mid.txt")?;
    scratch.file("z/hit_shallow.txt")?;
    scratch.file("hit_top.txt")?;
    scratch.file("a/hit_one.txt")?;
    // A deep branch under the folder read last must not be walked before
    // the shallower folder read first.
    scratch.file("z/y/hit_branch.txt")?;
    let root = scratch.root();
    let found = run(&root, &SearchOptions::new("hit"));
    let depths: Vec<usize> = found
        .entries
        .iter()
        .map(|entry| {
            entry
                .path
                .strip_prefix(&root)
                .map_or(0, |relative| relative.components().count())
        })
        .collect();
    assert_eq!(found.entries.len(), 6, "every match is found");
    assert!(
        depths.is_sorted(),
        "breadth-first: depths never decrease, got {depths:?}"
    );
    assert_eq!(depths.first(), Some(&1));
    assert_eq!(depths.last(), Some(&4));
    assert_eq!(found.summary.found, 6);
    assert!(!found.summary.capped && !found.summary.cancelled);
    Ok(())
}

#[test]
fn matching_is_a_case_insensitive_substring_of_the_display_name() -> TestResult {
    let scratch = Scratch::new("match")?;
    scratch.file("Report-FINAL.txt")?;
    scratch.file("notes/final_draft.md")?;
    scratch.file("other.txt")?;
    scratch.dir("sub/Finalists")?;
    let root = scratch.root();
    let found = run(&root, &SearchOptions::new("fInAl"));
    assert_eq!(
        found.names(&root),
        BTreeSet::from([
            "Report-FINAL.txt".to_owned(),
            "notes/final_draft.md".to_owned(),
            "sub/Finalists".to_owned(),
        ])
    );
    let folder = found
        .entries
        .iter()
        .find(|entry| entry.name == OsStr::new("Finalists"))
        .ok_or("the folder match is listed")?;
    assert_eq!(folder.kind, EntryKind::Directory);
    let file = found
        .entries
        .iter()
        .find(|entry| entry.name == OsStr::new("final_draft.md"))
        .ok_or("the file match is listed")?;
    assert_eq!(file.origin.as_deref(), Some(root.join("notes").as_path()));
    assert_eq!(file.size, Some(1));
    assert!(file.modified.is_some());
    // The path part is not matched: a folder's name does not match its files.
    assert!(!found.names(&root).contains("sub/Finalists/x"));
    Ok(())
}

#[test]
fn an_empty_query_finds_nothing() -> TestResult {
    let scratch = Scratch::new("empty")?;
    scratch.file("a.txt")?;
    let found = run(&scratch.root(), &SearchOptions::new(""));
    assert!(found.entries.is_empty());
    assert_eq!(found.summary, SearchSummary::default());
    Ok(())
}

#[test]
fn symlinked_directories_are_never_followed() -> TestResult {
    let scratch = Scratch::new("loop")?;
    scratch.file("sub/target_file.txt")?;
    let root = scratch.root();
    // A loop back to the root, a loop to the parent, and a link to a folder
    // outside the tree: none is entered.
    symlink(&root, root.join("sub/target_loop"))?;
    symlink("..", root.join("sub/target_up"))?;
    let outside = scratch.0.join("outside");
    fs::create_dir_all(&outside)?;
    fs::write(outside.join("target_outside.txt"), b"x")?;
    symlink(&outside, root.join("target_link"))?;
    let found = run(&root, &SearchOptions::new("target"));
    assert_eq!(
        found.names(&root),
        BTreeSet::from([
            "sub/target_file.txt".to_owned(),
            // The links themselves are results (they match by name)…
            "sub/target_loop".to_owned(),
            "sub/target_up".to_owned(),
            "target_link".to_owned(),
        ]),
        "…but nothing under them is"
    );
    let link = found
        .entries
        .iter()
        .find(|entry| entry.name == OsStr::new("target_link"))
        .ok_or("the link is listed")?;
    assert!(link.is_symlink);
    assert_eq!(link.kind, EntryKind::Directory);
    assert_eq!(found.summary.found, 4);
    Ok(())
}

fn fake_device(path: &Path, meta: &fs::Metadata) -> u64 {
    if path.file_name() == Some(OsStr::new("mounted")) {
        meta.dev() + 1
    } else {
        meta.dev()
    }
}

#[test]
fn another_device_is_not_entered_unless_asked() -> TestResult {
    let scratch = Scratch::new("device")?;
    scratch.file("mounted/inside_hit.txt")?;
    scratch.file("plain/inside_hit.txt")?;
    scratch.dir("mounted_hit")?;
    let root = scratch.root();
    let cancel = AtomicBool::new(false);

    let mut stay = Vec::new();
    let mut options = SearchOptions::new("hit");
    search_with_device(&root, &options, fake_device, &cancel, |batch| {
        stay.extend(batch);
    });
    let stay: BTreeSet<String> = stay
        .iter()
        .filter_map(|entry| entry.path.strip_prefix(&root).ok())
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        stay,
        BTreeSet::from(["plain/inside_hit.txt".to_owned(), "mounted_hit".to_owned()]),
        "the other device's folder is skipped"
    );

    // The mount point itself still matches by name.
    scratch.dir("mounted_hit_dir")?;
    options.cross_devices = true;
    let mut across = Vec::new();
    search_with_device(&root, &options, fake_device, &cancel, |batch| {
        across.extend(batch);
    });
    assert!(
        across
            .iter()
            .any(|entry| entry.path == root.join("mounted/inside_hit.txt")),
        "with cross_devices on, the other device is searched"
    );
    // A real tree on one device is searched whole by default.
    let found = run(&root, &SearchOptions::new("inside_hit"));
    assert_eq!(found.entries.len(), 2);
    Ok(())
}

#[test]
fn the_mount_point_itself_matches_by_name() -> TestResult {
    let scratch = Scratch::new("mountpoint")?;
    scratch.file("mounted/inside.txt")?;
    let root = scratch.root();
    let cancel = AtomicBool::new(false);
    let mut seen = Vec::new();
    search_with_device(
        &root,
        &SearchOptions::new("mount"),
        fake_device,
        &cancel,
        |batch| seen.extend(batch),
    );
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen.first().map(|entry| entry.path.clone()),
        Some(root.join("mounted"))
    );
    Ok(())
}

#[test]
fn hidden_entries_follow_the_listing_rule() -> TestResult {
    let scratch = Scratch::new("hidden")?;
    scratch.file("visible_match.txt")?;
    scratch.file(".dot_match.txt")?;
    scratch.file(".git/inner_match.txt")?;
    scratch.file("docs/listed_match.txt")?;
    scratch.file("docs/shown_match.txt")?;
    scratch.file("docs/.hidden")?;
    fs::write(
        scratch.root().join("docs/.hidden"),
        "listed_match.txt\r\nsecret_dir\n",
    )?;
    scratch.file("secret_dir/deep_match.txt")?;
    scratch.file("docs/secret_dir/deep_match.txt")?;
    let root = scratch.root();

    let hiding = run(&root, &SearchOptions::new("match"));
    assert_eq!(
        hiding.names(&root),
        BTreeSet::from([
            "visible_match.txt".to_owned(),
            "docs/shown_match.txt".to_owned(),
            // `.hidden` lists names of its own folder only.
            "secret_dir/deep_match.txt".to_owned(),
        ]),
        "dotfiles, dot folders' contents and `.hidden` names are skipped"
    );

    let mut options = SearchOptions::new("match");
    options.hide_hidden = false;
    let showing = run(&root, &options);
    assert_eq!(
        showing.names(&root),
        BTreeSet::from([
            "visible_match.txt".to_owned(),
            ".dot_match.txt".to_owned(),
            ".git/inner_match.txt".to_owned(),
            "docs/listed_match.txt".to_owned(),
            "docs/shown_match.txt".to_owned(),
            "docs/secret_dir/deep_match.txt".to_owned(),
            "secret_dir/deep_match.txt".to_owned(),
        ]),
        "with hidden files shown everything is searched"
    );
    Ok(())
}

#[test]
fn a_cancel_stops_the_walk_within_one_entry() -> TestResult {
    let scratch = Scratch::new("cancel")?;
    for folder in 0..20 {
        for file in 0..50 {
            scratch.file(&format!("d{folder:02}/hit_{file:02}.txt"))?;
        }
    }
    let root = scratch.root();
    let mut options = SearchOptions::new("hit");
    options.batch_size = 10;

    let cancel = AtomicBool::new(false);
    let mut batches = Vec::new();
    let summary = search(&root, &options, &cancel, |batch| {
        batches.push(batch.len());
        cancel.store(true, Ordering::Relaxed);
    });
    assert!(summary.cancelled);
    assert_eq!(batches, vec![10], "no entry is looked at after the flag");
    assert_eq!(summary.found, 10);

    // A walk cancelled before it starts looks at nothing.
    let mut none = 0;
    let summary = search(&root, &options, &cancel, |batch| none += batch.len());
    assert!(summary.cancelled);
    assert_eq!((summary.found, none), (0, 0));
    Ok(())
}

#[test]
fn the_cap_stops_the_walk_and_says_so() -> TestResult {
    let scratch = Scratch::new("cap")?;
    for index in 0..12 {
        scratch.file(&format!("d{}/hit_{index}.txt", index % 3))?;
    }
    let root = scratch.root();
    let mut options = SearchOptions::new("hit");
    options.cap = 5;
    let found = run(&root, &options);
    assert_eq!(found.entries.len(), 5);
    assert_eq!(found.summary.found, 5);
    assert!(found.summary.capped);
    assert!(!found.summary.cancelled);

    // Exactly the cap's worth of matches is not "capped".
    options.cap = 12;
    let found = run(&root, &options);
    assert_eq!(found.entries.len(), 12);
    assert!(!found.summary.capped);
    Ok(())
}

#[test]
fn results_stream_in_batches() -> TestResult {
    let scratch = Scratch::new("batches")?;
    for index in 0..10 {
        scratch.file(&format!("hit_{index}.txt"))?;
    }
    let mut options = SearchOptions::new("hit");
    options.batch_size = 4;
    let found = run(&scratch.root(), &options);
    assert_eq!(found.batches, vec![4, 4, 2]);
    Ok(())
}

#[test]
fn names_that_are_not_utf8_are_found_and_keep_their_bytes() -> TestResult {
    let scratch = Scratch::new("bytes")?;
    let name = OsStr::from_bytes(b"caf\xe9_hit.txt");
    let path = scratch.root().join(name);
    fs::create_dir_all(scratch.root())?;
    fs::write(&path, b"x")?;
    let root = scratch.root();
    for query in ["caf", "hit", "\u{fffd}_hit"] {
        let found = run(&root, &SearchOptions::new(query));
        assert_eq!(found.entries.len(), 1, "query {query:?}");
        let entry = found.entries.first().ok_or("one entry")?;
        assert_eq!(entry.path, path, "the raw path is kept");
        assert_eq!(entry.display_name(), "caf\u{fffd}_hit.txt");
    }
    Ok(())
}

#[test]
fn an_unreadable_folder_is_counted_and_the_rest_is_searched() -> TestResult {
    let scratch = Scratch::new("locked")?;
    scratch.file("locked/inner_hit.txt")?;
    scratch.file("open/hit.txt")?;
    let root = scratch.root();
    let locked = root.join("locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))?;
    let readable = fs::read_dir(&locked).is_ok();
    let found = run(&root, &SearchOptions::new("hit"));
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755))?;
    assert!(found.names(&root).contains("open/hit.txt"));
    if readable {
        // Running with privileges that ignore the mode: nothing to count.
        assert_eq!(found.summary.unreadable_dirs, 0);
    } else {
        assert_eq!(found.summary.unreadable_dirs, 1);
        assert_eq!(found.entries.len(), 1);
    }
    assert!(!found.summary.cancelled);
    Ok(())
}

#[test]
fn an_unreadable_root_is_counted_not_fatal() -> TestResult {
    let scratch = Scratch::new("noroot")?;
    let found = run(&scratch.0.join("missing"), &SearchOptions::new("hit"));
    assert!(found.entries.is_empty());
    assert_eq!(found.summary.unreadable_dirs, 1);
    Ok(())
}

#[test]
fn a_search_never_changes_the_tree() -> TestResult {
    let scratch = Scratch::new("readonly")?;
    scratch.file("a/hit.txt")?;
    scratch.file("a/b/other.txt")?;
    scratch.file(".hidden")?;
    let root = scratch.root();
    let snapshot = |root: &Path| -> Result<Vec<TreeFact>, Box<dyn Error>> {
        let mut seen = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for item in fs::read_dir(&dir)? {
                let item = item?;
                let meta = fs::symlink_metadata(item.path())?;
                seen.push((item.path(), meta.len(), meta.mtime()));
                if meta.is_dir() {
                    stack.push(item.path());
                }
            }
        }
        seen.sort();
        Ok(seen)
    };
    let before = snapshot(&root)?;
    let mut options = SearchOptions::new("hit");
    options.hide_hidden = false;
    drop(run(&root, &options));
    assert_eq!(snapshot(&root)?, before);
    Ok(())
}
