//! "New from template" on the UI side: the submenu of the New rows and the
//! job that copies a template into the shown folder.
//!
//! Listing the templates folder and copying a file live in
//! [`fileflux::core::templates`]. The copy goes through the same job path as
//! New file: the new file is selected and renamed in place, and Undo moves it
//! to the Trash if it is still the file that was created.

use std::path::Path;

use fileflux::core::Location;
use fileflux::core::templates::{self, Template};

use super::ops::{Identity, MenuCommand, MenuRow, Report, UndoAction};

/// The label of the row that opens the templates.
pub(crate) const LABEL: &str = "New from template";

/// Why `location` takes no new files from a template, or `None` for a real
/// folder (the same places New file is refused in).
pub(crate) fn refusal(location: &Location) -> Option<&'static str> {
    match location {
        Location::Directory(_) => None,
        Location::Trash => Some("Can't create files in the Trash"),
        Location::Recent => Some("Can't create files in Recent. Open a folder first"),
        Location::Search { .. } => {
            Some("Can't create files in search results. Open a folder first")
        }
    }
}

fn menu_rows(templates: &[Template]) -> Vec<MenuRow> {
    templates
        .iter()
        .map(|template| match template {
            Template::File { label, path } => MenuRow::TemplateFile {
                label: label.clone(),
                path: path.clone(),
            },
            Template::Folder { label, children } => MenuRow::TemplateFolder {
                label: label.clone(),
                rows: menu_rows(children),
            },
        })
        .collect()
}

/// Adds the "New from template" submenu right after "New file". Nothing
/// is added when there are no `templates` or the menu has no New file row.
pub(crate) fn add_rows(rows: &mut Vec<MenuRow>, templates: &[Template]) {
    if templates.is_empty() {
        return;
    }
    let after = rows
        .iter()
        .position(|row| matches!(row, MenuRow::Item(spec) if spec.command == MenuCommand::NewFile));
    if let Some(after) = after {
        rows.insert(
            after + 1,
            MenuRow::TemplateFolder {
                label: LABEL.to_owned(),
                rows: menu_rows(templates),
            },
        );
    }
}

/// Copies `template` into `dir`. Blocking: call it on the background
/// executor.
pub(crate) fn run_create(dir: &Path, template: &Path) -> Report {
    match templates::create_from(dir, template) {
        Ok(path) => {
            // Without an identity there is nothing safe to undo.
            let undo = Identity::of(&path)
                .ok()
                .map(|identity| UndoAction::Template {
                    path: path.clone(),
                    identity,
                });
            Report {
                select: vec![path],
                begin_rename: true,
                undo,
                ..Report::default()
            }
        }
        Err(error) => Report::error(format!(
            "Could not create the file from the template: {error}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::explorer::arrange::Arrangement;
    use crate::explorer::ops::{MenuTarget, menu_rows as context_rows, recent_menu_rows};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> std::io::Result<Scratch> {
            let dir = std::env::temp_dir().join(format!(
                "fileflux-ui-templates-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir)?;
            Ok(Scratch(dir))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn target(in_trash: bool) -> MenuTarget {
        MenuTarget {
            in_trash,
            selected: 0,
            folder_is_empty: false,
            terminal: true,
            can_paste: true,
            folder_selected: false,
            single_file: false,
            folder_favorite: false,
        }
    }

    fn sample() -> Vec<Template> {
        vec![
            Template::Folder {
                label: "Work".to_owned(),
                children: vec![Template::File {
                    label: "Contract".to_owned(),
                    path: PathBuf::from("/t/Work/Contract.odt"),
                }],
            },
            Template::File {
                label: "Letter".to_owned(),
                path: PathBuf::from("/t/Letter.odt"),
            },
        ]
    }

    fn labels(rows: &[MenuRow]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                MenuRow::Item(spec) => spec.label.to_owned(),
                MenuRow::Submenu(spec) => spec.label.to_owned(),
                MenuRow::TemplateFile { label, .. } | MenuRow::TemplateFolder { label, .. } => {
                    label.clone()
                }
                MenuRow::Header(label) => (*label).to_owned(),
                MenuRow::Separator => "-".to_owned(),
            })
            .collect()
    }

    #[test]
    fn the_submenu_follows_new_file_and_nests_the_folders() {
        let mut rows = context_rows(&target(false), &Arrangement::default());
        add_rows(&mut rows, &sample());
        let labels_now = labels(&rows);
        assert_eq!(
            labels_now.get(..3),
            Some(&["New folder", "New file", LABEL].map(str::to_owned)[..])
        );
        let inner = match rows.get(2) {
            Some(MenuRow::TemplateFolder { rows, .. }) => rows.as_slice(),
            _ => &[],
        };
        assert!(!inner.is_empty(), "no template submenu");
        assert_eq!(labels(inner), ["Work", "Letter"]);
        assert!(matches!(
            inner.first(),
            Some(MenuRow::TemplateFolder { rows, .. }) if labels(rows) == ["Contract"]
        ));
        assert!(matches!(
            inner.get(1),
            Some(MenuRow::TemplateFile { path, .. }) if path == Path::new("/t/Letter.odt")
        ));
    }

    #[test]
    fn without_templates_the_menu_is_unchanged() {
        let before = context_rows(&target(false), &Arrangement::default());
        let mut rows = before.clone();
        add_rows(&mut rows, &[]);
        assert_eq!(rows, before);
    }

    #[test]
    fn menus_without_new_file_get_no_submenu() {
        let mut trash = context_rows(&target(true), &Arrangement::default());
        let before = trash.clone();
        add_rows(&mut trash, &sample());
        assert_eq!(trash, before);
        let mut recent = recent_menu_rows(context_rows(&target(false), &Arrangement::default()), 0);
        let before = recent.clone();
        add_rows(&mut recent, &sample());
        assert_eq!(recent, before);
    }

    #[test]
    fn only_a_real_folder_takes_a_template() {
        assert_eq!(refusal(&Location::Directory(PathBuf::from("/x"))), None);
        assert!(refusal(&Location::Trash).is_some());
        assert!(refusal(&Location::Recent).is_some());
        assert!(
            refusal(&Location::Search {
                root: PathBuf::from("/x"),
                query: "a".to_owned()
            })
            .is_some()
        );
    }

    #[test]
    fn creating_selects_the_copy_asks_for_a_rename_and_can_be_undone() -> TestResult {
        let scratch = Scratch::new()?;
        let template = scratch.0.join("tpl/Report.odt");
        fs::create_dir_all(scratch.0.join("tpl"))?;
        fs::write(&template, b"template bytes")?;
        let dest = scratch.0.join("dest");
        fs::create_dir(&dest)?;
        fs::write(dest.join("Report.odt"), b"mine")?;
        let report = run_create(&dest, &template);
        let made = dest.join("Report (2).odt");
        assert_eq!(report.select, vec![made.clone()]);
        assert!(report.begin_rename);
        assert!(report.notice.is_none());
        assert_eq!(fs::read(&made)?, b"template bytes");
        assert_eq!(fs::read(dest.join("Report.odt"))?, b"mine");
        let identity = Identity::of(&made)?;
        assert_eq!(
            report.undo,
            Some(UndoAction::Template {
                path: made,
                identity
            })
        );
        Ok(())
    }

    #[test]
    fn a_failed_copy_reports_an_error_and_creates_nothing() -> TestResult {
        let scratch = Scratch::new()?;
        let dest = scratch.0.join("dest");
        fs::create_dir(&dest)?;
        let report = run_create(&dest, &scratch.0.join("gone.odt"));
        assert!(report.select.is_empty() && report.undo.is_none() && !report.begin_rename);
        assert!(report.notice.is_some());
        assert_eq!(fs::read_dir(&dest)?.count(), 0);
        Ok(())
    }
}
