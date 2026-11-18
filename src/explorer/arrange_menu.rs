//! The Sort and Filter menus as data: the rows the subbar buttons open and
//! the empty-space context menu drills into. Both draw from the one
//! [`Arrangement`], and every row's command goes through
//! [`Arrangement::apply`], so the menus, the chips, Ctrl+H and the list
//! header can never disagree.

use fileflux::core::kinds::FileCategory;

use super::arrange::{ArrangeCommand, Arrangement, DateRange, SizeRange, SortDir, SortKey};
use super::ops::{MenuCommand, MenuItemSpec, MenuPage, MenuRow};

/// The shortcut hint of the hidden-files row.
pub(crate) const HIDDEN_SHORTCUT: &str = "Ctrl+H";

fn choice(command: ArrangeCommand, label: &'static str, checked: bool) -> MenuRow {
    MenuRow::Item(MenuItemSpec {
        checked: Some(checked),
        ..MenuItemSpec::new(MenuCommand::Arrange(command), label)
    })
}

/// The row that returns from a drilled-in page to the context menu.
fn back_rows() -> [MenuRow; 2] {
    [
        MenuRow::Item(MenuItemSpec::new(
            MenuCommand::Page(MenuPage::Context),
            "Back",
        )),
        MenuRow::Separator,
    ]
}

/// The rows of the Sort page. `back` adds the way back to the context menu.
pub(crate) fn sort_rows(arrangement: &Arrangement, back: bool) -> Vec<MenuRow> {
    let sort = arrangement.sort;
    let mut rows = Vec::new();
    if back {
        rows.extend(back_rows());
    }
    rows.push(MenuRow::Header("Sort by"));
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

/// The rows of the Filter page: the hidden-files toggle and "Clear all" on
/// top, where a short window never scrolls them out of reach, then the
/// three groups. `back` adds the way back to the context menu.
pub(crate) fn filter_rows(arrangement: &Arrangement, back: bool) -> Vec<MenuRow> {
    let filters = &arrangement.filters;
    let mut rows = Vec::new();
    if back {
        rows.extend(back_rows());
    }
    rows.push(MenuRow::Item(MenuItemSpec {
        shortcut: Some(HIDDEN_SHORTCUT),
        checked: Some(!filters.hide_hidden),
        ..MenuItemSpec::new(
            MenuCommand::Arrange(ArrangeCommand::ToggleHidden),
            "Show hidden files",
        )
    }));
    rows.push(MenuRow::Item(MenuItemSpec {
        enabled: !filters.is_default(),
        ..MenuItemSpec::new(
            MenuCommand::Arrange(ArrangeCommand::ClearFilters),
            "Clear all filters",
        )
    }));
    rows.push(MenuRow::Separator);
    rows.push(MenuRow::Header("Type"));
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

/// The rows of `page` for `arrangement`; the context page is not built
/// here (it depends on the selection), so it yields nothing.
pub(crate) fn page_rows(page: MenuPage, arrangement: &Arrangement, back: bool) -> Vec<MenuRow> {
    match page {
        MenuPage::Sort => sort_rows(arrangement, back),
        MenuPage::Filter => filter_rows(arrangement, back),
        MenuPage::Context => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::ops::{MenuTarget, menu_rows, next_menu_index};

    fn items(rows: &[MenuRow]) -> Vec<&MenuItemSpec> {
        rows.iter()
            .filter_map(|row| match row {
                MenuRow::Item(spec) => Some(spec),
                MenuRow::Header(_) | MenuRow::Separator => None,
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

    fn target(selected: usize, in_trash: bool) -> MenuTarget {
        MenuTarget {
            in_trash,
            selected,
            folder_is_empty: false,
            terminal: true,
            can_paste: true,
            folder_selected: false,
            single_file: selected == 1,
        }
    }

    #[test]
    fn the_sort_page_checks_the_active_key_direction_and_toggles() {
        let mut arrangement = Arrangement::default();
        let rows = sort_rows(&arrangement, false);
        assert_eq!(
            labels(&rows),
            [
                "Name",
                "Size",
                "Modified",
                "Type",
                "Ascending",
                "Descending",
                "Folders first",
                "Natural number order"
            ]
        );
        assert_eq!(
            checked_labels(&rows),
            ["Name", "Ascending", "Folders first", "Natural number order"]
        );
        assert!(matches!(rows.first(), Some(MenuRow::Header("Sort by"))));

        arrangement.apply(ArrangeCommand::SortBy(SortKey::Type));
        arrangement.apply(ArrangeCommand::SetDir(SortDir::Descending));
        arrangement.apply(ArrangeCommand::ToggleFoldersFirst);
        arrangement.apply(ArrangeCommand::ToggleNatural);
        let rows = sort_rows(&arrangement, false);
        assert_eq!(checked_labels(&rows), ["Type", "Descending"]);
    }

    #[test]
    fn the_filter_page_lists_the_four_groups_with_their_check_states() {
        let mut arrangement = Arrangement::default();
        let rows = filter_rows(&arrangement, false);
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
            ["Show hidden files"],
            "shown by default"
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
        let rows = filter_rows(&arrangement, false);
        assert_eq!(
            checked_labels(&rows),
            ["Images", "Code", "Last 7 days", "> 1 GB"]
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
        for rows in [
            sort_rows(&arrangement, true),
            filter_rows(&arrangement, true),
        ] {
            for spec in items(&rows) {
                match spec.command {
                    MenuCommand::Arrange(command) => commands.push(command),
                    MenuCommand::Page(MenuPage::Context) => {}
                    other => unexpected.push((spec.label, other)),
                }
            }
        }
        assert!(
            unexpected.is_empty(),
            "not sort/filter commands: {unexpected:?}"
        );
        assert_eq!(commands.len(), 8 + 8 + 5 + 4 + 2);
        for command in commands {
            let mut changed = Arrangement::default();
            changed.apply(command);
            // Already-default choices and "clear" have nothing to change.
            let is_default_choice = matches!(
                command,
                ArrangeCommand::SortBy(SortKey::Name)
                    | ArrangeCommand::SetDir(SortDir::Ascending)
                    | ArrangeCommand::ClearFilters
            );
            if !is_default_choice {
                assert_ne!(changed, arrangement, "{command:?} changes nothing");
            }
        }
    }

    #[test]
    fn the_empty_space_menu_opens_sort_and_filter_pages() {
        let rows = menu_rows(&target(0, false));
        let sort = row(&rows, "Sort by");
        let filter = row(&rows, "Filter");
        assert_eq!(
            sort.map(|spec| (spec.command, spec.submenu, spec.enabled)),
            Some((MenuCommand::Page(MenuPage::Sort), true, true))
        );
        assert_eq!(
            filter.map(|spec| (spec.command, spec.submenu, spec.enabled)),
            Some((MenuCommand::Page(MenuPage::Filter), true, true))
        );
        // The pages sit between the paste group and the terminal rows, set
        // off by separators.
        let at = |label| {
            rows.iter()
                .position(|row| matches!(row, MenuRow::Item(spec) if spec.label == label))
        };
        assert_eq!(at("Filter"), at("Sort by").map(|sort| sort + 1));
        assert!(matches!(
            at("Sort by").and_then(|sort| rows.get(sort - 1)),
            Some(MenuRow::Separator)
        ));
        assert!(row(&rows, "New folder").is_some() && row(&rows, "Paste").is_some());
    }

    #[test]
    fn entry_and_trash_menus_are_unchanged() {
        for selected in [1, 3] {
            let rows = menu_rows(&target(selected, false));
            assert!(row(&rows, "Sort by").is_none(), "{selected} selected");
            assert!(row(&rows, "Filter").is_none(), "{selected} selected");
        }
        for selected in [0, 2] {
            let rows = menu_rows(&target(selected, true));
            assert!(
                row(&rows, "Sort by").is_none(),
                "trash, {selected} selected"
            );
        }
    }

    #[test]
    fn the_drilled_in_pages_offer_the_way_back_first() {
        let arrangement = Arrangement::default();
        for page in [MenuPage::Sort, MenuPage::Filter] {
            let rows = page_rows(page, &arrangement, true);
            let first = items(&rows).into_iter().next();
            assert_eq!(
                first.map(|spec| (spec.label, spec.command)),
                Some(("Back", MenuCommand::Page(MenuPage::Context)))
            );
            let direct = page_rows(page, &arrangement, false);
            assert!(
                row(&direct, "Back").is_none(),
                "the subbar menus have no back row"
            );
            assert_eq!(items(&direct).len() + 1, items(&rows).len());
        }
        assert!(page_rows(MenuPage::Context, &arrangement, false).is_empty());
    }

    #[test]
    fn arrow_keys_skip_headers_and_disabled_rows() {
        let arrangement = Arrangement::default();
        let rows = filter_rows(&arrangement, false);
        // Down from nothing lands on the first item; "Clear all filters" is
        // disabled and skipped on the way down.
        let first = next_menu_index(&rows, None, true);
        assert_eq!(first, Some(0));
        assert_eq!(
            next_menu_index(&rows, first, true),
            Some(4),
            "past the separator and the Type header"
        );
        // Up from nothing wraps to the last row.
        let last = next_menu_index(&rows, None, false);
        assert_eq!(
            last.and_then(|at| rows.get(at)).map(|row| match row {
                MenuRow::Item(spec) => spec.label,
                _ => "",
            }),
            Some("> 1 GB")
        );
    }
}
