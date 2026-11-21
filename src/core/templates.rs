//! Document templates: the entries of the user's templates folder offered by
//! the New menu, and creating a file from one.
//!
//! The folder is `XDG_TEMPLATES_DIR` of `user-dirs.dirs` (the way GNOME Files
//! and xdg-user-dirs find it), else `~/Templates`. A value equal to `$HOME`
//! is how xdg-user-dirs switches templates off, so it offers nothing.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use super::fs::natural_cmp;
use super::ops::unique_name;
use super::places::read_user_dirs;

/// How many levels of folders the menu nests: the templates folder itself is
/// level 1, so a folder at level 3 is not entered.
pub const MAX_DEPTH: usize = 3;
/// How many entries one level lists.
pub const MAX_PER_LEVEL: usize = 100;
/// How many entries a whole listing holds, nested folders included.
pub const MAX_TOTAL: usize = 500;

/// One row of the template menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Template {
    /// A file to copy. `label` is its name without the extension (the full
    /// name when another entry of the same level would read the same).
    File { label: String, path: PathBuf },
    /// A subfolder: a nested submenu. Never empty.
    Folder {
        label: String,
        children: Vec<Template>,
    },
}

impl Template {
    pub fn label(&self) -> &str {
        match self {
            Self::File { label, .. } | Self::Folder { label, .. } => label,
        }
    }
}

/// The templates folder for the `user-dirs.dirs` found in `config_home`
/// (`None`: switched off).
pub fn templates_dir(config_home: &Path, home: &Path) -> Option<PathBuf> {
    let mut warnings = Vec::new();
    let dirs = read_user_dirs(config_home, home, &mut warnings);
    match dirs.templates {
        None => Some(home.join("Templates")),
        Some(dir) if dir == home => None,
        Some(dir) => Some(dir),
    }
}

/// [`templates_dir`] for the process environment (`$HOME`,
/// `$XDG_CONFIG_HOME` when absolute, else `~/.config`).
pub fn templates_dir_from_env() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").filter(|home| !home.is_empty())?);
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    templates_dir(&config_home, &home)
}

/// The menu rows for the templates under `dir`, in the explorer's order
/// (folders first, natural name order). Hidden entries, broken links, links
/// to folders and anything that is not a regular file or a folder are left
/// out, and so are folders with nothing to offer. A missing or unreadable
/// folder lists nothing.
pub fn list(dir: &Path) -> Vec<Template> {
    let mut budget = MAX_TOTAL;
    list_level(dir, 1, &mut budget)
}

fn list_level(dir: &Path, depth: usize, budget: &mut usize) -> Vec<Template> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<OsString> = read
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .filter(|name| !is_hidden(name))
        .collect();
    names.sort_by(|a, b| natural_cmp(a, b));
    let mut folders = Vec::new();
    let mut files = Vec::new();
    for name in names {
        if *budget == 0 || folders.len() + files.len() >= MAX_PER_LEVEL {
            break;
        }
        let path = dir.join(&name);
        let Ok(link) = fs::symlink_metadata(&path) else {
            continue;
        };
        if link.is_dir() {
            if depth >= MAX_DEPTH {
                continue;
            }
            *budget -= 1;
            let children = list_level(&path, depth + 1, budget);
            if children.is_empty() {
                *budget += 1;
                continue;
            }
            folders.push((
                name.clone(),
                Template::Folder {
                    label: name.to_string_lossy().into_owned(),
                    children,
                },
            ));
        } else if fs::metadata(&path).is_ok_and(|target| target.is_file()) {
            *budget -= 1;
            files.push((
                name.clone(),
                Template::File {
                    label: name.to_string_lossy().into_owned(),
                    path,
                },
            ));
        }
    }
    relabel_files(&mut files);
    folders
        .into_iter()
        .chain(files)
        .map(|(_, template)| template)
        .collect()
}

fn is_hidden(name: &OsStr) -> bool {
    std::os::unix::ffi::OsStrExt::as_bytes(name).starts_with(b".")
}

/// Files show their name without the extension, unless that would make two
/// rows of the level read the same.
fn relabel_files(files: &mut [(OsString, Template)]) {
    let stems: Vec<String> = files
        .iter()
        .map(|(name, _)| {
            Path::new(name)
                .file_stem()
                .map_or_else(|| name.to_string_lossy(), OsStr::to_string_lossy)
                .into_owned()
        })
        .collect();
    for (index, (_, template)) in files.iter_mut().enumerate() {
        let Some(stem) = stems.get(index) else {
            continue;
        };
        let shared = stems.iter().filter(|other| *other == stem).count() > 1;
        if let (Template::File { label, .. }, false) = (template, shared) {
            label.clone_from(stem);
        }
    }
}

/// Copies the contents of `template` into a new file in `dir`, named like the
/// template (a link is followed: its contents are copied, never the link),
/// and returns the new path. A taken name gets " (2)", " (3)", ... before the
/// extension; nothing is ever overwritten. A copy that fails leaves nothing
/// behind.
pub fn create_from(dir: &Path, template: &Path) -> io::Result<PathBuf> {
    let name = template
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the template has no name"))?;
    let mut source = File::open(template)?;
    if !source.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the template is not a regular file",
        ));
    }
    // A name can be taken between choosing it and creating the file.
    for _ in 0..32 {
        let path = dir.join(unique_name(dir, name)?);
        let mut target = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        return match io::copy(&mut source, &mut target).and_then(|_| target.sync_all()) {
            Ok(()) => Ok(path),
            Err(error) => {
                drop(target);
                fs::remove_file(&path).ok();
                Err(error)
            }
        };
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free name for the new file",
    ))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> io::Result<Scratch> {
            let dir = std::env::temp_dir().join(format!(
                "fileflux-templates-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir)?;
            Ok(Scratch(dir))
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }

        fn write(&self, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
            let path = self.path(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, bytes)?;
            Ok(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn labels(rows: &[Template]) -> Vec<&str> {
        rows.iter().map(Template::label).collect()
    }

    fn dir_with(config: &str) -> Result<Option<PathBuf>, io::Error> {
        let scratch = Scratch::new()?;
        scratch.write("config/user-dirs.dirs", config.as_bytes())?;
        Ok(templates_dir(&scratch.path("config"), Path::new("/home/u")))
    }

    // --- user-dirs.dirs ---------------------------------------------------

    #[test]
    fn the_templates_dir_expands_home_and_strips_quotes() -> TestResult {
        assert_eq!(
            dir_with("XDG_TEMPLATES_DIR=\"$HOME/Modelos\"\n")?,
            Some(PathBuf::from("/home/u/Modelos"))
        );
        assert_eq!(
            dir_with("XDG_TEMPLATES_DIR='/srv/tpl'\n")?,
            Some(PathBuf::from("/srv/tpl"))
        );
        assert_eq!(
            dir_with("XDG_TEMPLATES_DIR=/srv/plain\n")?,
            Some(PathBuf::from("/srv/plain"))
        );
        Ok(())
    }

    #[test]
    fn comments_and_other_keys_do_not_set_the_templates_dir() -> TestResult {
        let text = "# XDG_TEMPLATES_DIR=\"$HOME/Nope\"\nXDG_DESKTOP_DIR=\"$HOME/Desk\"\n";
        assert_eq!(dir_with(text)?, Some(PathBuf::from("/home/u/Templates")));
        Ok(())
    }

    #[test]
    fn a_missing_key_or_file_falls_back_to_the_templates_folder() -> TestResult {
        assert_eq!(dir_with("")?, Some(PathBuf::from("/home/u/Templates")));
        let scratch = Scratch::new()?;
        assert_eq!(
            templates_dir(&scratch.path("absent"), Path::new("/home/u")),
            Some(PathBuf::from("/home/u/Templates"))
        );
        Ok(())
    }

    #[test]
    fn a_value_equal_to_home_switches_templates_off() -> TestResult {
        assert_eq!(dir_with("XDG_TEMPLATES_DIR=\"$HOME\"\n")?, None);
        assert_eq!(dir_with("XDG_TEMPLATES_DIR=\"$HOME/\"\n")?, None);
        assert_eq!(dir_with("XDG_TEMPLATES_DIR=\"/home/u\"\n")?, None);
        Ok(())
    }

    #[test]
    fn the_last_templates_assignment_wins() -> TestResult {
        let text = "XDG_TEMPLATES_DIR=\"$HOME/A\"\nXDG_TEMPLATES_DIR=\"$HOME/B\"\n";
        assert_eq!(dir_with(text)?, Some(PathBuf::from("/home/u/B")));
        Ok(())
    }

    // --- listing ----------------------------------------------------------

    #[test]
    fn a_missing_or_empty_folder_lists_nothing() -> TestResult {
        let scratch = Scratch::new()?;
        assert!(list(&scratch.path("absent")).is_empty());
        fs::create_dir(scratch.path("empty"))?;
        assert!(list(&scratch.path("empty")).is_empty());
        Ok(())
    }

    #[test]
    fn files_list_without_extension_in_natural_order() -> TestResult {
        let scratch = Scratch::new()?;
        scratch.write("t/Invoice 10.ods", b"")?;
        scratch.write("t/invoice 2.ods", b"")?;
        scratch.write("t/Letter.odt", b"")?;
        scratch.write("t/notes", b"")?;
        let rows = list(&scratch.path("t"));
        assert_eq!(
            labels(&rows),
            ["invoice 2", "Invoice 10", "Letter", "notes"]
        );
        assert_eq!(
            rows.first(),
            Some(&Template::File {
                label: "invoice 2".to_owned(),
                path: scratch.path("t/invoice 2.ods"),
            })
        );
        Ok(())
    }

    #[test]
    fn rows_that_would_read_the_same_keep_their_extension() -> TestResult {
        let scratch = Scratch::new()?;
        scratch.write("t/Report.odt", b"")?;
        scratch.write("t/Report.ods", b"")?;
        scratch.write("t/Other.txt", b"")?;
        assert_eq!(
            labels(&list(&scratch.path("t"))),
            ["Other", "Report.ods", "Report.odt"]
        );
        Ok(())
    }

    #[test]
    fn hidden_entries_broken_links_and_special_files_are_skipped() -> TestResult {
        let scratch = Scratch::new()?;
        scratch.write("t/.hidden.txt", b"")?;
        scratch.write("t/.git/config", b"x")?;
        scratch.write("t/Real.txt", b"")?;
        symlink(scratch.path("gone"), scratch.path("t/Broken.txt"))?;
        symlink(scratch.path("t/Real.txt"), scratch.path("t/Alias.txt"))?;
        scratch.write("elsewhere/inner.txt", b"x")?;
        symlink(scratch.path("elsewhere"), scratch.path("t/Linked folder"))?;
        std::os::unix::net::UnixListener::bind(scratch.path("t/socket"))?;
        assert_eq!(labels(&list(&scratch.path("t"))), ["Alias", "Real"]);
        Ok(())
    }

    #[test]
    fn folders_nest_first_and_empty_folders_are_left_out() -> TestResult {
        let scratch = Scratch::new()?;
        scratch.write("t/Zed.txt", b"")?;
        scratch.write("t/Work/Contract.odt", b"")?;
        scratch.write("t/Work/Deeper/Memo.txt", b"")?;
        fs::create_dir_all(scratch.path("t/Empty"))?;
        scratch.write("t/Hidden only/.secret", b"")?;
        let rows = list(&scratch.path("t"));
        assert_eq!(labels(&rows), ["Work", "Zed"]);
        let Some(Template::Folder { children, .. }) = rows.first() else {
            return Err("Work is not a folder".into());
        };
        assert_eq!(labels(children), ["Deeper", "Contract"]);
        Ok(())
    }

    #[test]
    fn folders_nest_three_levels_and_no_deeper() -> TestResult {
        let scratch = Scratch::new()?;
        scratch.write("t/one.txt", b"")?;
        scratch.write("t/a/two.txt", b"")?;
        scratch.write("t/a/b/three.txt", b"")?;
        scratch.write("t/a/b/c/four.txt", b"")?;
        let rows = list(&scratch.path("t"));
        let Some(Template::Folder { children: a, .. }) = rows.first() else {
            return Err("a is missing".into());
        };
        let Some(Template::Folder { children: b, .. }) = a.first() else {
            return Err("b is missing".into());
        };
        assert_eq!(labels(b), ["three"], "c would be a fourth level");
        Ok(())
    }

    #[test]
    fn one_level_lists_at_most_the_cap() -> TestResult {
        let scratch = Scratch::new()?;
        for index in 0..MAX_PER_LEVEL + 20 {
            scratch.write(&format!("t/file {index:03}.txt"), b"")?;
        }
        let rows = list(&scratch.path("t"));
        assert_eq!(rows.len(), MAX_PER_LEVEL);
        assert_eq!(rows.first().map(Template::label), Some("file 000"));
        Ok(())
    }

    // --- creating ---------------------------------------------------------

    #[test]
    fn creating_copies_the_exact_bytes_under_the_template_name() -> TestResult {
        let scratch = Scratch::new()?;
        let bytes: Vec<u8> = (0..=255u8).cycle().take(300_000).collect();
        let template = scratch.write("tpl/Report.odt", &bytes)?;
        fs::create_dir(scratch.path("dest"))?;
        let made = create_from(&scratch.path("dest"), &template)?;
        assert_eq!(made, scratch.path("dest/Report.odt"));
        assert_eq!(fs::read(&made)?, bytes);
        assert_eq!(fs::read(&template)?, bytes, "the template is untouched");
        Ok(())
    }

    #[test]
    fn a_taken_name_gets_a_number_and_nothing_is_overwritten() -> TestResult {
        let scratch = Scratch::new()?;
        let template = scratch.write("tpl/Report.odt", b"template")?;
        scratch.write("dest/Report.odt", b"mine")?;
        let second = create_from(&scratch.path("dest"), &template)?;
        assert_eq!(second, scratch.path("dest/Report (2).odt"));
        let third = create_from(&scratch.path("dest"), &template)?;
        assert_eq!(third, scratch.path("dest/Report (3).odt"));
        assert_eq!(fs::read(scratch.path("dest/Report.odt"))?, b"mine");
        assert_eq!(fs::read(second)?, b"template");
        Ok(())
    }

    #[test]
    fn a_dangling_name_is_taken_too() -> TestResult {
        let scratch = Scratch::new()?;
        let template = scratch.write("tpl/a.txt", b"x")?;
        fs::create_dir(scratch.path("dest"))?;
        symlink(scratch.path("nowhere"), scratch.path("dest/a.txt"))?;
        let made = create_from(&scratch.path("dest"), &template)?;
        assert_eq!(made, scratch.path("dest/a (2).txt"));
        assert!(
            !scratch.path("nowhere").exists(),
            "the link is not followed"
        );
        Ok(())
    }

    #[test]
    fn a_linked_template_copies_its_contents_not_the_link() -> TestResult {
        let scratch = Scratch::new()?;
        let real = scratch.write("store/real.bin", b"payload")?;
        fs::create_dir_all(scratch.path("tpl"))?;
        symlink(&real, scratch.path("tpl/Shown.dat"))?;
        fs::create_dir(scratch.path("dest"))?;
        let made = create_from(&scratch.path("dest"), &scratch.path("tpl/Shown.dat"))?;
        assert_eq!(made, scratch.path("dest/Shown.dat"));
        assert!(!fs::symlink_metadata(&made)?.file_type().is_symlink());
        assert_eq!(fs::read(&made)?, b"payload");
        Ok(())
    }

    #[test]
    fn a_template_that_is_not_a_file_creates_nothing() -> TestResult {
        let scratch = Scratch::new()?;
        fs::create_dir_all(scratch.path("tpl/folder"))?;
        fs::create_dir(scratch.path("dest"))?;
        assert!(create_from(&scratch.path("dest"), &scratch.path("tpl/folder")).is_err());
        assert!(create_from(&scratch.path("dest"), &scratch.path("tpl/gone")).is_err());
        assert_eq!(fs::read_dir(scratch.path("dest"))?.count(), 0);
        Ok(())
    }
}
