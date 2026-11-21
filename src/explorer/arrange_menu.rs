//! The Sort and Filter menus as data: the rows the subbar buttons open and
//! the cascading submenus of the empty-space context menu. Both draw from the one
//! [`Arrangement`], and every row's command goes through
//! [`Arrangement::apply`], so the menus, the chips, Ctrl+H and the list
//! header can never disagree.

use fileflux::core::kinds::FileCategory;
use fileflux::core::settings::ColumnPref;
use fileflux::core::zoom::ZoomCommand;

use super::arrange::{ArrangeCommand, Arrangement, DateRange, SizeRange, SortDir, SortKey};
use super::ops::{ArrangeMenu, MenuCommand, MenuItemSpec, MenuRow, SubmenuSpec};

/// The shortcut hint of the hidden-files row.
pub(crate) const HIDDEN_SHORTCUT: &str = "Ctrl+H";

fn choice(command: ArrangeCommand, label: &'static str, checked: bool) -> MenuRow {
    MenuRow::Item(MenuItemSpec {
        checked: Some(checked),
        ..MenuItemSpec::new(MenuCommand::Arrange(command), label)
    })
}

/// The rows of the Sort menu.
pub(crate) fn sort_rows(arrangement: &Arrangement) -> Vec<MenuRow> {
    let sort = arrangement.sort;
    let mut rows = vec![MenuRow::Header("Sort by")];
    rows.extend(
        SortKey::ALL
            .into_iter()
            .map(|key| choice(ArrangeCommand::SortBy(key), key.label(), sort.key == key)),
    );
    rows.push(MenuRow::Separator);
    rows.extend(
        [SortDir::Ascending, SortDir::Descending]
            .into_iter()
            .map(|dir| choice(ArrangeCommand::SetDir(dir), dir.label(), sort.dir == dir)),
    );
    rows.push(MenuRow::Separator);
    rows.push(choice(
        ArrangeCommand::ToggleFoldersFirst,
        "Folders first",
        sort.folders_first,
    ));
    rows.push(choice(
        ArrangeCommand::ToggleNatural,
        "Natural number order",
        sort.natural,
    ));
    rows
}

/// The rows of the Filter menu: the hidden-files toggle and "Clear all" on
/// top, where a short window never scrolls them out of reach, then the
/// three groups.
pub(crate) fn filter_rows(arrangement: &Arrangement) -> Vec<MenuRow> {
    let filters = &arrangement.filters;
    let mut rows = vec![
        MenuRow::Item(MenuItemSpec {
            shortcut: Some(HIDDEN_SHORTCUT),
            checked: Some(!filters.hide_hidden),
            ..MenuItemSpec::new(
                MenuCommand::Arrange(ArrangeCommand::ToggleHidden),
                "Show hidden files",
            )
        }),
        MenuRow::Item(MenuItemSpec {
            enabled: !filters.is_default(),
            ..MenuItemSpec::new(
                MenuCommand::Arrange(ArrangeCommand::ClearFilters),
                "Clear all filters",
            )
        }),
        MenuRow::Separator,
        MenuRow::Header("Type"),
    ];
    rows.extend(FileCategory::ALL.into_iter().map(|category| {
        choice(
            ArrangeCommand::ToggleCategory(category),
            category.label(),
            filters.kinds.contains(&category),
        )
    }));
    rows.push(MenuRow::Separator);
    rows.push(MenuRow::Header("Modified"));
    rows.extend(DateRange::ALL.into_iter().map(|range| {
        choice(
            ArrangeCommand::ToggleDate(range),
            range.label(),
            filters.modified == Some(range),
        )
    }));
    rows.push(MenuRow::Separator);
    rows.push(MenuRow::Header("Size"));
    rows.extend(SizeRange::ALL.into_iter().map(|range| {
        choice(
            ArrangeCommand::ToggleSize(range),
            range.label(),
            filters.size == Some(range),
        )
    }));
    rows
}

/// The rows of the Select menu.
pub(crate) fn select_rows() -> Vec<MenuRow> {
    let row = |command, label, shortcut| {
        MenuRow::Item(MenuItemSpec {
            shortcut: Some(shortcut),
            ..MenuItemSpec::new(command, label)
        })
    };
    vec![
        row(MenuCommand::SelectAll, "Select all", "Ctrl+A"),
        row(
            MenuCommand::InvertSelection,
            "Invert selection",
            "Ctrl+Shift+I",
        ),
        row(
            MenuCommand::SelectMatching,
            "Select items matching\u{2026}",
            "Ctrl+S",
        ),
    ]
}

/// The rows of the Columns menu: one check row per optional column, checked
/// when `columns` shows it. Name has no row: it is always shown.
pub(crate) fn column_rows(columns: &[ColumnPref]) -> Vec<MenuRow> {
    use ColumnPref::{Group, Modified, Owner, Permissions, Size, Type};
    [
        (Size, "Size"),
        (Modified, "Modified"),
        (Type, "Type"),
        (Permissions, "Permissions"),
        (Owner, "Owner"),
        (Group, "Group"),
    ]
    .into_iter()
    .map(|(column, label)| {
        MenuRow::Item(MenuItemSpec {
            checked: Some(columns.contains(&column)),
            ..MenuItemSpec::new(MenuCommand::ToggleColumn(column), label)
        })
    })
    .collect()
}

/// The rows of the Zoom menu: the icon size of the view that is shown. The
/// shortcut hints name the first chord of each (Ctrl++ also zooms in).
pub(crate) fn zoom_rows() -> Vec<MenuRow> {
    [
        (ZoomCommand::In, "Zoom in", "Ctrl+="),
        (ZoomCommand::Out, "Zoom out", "Ctrl+-"),
        (ZoomCommand::Reset, "Reset zoom", "Ctrl+0"),
    ]
    .into_iter()
    .map(|(command, label, shortcut)| {
        MenuRow::Item(MenuItemSpec {
            shortcut: Some(shortcut),
            ..MenuItemSpec::new(MenuCommand::Zoom(command), label)
        })
    })
    .collect()
}

/// `columns` after a click on `column`'s row: shown when it was hidden,
/// hidden when it was shown, in the canonical order.
pub(crate) fn toggled_columns(columns: &[ColumnPref], column: ColumnPref) -> Vec<ColumnPref> {
    let mut next: Vec<ColumnPref> = columns.iter().copied().filter(|c| *c != column).collect();
    if !columns.contains(&column) {
        next.push(column);
    }
    next.sort_unstable();
    next.dedup();
    next
}

/// The rows of `menu` for `arrangement` and the list's `columns`.
pub(crate) fn rows(
    menu: ArrangeMenu,
    arrangement: &Arrangement,
    columns: &[ColumnPref],
) -> Vec<MenuRow> {
    match menu {
        ArrangeMenu::Sort => sort_rows(arrangement),
        ArrangeMenu::Filter => filter_rows(arrangement),
        ArrangeMenu::Select => select_rows(),
        ArrangeMenu::Columns => column_rows(columns),
        ArrangeMenu::Zoom => zoom_rows(),
    }
}

/// `rows` of an empty-space menu with the Columns submenu after Filter.
/// The list view offers it; the grid has no columns to choose.
pub(crate) fn with_columns(
    mut rows: Vec<MenuRow>,
    arrangement: &Arrangement,
    columns: &[ColumnPref],
) -> Vec<MenuRow> {
    let after_filter = rows
        .iter()
        .position(|row| matches!(row, MenuRow::Submenu(spec) if spec.menu == ArrangeMenu::Filter));
    if let Some(at) = after_filter {
        rows.insert(at + 1, submenu(ArrangeMenu::Columns, arrangement, columns));
    }
    rows
}

/// `rows` of an empty-space menu with the Zoom submenu after the last of
/// Filter and Columns, so it is the view-related group's tail in both views.
pub(crate) fn with_zoom(mut rows: Vec<MenuRow>) -> Vec<MenuRow> {
    let after = rows.iter().rposition(|row| {
        matches!(row, MenuRow::Submenu(spec)
            if matches!(spec.menu, ArrangeMenu::Filter | ArrangeMenu::Columns))
    });
    if let Some(at) = after {
        rows.insert(
            at + 1,
            MenuRow::Submenu(SubmenuSpec {
                menu: ArrangeMenu::Zoom,
                label: ArrangeMenu::Zoom.label(),
                rows: zoom_rows(),
            }),
        );
    }
    rows
}

/// The row of a context menu that opens `menu` as a cascading submenu.
pub(crate) fn submenu(
    menu: ArrangeMenu,
    arrangement: &Arrangement,
    columns: &[ColumnPref],
) -> MenuRow {
    MenuRow::Submenu(SubmenuSpec {
        menu,
        label: menu.label(),
        rows: rows(menu, arrangement, columns),
    })
}

/// Redraws the checks of every Sort/Filter submenu in `rows` from
/// `arrangement`, after a choice changed it.
pub(crate) fn refresh(
    rows_to_refresh: &mut [MenuRow],
    arrangement: &Arrangement,
    columns: &[ColumnPref],
) {
    for row in rows_to_refresh {
        if let MenuRow::Submenu(spec) = row {
            spec.rows = rows(spec.menu, arrangement, columns);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::ops::{MenuTarget, menu_rows};

    fn items(rows: &[MenuRow]) -> Vec<&MenuItemSpec> {
        rows.iter()
            .filter_map(|row| match row {
                MenuRow::Item(spec) => Some(spec),
                MenuRow::Header(_)
                | MenuRow::Separator
                | MenuRow::Submenu(_)
                | MenuRow::TemplateFile { .. }
                | MenuRow::TemplateFolder { .. }
                | MenuRow::Custom { .. }
                | MenuRow::CustomFolder { .. } => None,
            })
            .collect()
    }

    fn checked_labels(rows: &[MenuRow]) -> Vec<&'static str> {
        items(rows)
            .into_iter()
            .filter(|spec| spec.checked == Some(true))
            .map(|spec| spec.label)
            .collect()
    }

    fn labels(rows: &[MenuRow]) -> Vec<&'static str> {
        items(rows).into_iter().map(|spec| spec.label).collect()
    }

    fn row<'a>(rows: &'a [MenuRow], label: &str) -> Option<&'a MenuItemSpec> {
        items(rows).into_iter().find(|spec| spec.label == label)
    }

    fn submenu_of(rows: &[MenuRow], menu: ArrangeMenu) -> Option<&SubmenuSpec> {
        rows.iter().find_map(|row| match row {
            MenuRow::Submenu(spec) if spec.menu == menu => Some(spec),
            _ => None,
        })
    }

    fn target(selected: usize, in_trash: bool) -> MenuTarget {
        MenuTarget {
            in_trash,
            selected,
            folder_is_empty: false,
            terminal: true,
            can_paste: true,
            folder_selected: false,
            single_file: selected == 1,
            folder_favorite: false,
        }
    }

    #[test]
    fn the_sort_menu_checks_the_active_key_direction_and_toggles() {
        let mut arrangement = Arrangement::default();
        let rows = sort_rows(&arrangement);
        assert_eq!(
            labels(&rows),
            [
                "Name",
                "Size",
                "Modified",
                "Type",
                "Permissions",
                "Owner",
                "Group",
                "Ascending",
                "Descending",
                "Folders first",
                "Natural number order"
            ]
        );
        assert_eq!(
            checked_labels(&rows),
            [
                "Modified",
                "Descending",
                "Folders first",
                "Natural number order"
            ]
        );
        assert!(matches!(rows.first(), Some(MenuRow::Header("Sort by"))));

        arrangement.apply(ArrangeCommand::SortBy(SortKey::Type));
        arrangement.apply(ArrangeCommand::SetDir(SortDir::Descending));
        arrangement.apply(ArrangeCommand::ToggleFoldersFirst);
        arrangement.apply(ArrangeCommand::ToggleNatural);
        let rows = sort_rows(&arrangement);
        assert_eq!(checked_labels(&rows), ["Type", "Descending"]);
    }

    #[test]
    fn the_filter_menu_lists_the_four_groups_with_their_check_states() {
        let mut arrangement = Arrangement::default();
        let rows = filter_rows(&arrangement);
        assert_eq!(
            labels(&rows),
            [
                "Show hidden files",
                "Clear all filters",
                "Folders",
                "Images",
                "Videos",
                "Audio",
                "Documents",
                "Archives",
                "Code",
                "Other",
                "Today",
                "Last 7 days",
                "Last 30 days",
                "This year",
                "Older",
                "< 1 MB",
                "1\u{2013}100 MB",
                "100 MB\u{2013}1 GB",
                "> 1 GB"
            ]
        );
        assert_eq!(
            checked_labels(&rows),
            Vec::<&str>::new(),
            "hidden files are hidden by default"
        );
        let hidden = row(&rows, "Show hidden files");
        assert_eq!(hidden.and_then(|spec| spec.shortcut), Some("Ctrl+H"));
        assert_eq!(
            row(&rows, "Clear all filters").map(|spec| spec.enabled),
            Some(false),
            "nothing to clear yet"
        );

        for command in [
            ArrangeCommand::ToggleCategory(FileCategory::Images),
            ArrangeCommand::ToggleCategory(FileCategory::Code),
            ArrangeCommand::ToggleDate(DateRange::Last7Days),
            ArrangeCommand::ToggleSize(SizeRange::Over1Gb),
            ArrangeCommand::ToggleHidden,
        ] {
            arrangement.apply(command);
        }
        let rows = filter_rows(&arrangement);
        assert_eq!(
            checked_labels(&rows),
            [
                "Show hidden files",
                "Images",
                "Code",
                "Last 7 days",
                "> 1 GB"
            ]
        );
        assert_eq!(
            row(&rows, "Clear all filters").map(|spec| spec.enabled),
            Some(true)
        );
    }

    #[test]
    fn every_row_command_goes_through_the_one_model() {
        let arrangement = Arrangement::default();
        let mut commands = Vec::new();
        let mut unexpected = Vec::new();
        for rows in [sort_rows(&arrangement), filter_rows(&arrangement)] {
            for spec in items(&rows) {
                match spec.command {
                    MenuCommand::Arrange(command) => commands.push(command),
                    other => unexpected.push((spec.label, other)),
                }
            }
        }
        assert!(
            unexpected.is_empty(),
            "not sort/filter commands: {unexpected:?}"
        );
        assert_eq!(commands.len(), 11 + 2 + 8 + 5 + 4);
        for command in commands {
            let mut changed = Arrangement::default();
            changed.apply(command);
            // Already-default choices and "clear" have nothing to change.
            let is_default_choice = matches!(
                command,
                ArrangeCommand::SortBy(SortKey::Modified)
                    | ArrangeCommand::SetDir(SortDir::Descending)
                    | ArrangeCommand::ClearFilters
            );
            if !is_default_choice {
                assert_ne!(changed, arrangement, "{command:?} changes nothing");
            }
        }
    }

    #[test]
    fn the_columns_menu_checks_the_columns_that_are_shown() {
        let defaults = ColumnPref::defaults();
        let rows = column_rows(&defaults);
        assert_eq!(
            labels(&rows),
            ["Size", "Modified", "Type", "Permissions", "Owner", "Group"],
            "Name has no row: it is always shown"
        );
        assert_eq!(
            checked_labels(&rows),
            ["Size", "Modified"],
            "today's columns"
        );
        let all = [
            ColumnPref::Group,
            ColumnPref::Type,
            ColumnPref::Owner,
            ColumnPref::Permissions,
        ];
        assert_eq!(
            checked_labels(&column_rows(&all)),
            ["Type", "Permissions", "Owner", "Group"]
        );
        assert!(checked_labels(&column_rows(&[])).is_empty());
        for spec in items(&rows) {
            assert!(spec.enabled && spec.shortcut.is_none() && !spec.danger);
            assert!(matches!(spec.command, MenuCommand::ToggleColumn(_)));
        }
    }

    #[test]
    fn clicking_a_column_row_shows_or_hides_it_in_the_canonical_order() {
        let defaults = ColumnPref::defaults();
        assert_eq!(
            toggled_columns(&defaults, ColumnPref::Owner),
            [ColumnPref::Size, ColumnPref::Modified, ColumnPref::Owner]
        );
        assert_eq!(
            toggled_columns(&defaults, ColumnPref::Size),
            [ColumnPref::Modified]
        );
        assert_eq!(
            toggled_columns(&[ColumnPref::Group], ColumnPref::Size),
            [ColumnPref::Size, ColumnPref::Group],
            "canonical order, not click order"
        );
        assert!(toggled_columns(&[ColumnPref::Type], ColumnPref::Type).is_empty());
    }

    #[test]
    fn the_list_menu_nests_columns_after_filter_and_the_grid_menu_does_not() {
        let arrangement = Arrangement::default();
        let plain = menu_rows(&target(0, false), &arrangement);
        assert!(
            submenu_of(&plain, ArrangeMenu::Columns).is_none(),
            "the grid: no columns"
        );
        let columns = [ColumnPref::Owner];
        let rows = with_columns(plain.clone(), &arrangement, &columns);
        let spec = submenu_of(&rows, ArrangeMenu::Columns);
        assert_eq!(spec.map(|spec| spec.label), Some("Columns"));
        assert_eq!(
            spec.map(|spec| spec.rows.clone()),
            Some(column_rows(&columns))
        );
        let at = |menu| {
            rows.iter()
                .position(|row| matches!(row, MenuRow::Submenu(spec) if spec.menu == menu))
        };
        assert_eq!(
            at(ArrangeMenu::Columns),
            at(ArrangeMenu::Filter).map(|filter| filter + 1)
        );
        assert_eq!(rows.len(), plain.len() + 1, "nothing else changes");
        // A menu without the Filter group (a selection) is left alone.
        let selection = menu_rows(&target(2, false), &arrangement);
        assert_eq!(
            with_columns(selection.clone(), &arrangement, &columns),
            selection
        );
    }

    #[test]
    fn refreshing_a_menu_redraws_the_columns_checks() {
        let arrangement = Arrangement::default();
        let mut rows = with_columns(
            menu_rows(&target(0, false), &arrangement),
            &arrangement,
            &ColumnPref::defaults(),
        );
        refresh(&mut rows, &arrangement, &[ColumnPref::Group]);
        let columns = submenu_of(&rows, ArrangeMenu::Columns).map(|spec| spec.rows.clone());
        assert_eq!(columns.as_deref().map(checked_labels), Some(vec!["Group"]));
    }

    #[test]
    fn the_empty_space_menu_nests_sort_and_filter_as_submenus() {
        let arrangement = Arrangement::default();
        let rows = menu_rows(&target(0, false), &arrangement);
        let sort = submenu_of(&rows, ArrangeMenu::Sort);
        let filter = submenu_of(&rows, ArrangeMenu::Filter);
        assert_eq!(sort.map(|spec| spec.label), Some("Sort by"));
        assert_eq!(filter.map(|spec| spec.label), Some("Filter"));
        assert_eq!(
            sort.map(|spec| spec.rows.clone()),
            Some(sort_rows(&arrangement)),
            "the flyout holds the Sort menu's rows"
        );
        assert_eq!(
            filter.map(|spec| spec.rows.clone()),
            Some(filter_rows(&arrangement))
        );
        // The submenus sit between the paste group and the terminal rows, set
        // off by separators, and no flat Sort/Filter row remains.
        let at = |menu| {
            rows.iter()
                .position(|row| matches!(row, MenuRow::Submenu(spec) if spec.menu == menu))
        };
        assert_eq!(
            at(ArrangeMenu::Filter),
            at(ArrangeMenu::Sort).map(|sort| sort + 1)
        );
        // Select opens the group, right before Sort by.
        assert_eq!(
            at(ArrangeMenu::Select).map(|select| select + 1),
            at(ArrangeMenu::Sort)
        );
        assert!(matches!(
            at(ArrangeMenu::Select).and_then(|select| rows.get(select - 1)),
            Some(MenuRow::Separator)
        ));
        assert!(row(&rows, "Sort by").is_none() && row(&rows, "Filter").is_none());
        assert!(row(&rows, "New folder").is_some() && row(&rows, "Paste").is_some());
    }

    #[test]
    fn the_zoom_menu_offers_in_out_and_reset_with_their_keys() {
        use fileflux::core::zoom::ZoomCommand;
        let rows = zoom_rows();
        assert_eq!(labels(&rows), ["Zoom in", "Zoom out", "Reset zoom"]);
        let specs = items(&rows);
        let commands: Vec<_> = specs.iter().map(|spec| spec.command).collect();
        assert_eq!(
            commands,
            [
                MenuCommand::Zoom(ZoomCommand::In),
                MenuCommand::Zoom(ZoomCommand::Out),
                MenuCommand::Zoom(ZoomCommand::Reset),
            ]
        );
        let keys: Vec<_> = specs.iter().map(|spec| spec.shortcut).collect();
        assert_eq!(keys, [Some("Ctrl+="), Some("Ctrl+-"), Some("Ctrl+0")]);
        assert!(
            specs
                .iter()
                .all(|spec| spec.enabled && spec.checked.is_none())
        );
        assert_eq!(ArrangeMenu::Zoom.label(), "Zoom");
    }

    /// The folder's own menu gets Zoom after Filter in the grid and after
    /// Columns in the list; the other menus have none.
    #[test]
    fn zoom_follows_filter_in_the_grid_and_columns_in_the_list() {
        let arrangement = Arrangement::default();
        let base = menu_rows(&target(0, false), &arrangement);
        let order = |rows: &[MenuRow]| -> Vec<ArrangeMenu> {
            rows.iter()
                .filter_map(|row| match row {
                    MenuRow::Submenu(spec) => Some(spec.menu),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            order(&base),
            [ArrangeMenu::Select, ArrangeMenu::Sort, ArrangeMenu::Filter],
            "the base menu is untouched"
        );
        let grid = with_zoom(base.clone());
        assert_eq!(
            order(&grid),
            [
                ArrangeMenu::Select,
                ArrangeMenu::Sort,
                ArrangeMenu::Filter,
                ArrangeMenu::Zoom
            ]
        );
        let list = with_zoom(with_columns(base, &arrangement, &ColumnPref::defaults()));
        assert_eq!(
            order(&list),
            [
                ArrangeMenu::Select,
                ArrangeMenu::Sort,
                ArrangeMenu::Filter,
                ArrangeMenu::Columns,
                ArrangeMenu::Zoom
            ]
        );
        let flyout = submenu_of(&list, ArrangeMenu::Zoom).map(|spec| spec.rows.clone());
        assert_eq!(flyout, Some(zoom_rows()));
        // Refreshing the checks keeps the rows as they are.
        let mut refreshed = list.clone();
        refresh(&mut refreshed, &arrangement, &ColumnPref::defaults());
        assert_eq!(refreshed, list);
    }

    #[test]
    fn the_submenu_checks_follow_the_arrangement() {
        let mut arrangement = Arrangement::default();
        let mut rows = menu_rows(&target(0, false), &arrangement);
        arrangement.apply(ArrangeCommand::SortBy(SortKey::Name));
        arrangement.apply(ArrangeCommand::ToggleCategory(FileCategory::Images));
        arrangement.apply(ArrangeCommand::ToggleHidden);
        refresh(&mut rows, &arrangement, &[]);
        let sort = submenu_of(&rows, ArrangeMenu::Sort).map(|spec| checked_labels(&spec.rows));
        assert_eq!(
            sort,
            Some(vec![
                "Name",
                "Descending",
                "Folders first",
                "Natural number order"
            ])
        );
        let filter = submenu_of(&rows, ArrangeMenu::Filter).map(|spec| checked_labels(&spec.rows));
        assert_eq!(filter, Some(vec!["Show hidden files", "Images"]));
    }

    #[test]
    fn entry_and_trash_menus_are_unchanged() {
        let arrangement = Arrangement::default();
        for selected in [1, 3] {
            let rows = menu_rows(&target(selected, false), &arrangement);
            assert!(
                rows.iter().all(|row| !matches!(row, MenuRow::Submenu(_))),
                "{selected} selected"
            );
        }
        let rows = menu_rows(&target(2, true), &arrangement);
        assert!(rows.iter().all(|row| !matches!(row, MenuRow::Submenu(_))));
        // The Trash's empty space only gains Select, never Sort or Filter.
        let rows = menu_rows(&target(0, true), &arrangement);
        assert!(
            rows.iter().all(
                |row| !matches!(row, MenuRow::Submenu(spec) if spec.menu != ArrangeMenu::Select)
            )
        );
    }
}
