//! Custom actions on the UI side: the context-menu rows and running one.
//!
//! The file format, matching and placeholder expansion live in
//! [`fileflux::core::custom_actions`]. The menu offers the actions that match
//! the selection: inline rows when there are at most [`INLINE_MAX`], else one
//! "Custom actions" flyout. They are read again whenever a menu opens (one
//! `stat` unless the file changed).

use std::path::{Path, PathBuf};

use fileflux::core::Location;
use fileflux::core::custom_actions::{Action, AppearsOn, Candidate};
use gpui::Context;

use super::ops::{MenuCommand, MenuRow};
use super::view::Explorer;

/// Up to this many matching actions are inline rows; more go in a flyout.
pub(crate) const INLINE_MAX: usize = 3;

/// The label of the flyout.
pub(crate) const FLYOUT_LABEL: &str = "Custom actions";

/// The indices (into `actions`) of the actions offered for `selection`.
pub(crate) fn matching(actions: &[Action], selection: &[Candidate<'_>]) -> Vec<usize> {
    actions
        .iter()
        .enumerate()
        .filter(|(_, action)| action.applies_to(selection))
        .map(|(index, _)| index)
        .collect()
}

/// The rows for the matching actions: none, one per action, or one flyout.
pub(crate) fn rows_for(actions: &[Action], matched: &[usize]) -> Vec<MenuRow> {
    let rows: Vec<MenuRow> = matched
        .iter()
        .filter_map(|&index| {
            Some(MenuRow::Custom {
                label: actions.get(index)?.name.clone(),
                index,
            })
        })
        .collect();
    if rows.len() <= INLINE_MAX {
        rows
    } else {
        vec![MenuRow::CustomFolder {
            label: FLYOUT_LABEL.to_owned(),
            rows,
        }]
    }
}

/// Puts `group` in the menu of entries after "Open with…" (else at the end
/// of the first group of rows), or at the end of the empty-space menu in a
/// group of its own.
pub(crate) fn add_rows(rows: &mut Vec<MenuRow>, group: Vec<MenuRow>, background: bool) {
    if group.is_empty() {
        return;
    }
    if background {
        if !matches!(rows.last(), None | Some(MenuRow::Separator)) {
            rows.push(MenuRow::Separator);
        }
        rows.extend(group);
        return;
    }
    let at = rows
        .iter()
        .position(|row| matches!(row, MenuRow::Item(spec) if spec.command == MenuCommand::OpenWith))
        .map(|open_with| open_with + 1)
        .or_else(|| {
            rows.iter()
                .position(|row| matches!(row, MenuRow::Separator))
        })
        .unwrap_or(rows.len());
    rows.splice(at..at, group);
}

/// Where the program starts: the shown folder, or for search and Recent
/// results the folder of the first path.
pub(crate) fn working_dir(shown: &Location, paths: &[PathBuf]) -> Option<PathBuf> {
    match shown {
        Location::Directory(folder) => Some(folder.clone()),
        Location::Trash | Location::Recent | Location::Search { .. } => paths
            .first()
            .and_then(|path| path.parent())
            .map(Path::to_path_buf),
    }
}

impl Explorer {
    /// Reads the actions file again when it changed since the last menu
    /// (one `stat` otherwise), and tells once when it is invalid.
    pub(crate) fn refresh_custom_actions(&mut self) {
        let path_var = std::env::var_os("PATH");
        if let Some(error) = self.custom_actions.refresh(path_var.as_deref()) {
            self.tab_mut()
                .state
                .set_notice(format!("Custom actions are off: {error}"));
        }
    }

    /// The rows of the custom actions for a menu on `selection` (empty: the
    /// empty space of a folder).
    pub(crate) fn custom_action_rows(&self, selection: &[Candidate<'_>]) -> Vec<MenuRow> {
        let actions = self.custom_actions.actions();
        rows_for(actions, &matching(actions, selection))
    }

    /// Runs the action `index` of the menu that was just chosen from, on the
    /// selection (or the shown folder for a background action).
    pub(crate) fn run_custom_action(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(action) = self.custom_actions.actions().get(index).cloned() else {
            return;
        };
        let shown = self.tab().state.current().clone();
        let paths = if action.appears_on == AppearsOn::Background {
            match &shown {
                Location::Directory(folder) => vec![folder.clone()],
                _ => return,
            }
        } else if matches!(shown, Location::Trash) {
            return;
        } else {
            self.selected_visible_paths()
        };
        let failure = format!("Could not run {}", action.name);
        match action.expand(&paths) {
            Ok(invocation) => self.launch(
                PathBuf::from(invocation.program),
                invocation.args,
                working_dir(&shown, &paths),
                failure,
                cx,
            ),
            Err(error) => {
                self.tab_mut()
                    .state
                    .set_notice(format!("{failure}: {error}"));
                cx.notify();
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use std::ffi::OsStr;

    use fileflux::core::custom_actions::parse;

    use super::*;
    use crate::explorer::arrange::Arrangement;
    use crate::explorer::ops::{MenuTarget, menu_rows};

    fn actions(count: usize, appears_on: &str) -> Vec<Action> {
        let text: String = (1..=count)
            .map(|n| {
                format!(
                    "[[action]]\nname = \"Action {n}\"\ncommand = [\"sh\"]\nappears_on = \"{appears_on}\"\n"
                )
            })
            .collect();
        parse(&text, Some(OsStr::new("/bin"))).expect("valid")
    }

    fn label(row: &MenuRow) -> String {
        match row {
            MenuRow::Item(spec) => spec.label.to_owned(),
            MenuRow::Custom { label, .. } | MenuRow::CustomFolder { label, .. } => label.clone(),
            MenuRow::Separator => "-".to_owned(),
            other => format!("{other:?}"),
        }
    }

    fn target(selected: usize, single_file: bool) -> MenuTarget {
        MenuTarget {
            in_trash: false,
            selected,
            folder_is_empty: false,
            terminal: false,
            can_paste: false,
            folder_selected: false,
            single_file,
            folder_favorite: false,
        }
    }

    fn file(name: &str) -> Candidate<'_> {
        Candidate {
            name: OsStr::new(name),
            is_directory: false,
        }
    }

    #[test]
    fn up_to_three_actions_are_inline_rows() {
        let all = actions(3, "files");
        let matched = matching(&all, &[file("a.txt")]);
        let rows = rows_for(&all, &matched);
        assert_eq!(
            rows.iter().map(label).collect::<Vec<_>>(),
            ["Action 1", "Action 2", "Action 3"]
        );
        assert!(rows.iter().all(|row| matches!(row, MenuRow::Custom { .. })));
        assert!(rows_for(&all, &[]).is_empty(), "nothing matched, no rows");
    }

    #[test]
    fn more_than_three_actions_become_one_flyout() {
        let all = actions(4, "files");
        let rows = rows_for(&all, &matching(&all, &[file("a.txt")]));
        let [MenuRow::CustomFolder { label, rows: inner }] = rows.as_slice() else {
            panic!("one flyout expected: {rows:?}");
        };
        assert_eq!(label, "Custom actions");
        assert_eq!(inner.len(), 4);
        assert!(matches!(&inner[3], MenuRow::Custom { index: 3, .. }));
    }

    #[test]
    fn only_matching_actions_count_towards_the_limit() {
        let mut all = actions(4, "files");
        all.extend(actions(2, "folders"));
        let rows = rows_for(&all, &matching(&all, &[file("a.txt")]));
        assert!(matches!(rows.as_slice(), [MenuRow::CustomFolder { .. }]));
        let folder = [Candidate {
            name: OsStr::new("d"),
            is_directory: true,
        }];
        let rows = rows_for(&all, &matching(&all, &folder));
        assert_eq!(rows.len(), 2);
        assert!(matches!(&rows[0], MenuRow::Custom { index: 4, .. }));
    }

    #[test]
    fn inline_rows_follow_open_with_in_the_entry_menu() {
        let all = actions(2, "files");
        let mut rows = menu_rows(&target(1, true), &Arrangement::default());
        add_rows(
            &mut rows,
            rows_for(&all, &matching(&all, &[file("a.txt")])),
            false,
        );
        let labels: Vec<String> = rows.iter().map(label).collect();
        let open_with = labels
            .iter()
            .position(|l| l.starts_with("Open with"))
            .expect("Open with is offered");
        assert_eq!(labels[open_with + 1], "Action 1");
        assert_eq!(labels[open_with + 2], "Action 2");
        assert_eq!(labels[open_with + 3], "-", "the group ends where it did");
    }

    #[test]
    fn a_flyout_follows_open_with_too_and_a_folder_menu_keeps_its_first_group() {
        let all = actions(5, "both");
        let mut rows = menu_rows(&target(1, true), &Arrangement::default());
        add_rows(
            &mut rows,
            rows_for(&all, &matching(&all, &[file("a.txt")])),
            false,
        );
        let labels: Vec<String> = rows.iter().map(label).collect();
        let open_with = labels
            .iter()
            .position(|l| l.starts_with("Open with"))
            .expect("row exists");
        assert_eq!(labels[open_with + 1], "Custom actions");

        let mut folder_menu = menu_rows(
            &MenuTarget {
                folder_selected: true,
                ..target(1, false)
            },
            &Arrangement::default(),
        );
        let before = folder_menu.len();
        add_rows(&mut folder_menu, rows_for(&all, &[0]), false);
        assert_eq!(folder_menu.len(), before + 1);
        let at = folder_menu
            .iter()
            .position(|row| matches!(row, MenuRow::Custom { .. }))
            .expect("row exists");
        assert_eq!(
            folder_menu[at + 1],
            MenuRow::Separator,
            "before the first separator"
        );
    }

    #[test]
    fn background_actions_end_the_empty_space_menu_in_a_group() {
        let all = actions(1, "background");
        let mut rows = menu_rows(&target(0, false), &Arrangement::default());
        let before = rows.len();
        add_rows(&mut rows, rows_for(&all, &matching(&all, &[])), true);
        let labels: Vec<String> = rows.iter().map(label).collect();
        assert_eq!(rows.len(), before + 2);
        assert_eq!(labels[before], "-");
        assert_eq!(labels[before + 1], "Action 1");
        // Entries never see background actions.
        assert!(matching(&all, &[file("a.txt")]).is_empty());
        // No rows, no change.
        let mut same = rows.clone();
        add_rows(&mut same, Vec::new(), true);
        assert_eq!(same, rows);
    }

    #[test]
    fn the_working_directory_is_the_shown_folder_or_the_results_folder() {
        let paths = [PathBuf::from("/real/dir/a.txt")];
        assert_eq!(
            working_dir(&Location::Directory(PathBuf::from("/shown")), &paths),
            Some(PathBuf::from("/shown"))
        );
        assert_eq!(
            working_dir(&Location::Recent, &paths),
            Some(PathBuf::from("/real/dir"))
        );
        assert_eq!(
            working_dir(
                &Location::Search {
                    root: PathBuf::from("/root"),
                    query: "a".to_owned()
                },
                &paths
            ),
            Some(PathBuf::from("/real/dir"))
        );
        assert_eq!(working_dir(&Location::Recent, &[]), None);
    }
}
