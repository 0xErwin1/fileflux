//! The context menu as a Bolt cascade: the rows become [`MenuNode`]s, and
//! [`MenuState`] keeps the cursor path and the hover intent that
//! `bolt::composites::render_menu_tree` draws from.
//!
//! The rows stay the model (`MenuRow`); the nodes are rebuilt from them on
//! every draw, so a check mark always reflects the rows' current state. A
//! node never carries an id: an activation reports its path, which
//! [`MenuState::item_at`] maps back to the row.

use std::time::Instant;

use bolt::composites::{
    BackAction, ChildAction, HoverIntent, HoverTick, MenuItem, MenuNav, MenuNode,
};

use super::actions::MenuState;
use super::icon;
use super::ops::{ArrangeMenu, MenuCommand, MenuItemSpec, MenuRow};
use fileflux::core::zoom::ZoomCommand;

/// The rows the target title takes at the top of the root level: the header
/// and the separator under it.
const TITLE_ROWS: usize = 2;

fn command_icon(command: MenuCommand) -> Option<gpui_kit_assets::IconName> {
    use gpui_kit_assets::IconName;
    Some(match command {
        // Sort and filter choices show their check mark instead.
        MenuCommand::Arrange(_) | MenuCommand::ToggleColumn(_) => return None,
        MenuCommand::Zoom(ZoomCommand::In) => IconName::ZoomIn,
        MenuCommand::Zoom(ZoomCommand::Out) => IconName::ZoomOut,
        MenuCommand::Zoom(ZoomCommand::Reset) | MenuCommand::ResetFolderView => IconName::RotateCcw,
        MenuCommand::AddFavorite => IconName::Star,
        MenuCommand::RemoveFavorite => IconName::StarFill,
        MenuCommand::Open | MenuCommand::OpenWith => IconName::FolderOpen,
        MenuCommand::OpenInViewer => IconName::Image,
        MenuCommand::OpenInNewTab => IconName::ExternalLink,
        MenuCommand::OpenContainingFolder => IconName::FolderOpen,
        MenuCommand::Rename => IconName::Pencil,
        MenuCommand::CopyPath | MenuCommand::CopyFolderPath => IconName::Link,
        MenuCommand::Properties => IconName::Info,
        MenuCommand::MoveToTrash | MenuCommand::DeletePermanently | MenuCommand::EmptyTrash => {
            IconName::Trash
        }
        MenuCommand::SelectAll | MenuCommand::InvertSelection | MenuCommand::SelectMatching => {
            return None;
        }
        MenuCommand::NewFolder => IconName::FolderPlus,
        MenuCommand::NewFile => IconName::FilePlus,
        MenuCommand::OpenTerminal => IconName::Terminal,
        MenuCommand::Restore => IconName::Undo2,
        MenuCommand::Cut => IconName::Scissors,
        MenuCommand::Copy => IconName::Copy,
        MenuCommand::Paste | MenuCommand::PasteIntoFolder => IconName::ClipboardPaste,
        MenuCommand::MakeLink | MenuCommand::PasteAsLink => IconName::Link2,
        MenuCommand::Compress => IconName::Archive,
        MenuCommand::ExtractHere | MenuCommand::ExtractTo => IconName::PackageOpen,
        MenuCommand::Mount => IconName::HardDrive,
        MenuCommand::Unlock => IconName::Lock,
        MenuCommand::ConnectToServer | MenuCommand::ConnectBookmark => IconName::Network,
        MenuCommand::RemoveBookmark => IconName::StarFill,
        MenuCommand::Unmount | MenuCommand::Eject => IconName::Eject,
    })
}

fn submenu_icon(menu: ArrangeMenu) -> gpui_kit_assets::IconName {
    match menu {
        ArrangeMenu::Sort => gpui_kit_assets::IconName::ArrowUpDown,
        ArrangeMenu::Filter => gpui_kit_assets::IconName::ListFilter,
        ArrangeMenu::Select => gpui_kit_assets::IconName::SquareCheck,
        ArrangeMenu::Columns => gpui_kit_assets::IconName::Table,
        ArrangeMenu::Zoom => gpui_kit_assets::IconName::ZoomIn,
    }
}

fn item_node(spec: &MenuItemSpec) -> MenuNode {
    let mut item = MenuItem::new(spec.label);
    if let Some(name) = command_icon(spec.command) {
        item = item.icon(icon(name));
    }
    if let Some(checked) = spec.checked {
        item = item.checked(checked);
    }
    if let Some(shortcut) = spec.shortcut {
        item = item.shortcut(shortcut);
    }
    if spec.danger {
        item = item.danger();
    }
    if !spec.enabled {
        item = item.disabled();
    }
    let node = MenuNode::new(item);
    // A sort or filter choice keeps the menu open and redraws its checks.
    if matches!(
        spec.command,
        MenuCommand::Arrange(_) | MenuCommand::ToggleColumn(_)
    ) {
        node.keep_open(true)
    } else {
        node
    }
}

/// One node per row, in order, so a node's path is its row's path.
fn row_nodes(rows: &[MenuRow]) -> Vec<MenuNode> {
    rows.iter()
        .map(|row| match row {
            MenuRow::Separator => MenuNode::separator(),
            MenuRow::Header(label) => MenuNode::header(*label),
            MenuRow::Item(spec) => item_node(spec),
            MenuRow::TemplateFile { label, .. } => MenuNode::new(MenuItem::new(label.clone())),
            MenuRow::TemplateFolder { label, rows } => MenuNode::row(label.clone())
                .icon(icon(gpui_kit_assets::IconName::FilePlus))
                .children(row_nodes(rows)),
            MenuRow::Custom { label, .. } => MenuNode::new(MenuItem::new(label.clone())),
            MenuRow::CustomFolder { label, rows } => MenuNode::row(label.clone())
                .icon(icon(gpui_kit_assets::IconName::Terminal))
                .children(row_nodes(rows)),
            MenuRow::Submenu(spec) => MenuNode::row(spec.label)
                .icon(icon(submenu_icon(spec.menu)))
                .children(row_nodes(&spec.rows)),
        })
        .collect()
}

/// The root level of a menu: the target `title` (when there is one) above
/// `rows`. The title is a header, so the cursor never rests on it.
pub(crate) fn nodes(title: Option<&str>, rows: &[MenuRow]) -> Vec<MenuNode> {
    let mut nodes = Vec::with_capacity(rows.len() + TITLE_ROWS);
    if let Some(title) = title {
        nodes.push(MenuNode::header(title.to_owned()));
        nodes.push(MenuNode::separator());
    }
    nodes.extend(row_nodes(rows));
    nodes
}

/// The item `path` points at, following submenu rows down from `rows`.
/// `titled`: the root level starts with the title header and its separator.
fn row_in<'a>(rows: &'a [MenuRow], titled: bool, path: &[usize]) -> Option<&'a MenuRow> {
    let (last, parents) = path.split_last()?;
    let skip = |at: usize, level: usize| {
        if level == 0 && titled {
            at.checked_sub(TITLE_ROWS)
        } else {
            Some(at)
        }
    };
    let mut level = rows;
    for (depth, at) in parents.iter().enumerate() {
        level = match level.get(skip(*at, depth)?)? {
            MenuRow::Submenu(spec) => &spec.rows,
            MenuRow::TemplateFolder { rows, .. } | MenuRow::CustomFolder { rows, .. } => rows,
            _ => return None,
        };
    }
    level.get(skip(*last, parents.len())?)
}

impl MenuState {
    /// A menu open on its first selectable row.
    pub(crate) fn new(
        on: super::actions::MenuOn,
        title: Option<String>,
        position: gpui::Point<gpui::Pixels>,
        rows: Vec<MenuRow>,
    ) -> Self {
        let mut menu = Self {
            on,
            title,
            position,
            rows,
            nav: MenuNav::new(),
            hover: HoverIntent::new(),
            keyboard: false,
        };
        let root = menu.nodes();
        menu.nav.reset(&root);
        menu
    }

    /// The cascade to draw, from the rows as they are now.
    pub(crate) fn nodes(&self) -> Vec<MenuNode> {
        nodes(self.title.as_deref(), &self.rows)
    }

    /// The item an activation `path` names; `None` for a submenu row, a
    /// header or a separator.
    pub(crate) fn item_at(&self, path: &[usize]) -> Option<&MenuItemSpec> {
        match row_in(&self.rows, self.title.is_some(), path)? {
            MenuRow::Item(spec) => Some(spec),
            _ => None,
        }
    }

    /// The template file an activation `path` names.
    pub(crate) fn template_at(&self, path: &[usize]) -> Option<&std::path::Path> {
        match row_in(&self.rows, self.title.is_some(), path)? {
            MenuRow::TemplateFile { path, .. } => Some(path),
            _ => None,
        }
    }

    /// The custom action an activation `path` names, by its index.
    pub(crate) fn custom_action_at(&self, path: &[usize]) -> Option<usize> {
        match row_in(&self.rows, self.title.is_some(), path)? {
            MenuRow::Custom { index, .. } => Some(*index),
            _ => None,
        }
    }

    /// Up/Down: moves the highlight within the deepest open level, wrapping.
    pub(crate) fn step(&mut self, forward: bool) -> bool {
        self.keyboard = true;
        let root = self.nodes();
        self.nav.step(&root, forward)
    }

    /// Right or Enter on the highlighted row.
    pub(crate) fn open_child(&mut self) -> ChildAction {
        self.keyboard = true;
        let root = self.nodes();
        let action = self.nav.open_child(&root);
        if action == ChildAction::Opened {
            // Resting the pointer on the owning row afterwards must not
            // reset the flyout's own highlight.
            let depth = self.nav.cursor().depth();
            if let Some(owner) = depth
                .checked_sub(2)
                .and_then(|level| Some((level, self.nav.cursor().highlight(level)?)))
            {
                self.hover.mark_open(owner.0, Some(owner.1));
            }
        }
        action
    }

    /// Left or Escape: closes the deepest open level.
    pub(crate) fn back(&mut self) -> BackAction {
        self.keyboard = true;
        let action = self.nav.back();
        if action == BackAction::ClosedLevel {
            let depth = self.nav.cursor().depth();
            self.hover.mark_open(depth.saturating_sub(1), None);
        }
        action
    }

    /// A submenu row was clicked: opens its flyout, keeping the rest of the
    /// cascade.
    pub(crate) fn open_flyout(&mut self, level: usize, index: usize) -> bool {
        self.keyboard = false;
        let root = self.nodes();
        let moved = self.nav.hover(level, index, &root);
        if moved {
            self.hover.mark_open(level, Some(index));
        }
        moved
    }

    /// The pointer entered `index` of `level`: the highlight follows it, and
    /// the flyout hanging off a different row of the level closes. Resting on
    /// the row that owns the open flyout leaves the flyout fixed. Returns
    /// whether the cascade changed.
    pub(crate) fn hover_row(&mut self, level: usize, index: usize, now: Instant) -> bool {
        self.keyboard = false;
        let root = self.nodes();
        let depth_before = self.nav.cursor().depth();
        let moved = if self.nav.cursor().highlight(level) == Some(index) {
            false
        } else {
            self.nav.set_level(level, index, &root)
        };
        if self.nav.cursor().depth() < depth_before {
            // The flyout of the old row closed: the open record must not keep
            // claiming it is open.
            self.hover.mark_open(level, None);
        }
        self.hover.enter(level, index, now);
        moved
    }

    /// One open-on-hover decision: opens the flyout of the row the pointer
    /// rested on for the open delay. Returns whether the cascade changed.
    pub(crate) fn hover_tick(&mut self, now: Instant) -> bool {
        match self.hover.tick(now) {
            HoverTick::Open(level, index) => {
                let root = self.nodes();
                let before = self.nav.cursor().clone();
                self.nav.hover(level, index, &root);
                self.hover.mark_open(level, Some(index));
                self.nav.cursor() != &before
            }
            // The cascade reports no pointer-left signal: a level closes when
            // another row of its parent is hovered (see `hover_row`).
            HoverTick::CloseLevel(_) | HoverTick::Nothing => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;
    use std::time::Duration;

    use bolt::composites::HOVER_OPEN_DELAY;
    use fileflux::core::kinds::FileCategory;
    use gpui::{point, px};

    use super::*;
    use crate::explorer::actions::MenuOn;
    use crate::explorer::arrange::{ArrangeCommand, Arrangement, SortKey};
    use crate::explorer::arrange_menu;
    use crate::explorer::ops::{MenuTarget, menu_rows};

    fn at(millis: u64) -> Instant {
        static ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);
        *ORIGIN + Duration::from_millis(millis)
    }

    fn open_delay() -> u64 {
        u64::try_from(HOVER_OPEN_DELAY.as_millis()).unwrap_or(u64::MAX)
    }

    fn empty_space(arrangement: &Arrangement) -> MenuState {
        let target = MenuTarget {
            in_trash: false,
            selected: 0,
            folder_is_empty: false,
            terminal: true,
            can_paste: true,
            folder_selected: false,
            single_file: false,
            folder_favorite: false,
        };
        MenuState::new(
            MenuOn::EmptySpace,
            Some("Home (this folder)".to_owned()),
            point(px(10.0), px(10.0)),
            menu_rows(&target, arrangement),
        )
    }

    fn labelled(nodes: &[MenuNode], label: &str) -> Option<usize> {
        nodes
            .iter()
            .position(|node| node.item.label.as_ref() == label)
    }

    fn checked(nodes: &[MenuNode]) -> Vec<String> {
        nodes
            .iter()
            .filter(|node| node.item.checked == Some(true))
            .map(|node| node.item.label.to_string())
            .collect()
    }

    #[test]
    fn template_rows_cascade_and_activate_by_path() {
        let templates = vec![
            fileflux::core::templates::Template::Folder {
                label: "Work".to_owned(),
                children: vec![fileflux::core::templates::Template::File {
                    label: "Contract".to_owned(),
                    path: "/t/Work/Contract.odt".into(),
                }],
            },
            fileflux::core::templates::Template::File {
                label: "Letter".to_owned(),
                path: "/t/Letter.odt".into(),
            },
        ];
        let mut menu = empty_space(&Arrangement::default());
        crate::explorer::templates::add_rows(&mut menu.rows, &templates);
        let root = menu.nodes();
        let at = labelled(&root, "New from template");
        assert!(
            at.and_then(|at| root.get(at))
                .is_some_and(MenuNode::opens_flyout)
        );
        assert!(at.is_some(), "no template row");
        let at = at.unwrap_or_default();
        // Title header and separator come first.
        assert_eq!(
            menu.template_at(&[at, 1]),
            Some(std::path::Path::new("/t/Letter.odt"))
        );
        assert_eq!(
            menu.template_at(&[at, 0, 0]),
            Some(std::path::Path::new("/t/Work/Contract.odt"))
        );
        assert_eq!(
            menu.template_at(&[at, 0]),
            None,
            "a folder row is no template"
        );
        assert!(menu.item_at(&[at, 1]).is_none());
        assert!(menu.item_at(&[2]).is_some(), "New folder still resolves");
    }

    #[test]
    fn sort_and_filter_nest_in_the_empty_space_menu() {
        let menu = empty_space(&Arrangement::default());
        let root = menu.nodes();
        let sort = labelled(&root, "Sort by").and_then(|at| root.get(at));
        let filter = labelled(&root, "Filter").and_then(|at| root.get(at));
        assert!(sort.is_some_and(MenuNode::opens_flyout));
        assert!(filter.is_some_and(MenuNode::opens_flyout));
        assert!(sort.is_some_and(|node| node.item.has_submenu));
        assert_eq!(sort.map(|node| node.children.len()), Some(14));
        // Select nests too, and no other row does; no "Back" row exists.
        let nesting = root.iter().filter(|node| node.opens_flyout()).count();
        assert_eq!(nesting, 3);
        assert!(
            labelled(&root, "Select")
                .is_some_and(|at| root.get(at).is_some_and(MenuNode::opens_flyout))
        );
        assert!(labelled(&root, "Back").is_none());
        assert!(
            sort.is_some_and(|node| labelled(&node.children, "Back").is_none()),
            "a flyout has no way-back row"
        );
        // The title heads the root level and takes no part in the cursor.
        assert!(root.first().is_some_and(|node| node.item.is_header));
        assert_eq!(menu.nav.cursor().path(), [2]);
    }

    #[test]
    fn the_flyout_checks_reflect_the_arrangement() {
        let mut arrangement = Arrangement::default();
        let mut menu = empty_space(&arrangement);
        let root = menu.nodes();
        let sort = labelled(&root, "Sort by").and_then(|at| root.get(at));
        assert_eq!(
            sort.map(|node| checked(&node.children)),
            Some(vec![
                "Modified".to_owned(),
                "Descending".to_owned(),
                "Folders first".to_owned(),
                "Natural number order".to_owned()
            ])
        );

        arrangement.apply(ArrangeCommand::SortBy(SortKey::Name));
        arrangement.apply(ArrangeCommand::ToggleCategory(FileCategory::Images));
        arrange_menu::refresh(&mut menu.rows, &arrangement, &[]);
        let root = menu.nodes();
        let sort = labelled(&root, "Sort by").and_then(|at| root.get(at));
        let filter = labelled(&root, "Filter").and_then(|at| root.get(at));
        assert_eq!(
            sort.map(|node| checked(&node.children))
                .and_then(|c| c.first().cloned()),
            Some("Name".to_owned())
        );
        assert_eq!(
            filter.map(|node| checked(&node.children)),
            Some(vec!["Images".to_owned()]),
            "hidden files are hidden by default: only the Images toggle is checked"
        );
        assert!(
            sort.is_some_and(|node| node
                .children
                .iter()
                .filter(|child| child.item.checked.is_some())
                .all(MenuNode::keeps_menu_open)),
            "checkable rows keep the menu open"
        );
    }

    #[test]
    fn an_activation_path_names_the_row_it_came_from() {
        let menu = empty_space(&Arrangement::default());
        let root = menu.nodes();
        let paste = labelled(&root, "Paste").unwrap_or_default();
        assert_eq!(
            menu.item_at(&[paste]).map(|spec| spec.command),
            Some(MenuCommand::Paste)
        );
        let sort = labelled(&root, "Sort by").unwrap_or_default();
        let flyout = root
            .get(sort)
            .map(|node| node.children.clone())
            .unwrap_or_default();
        let name = labelled(&flyout, "Name").unwrap_or_default();
        assert_eq!(
            menu.item_at(&[sort, name]).map(|spec| spec.command),
            Some(MenuCommand::Arrange(ArrangeCommand::SortBy(SortKey::Name)))
        );
        // The submenu row itself, the title and the separator are no item.
        assert!(menu.item_at(&[sort]).is_none());
        assert!(menu.item_at(&[0]).is_none());
        assert!(menu.item_at(&[1]).is_none());
        assert!(menu.item_at(&[]).is_none());
    }

    #[test]
    fn right_opens_the_flyout_left_and_escape_close_one_level() {
        let mut menu = empty_space(&Arrangement::default());
        let root = menu.nodes();
        let sort = labelled(&root, "Sort by").unwrap_or_default();
        // Down from "New folder" (first row) to "Sort by".
        while menu.nav.cursor().highlight(0) != Some(sort) {
            assert!(menu.step(true));
        }
        assert_eq!(menu.open_child(), ChildAction::Opened);
        assert_eq!(menu.nav.cursor().depth(), 2);
        assert!(menu.keyboard);
        // The flyout's first selectable row is highlighted (past the header).
        assert_eq!(menu.nav.cursor().highlight(1), Some(1));
        // A leaf row activates instead of opening.
        assert_eq!(menu.open_child(), ChildAction::Activate);
        assert_eq!(menu.back(), BackAction::ClosedLevel);
        assert_eq!(menu.nav.cursor().path(), [sort]);
        assert_eq!(menu.back(), BackAction::Dismissed);
    }

    #[test]
    fn hovering_a_submenu_row_opens_its_flyout_after_the_delay() {
        let mut menu = empty_space(&Arrangement::default());
        let root = menu.nodes();
        let sort = labelled(&root, "Sort by").unwrap_or_default();
        let filter = labelled(&root, "Filter").unwrap_or_default();
        assert!(menu.hover_row(0, sort, at(0)));
        assert!(!menu.hover_tick(at(open_delay() - 1)), "not yet");
        assert!(menu.hover_tick(at(open_delay())));
        assert_eq!(menu.nav.cursor().depth(), 2);
        assert!(!menu.keyboard);
        // Hovering the owning row leaves the flyout fixed.
        assert!(!menu.hover_row(0, sort, at(open_delay() + 10)));
        assert_eq!(menu.nav.cursor().depth(), 2);
        // Another submenu row switches the cascade to it.
        assert!(menu.hover_row(0, filter, at(open_delay() + 20)));
        assert_eq!(menu.nav.cursor().depth(), 1);
        assert!(menu.hover_tick(at(2 * open_delay() + 20)));
        assert_eq!(menu.nav.cursor().path().first(), Some(&filter));
        assert_eq!(menu.nav.cursor().depth(), 2);
    }

    #[test]
    fn the_subbar_menu_is_the_flat_rows_with_no_title() {
        let arrangement = Arrangement::default();
        let menu = MenuState::new(
            MenuOn::Arrange(ArrangeMenu::Filter),
            None,
            point(px(0.0), px(0.0)),
            arrange_menu::rows(ArrangeMenu::Filter, &arrangement, &[]),
        );
        let root = menu.nodes();
        assert!(root.iter().all(|node| !node.opens_flyout()));
        assert!(!root.first().is_some_and(|node| node.item.is_header));
        assert_eq!(
            menu.item_at(&[0]).map(|spec| spec.label),
            Some("Show hidden files")
        );
        // Arrow keys skip the separator, headers and the disabled "Clear".
        let mut menu = menu;
        assert!(menu.step(true));
        assert_eq!(
            menu.nav.cursor().path(),
            [4],
            "past Clear, the separator and Type"
        );
    }
}
