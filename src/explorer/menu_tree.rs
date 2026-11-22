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

use super::actions::{MenuOn, MenuState};
use super::icon;
use super::ops::{ArrangeMenu, MenuCommand, MenuItemSpec, MenuRow, SearchOption};
use fileflux::core::zoom::ZoomCommand;

/// What the title of a menu in new tab mode says after the target's name.
const NEW_TAB_SUFFIX: &str = " \u{b7} in new tab";

/// Where a menu command shows the location it navigates to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shown {
    /// In the shown tab, as the command always did.
    Here,
    /// In a new background tab (a menu opened with Shift held).
    NewTab,
}

/// Whether `command`, run from a menu opened `on` something, changes the
/// shown location. Only these commands open in a new tab under Shift; the
/// rest behave as without it. "Open in new tab" and "Open in split" already
/// choose their own destination, and the entries' and toolbar's Properties
/// toggle the details panel without leaving the folder.
fn command_navigates(on: &MenuOn, command: MenuCommand) -> bool {
    match command {
        MenuCommand::Open
        | MenuCommand::OpenContainingFolder
        | MenuCommand::AnalyzeDiskUsage
        | MenuCommand::Mount
        // Both open the folder they reach once they finish.
        | MenuCommand::Unlock
        | MenuCommand::ConnectBookmark => true,
        // The sidebar and device menus show the place, then the panel.
        MenuCommand::Properties => {
            matches!(on, MenuOn::Sidebar { .. } | MenuOn::DeviceMenu { .. })
        }
        _ => false,
    }
}

/// The one decision behind Shift+right-click: a menu in new tab mode
/// (`new_tab`) shows what a navigating command opens in a new tab.
pub(crate) fn shown_for(new_tab: bool, on: &MenuOn, command: MenuCommand) -> Shown {
    if new_tab && command_navigates(on, command) {
        Shown::NewTab
    } else {
        Shown::Here
    }
}

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
        MenuCommand::RenameFavorite => IconName::Pencil,
        MenuCommand::MoveFavoriteUp => IconName::ArrowUp,
        MenuCommand::MoveFavoriteDown => IconName::ArrowDown,
        MenuCommand::Open | MenuCommand::OpenWith => IconName::FolderOpen,
        MenuCommand::OpenInViewer => IconName::Image,
        MenuCommand::Launch => IconName::Play,
        MenuCommand::OpenInNewTab => IconName::ExternalLink,
        MenuCommand::OpenInSplit => IconName::Columns2,
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
        MenuCommand::AnalyzeDiskUsage => IconName::ChartPie,
        MenuCommand::SearchOption(SearchOption::MatchCase) => IconName::CaseSensitive,
        MenuCommand::SearchOption(SearchOption::Regex) => IconName::Regex,
        MenuCommand::SearchOption(SearchOption::Contents) => IconName::FileText,
        MenuCommand::SearchOption(SearchOption::Subfolders) => IconName::FolderSearch,
        // The toolbar's ⋯ rows show their shortcut only, as the artboard
        // draws them.
        MenuCommand::SplitView | MenuCommand::FavoriteFolder | MenuCommand::Reload => {
            return None;
        }
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
    // A sort, filter or search-option choice keeps the menu open: several
    // can be ticked in a row, and the checks redraw.
    if matches!(
        spec.command,
        MenuCommand::Arrange(_) | MenuCommand::ToggleColumn(_) | MenuCommand::SearchOption(_)
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

/// Whether a menu that opens now is in new tab mode: only a menu opened by
/// a right mouse press with Shift held. A menu opened from the keyboard (a
/// Shift chord) or by a left click on a button (the toolbar, a device) never
/// is, whatever Shift says.
pub(crate) const fn opens_in_new_tab(right_press: bool, shift: bool) -> bool {
    right_press && shift
}

/// Whether a right mouse press is being dispatched. The window sets it in a
/// capture handler ahead of every handler of the press ([`Self::press`]) and
/// clears it as soon as the press is done ([`Self::settle`]), so a menu the
/// press opens knows the mouse opened it, and a menu a later key opens does
/// not inherit that.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RightPress {
    dispatching: bool,
}

impl RightPress {
    /// A mouse press of `button` reached the window. Returns whether the
    /// caller must [`Self::settle`] once the press's handlers have run.
    pub(crate) fn press(&mut self, button: gpui::MouseButton) -> bool {
        self.dispatching = button == gpui::MouseButton::Right;
        self.dispatching
    }

    /// The press's handlers have run.
    pub(crate) fn settle(&mut self) {
        self.dispatching = false;
    }

    /// Whether a menu opened now is in new tab mode (see
    /// [`opens_in_new_tab`]).
    pub(crate) const fn opens_in_new_tab(self, shift: bool) -> bool {
        opens_in_new_tab(self.dispatching, shift)
    }
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
            new_tab: false,
        };
        let root = menu.nodes();
        menu.nav.reset(&root);
        menu
    }

    /// Puts the menu in new tab mode when `new_tab` (what
    /// [`opens_in_new_tab`] decided): its navigating commands open background
    /// tabs, and the title says so.
    pub(crate) fn in_new_tab(mut self, new_tab: bool) -> Self {
        self.new_tab = new_tab;
        if new_tab && let Some(title) = self.title.as_mut() {
            title.push_str(NEW_TAB_SUFFIX);
        }
        let root = self.nodes();
        self.nav.reset(&root);
        self
    }

    /// The cascade to draw, from the rows as they are now.
    pub(crate) fn nodes(&self) -> Vec<MenuNode> {
        let mut root = nodes(self.title.as_deref(), &self.rows);
        self.draw_partitions(&mut root);
        root
    }

    /// A device menu's partition rows are [`MenuRow::Custom`] rows that
    /// carry the partition's name only: the mount point and the free space
    /// the menu was opened with (see [`super::actions::MenuOn::DeviceMenu`])
    /// draw as the row's detail line and mono hint. The menu carries no
    /// title, so its rows and nodes line up one to one.
    fn draw_partitions(&self, root: &mut [MenuNode]) {
        let super::actions::MenuOn::DeviceMenu { partitions, .. } = &self.on else {
            return;
        };
        if self.title.is_some() {
            return;
        }
        for (node, row) in root.iter_mut().zip(&self.rows) {
            let MenuRow::Custom { index, .. } = row else {
                continue;
            };
            let Some(partition) = partitions.get(*index) else {
                continue;
            };
            let mut item = node
                .item
                .clone()
                .detail(partition.mount.to_string_lossy().into_owned());
            if let Some(free) = &partition.free {
                item = item.shortcut(free.clone());
            }
            node.item = item;
        }
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
    use crate::explorer::sidebar::{DevicePartition, device_row_menu_rows};
    use std::path::PathBuf;

    use crate::explorer::ops::{SearchOption, search_option_rows};
    use fileflux::core::Location;
    use fileflux::core::search_match::QueryOptions;

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
            only_files: false,
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
    fn only_a_right_press_with_shift_opens_a_menu_in_new_tab_mode() {
        assert!(opens_in_new_tab(true, true));
        assert!(!opens_in_new_tab(true, false), "a plain right click");
        assert!(
            !opens_in_new_tab(false, true),
            "a Shift chord or a left click on a button"
        );
        assert!(!opens_in_new_tab(false, false));
    }

    #[test]
    fn a_right_press_sets_the_flag_before_its_handlers_and_clears_it_after() {
        use gpui::MouseButton;
        let mut flag = RightPress::default();
        assert!(
            !flag.opens_in_new_tab(true),
            "no press: a keyboard menu with Shift"
        );
        // The capture handler runs first: the menu the press opens, from any
        // handler of it, sees the flag.
        assert!(flag.press(MouseButton::Right), "a right press must settle");
        assert!(flag.opens_in_new_tab(true));
        assert!(!flag.opens_in_new_tab(false), "Shift is still needed");
        // Cleared right after the press: a keyboard menu does not inherit it.
        flag.settle();
        assert!(!flag.opens_in_new_tab(true), "a later keyboard menu");
        assert_eq!(flag, RightPress::default());
    }

    #[test]
    fn only_a_right_press_raises_the_flag() {
        use gpui::MouseButton;
        for button in [MouseButton::Left, MouseButton::Middle] {
            let mut flag = RightPress::default();
            assert!(!flag.press(button), "{button:?} has nothing to settle");
            assert!(!flag.opens_in_new_tab(true), "{button:?}");
        }
        // A left press after a right one drops a flag nobody settled.
        let mut flag = RightPress::default();
        flag.press(MouseButton::Right);
        flag.press(MouseButton::Left);
        assert!(!flag.opens_in_new_tab(true));
    }

    #[test]
    fn shift_puts_a_menu_in_new_tab_mode_and_the_title_says_so() {
        let titled = |new_tab| {
            MenuState::new(
                MenuOn::EmptySpace,
                Some("System".to_owned()),
                point(px(0.0), px(0.0)),
                Vec::new(),
            )
            .in_new_tab(new_tab)
        };
        let plain = titled(false);
        assert!(!plain.new_tab);
        assert_eq!(plain.title.as_deref(), Some("System"));
        let shifted = titled(true);
        assert!(shifted.new_tab);
        assert_eq!(shifted.title.as_deref(), Some("System \u{b7} in new tab"));
        let untitled = MenuState::new(MenuOn::Toolbar, None, point(px(0.0), px(0.0)), Vec::new())
            .in_new_tab(true);
        assert!(untitled.new_tab);
        assert_eq!(untitled.title, None, "no title, nothing to annotate");
    }

    #[test]
    fn only_navigating_commands_open_in_a_new_tab_under_shift() {
        let sidebar = MenuOn::Sidebar {
            row: "r".to_owned(),
            location: Location::Trash,
            device: None,
        };
        #[allow(clippy::expect_used)]
        let bookmark = MenuOn::Network {
            row: "network-bookmark-0".to_owned(),
            bookmark: fileflux::core::bookmarks::NetworkBookmark {
                uri: fileflux::core::network::ServerUri::parse("smb://server/share")
                    .expect("a valid address"),
                title: None,
            },
        };
        let locked = MenuOn::Device {
            row: "device-0".to_owned(),
            device: "/dev/sdb1".into(),
        };
        let navigating = [
            (&MenuOn::Entries, MenuCommand::Open),
            (&MenuOn::Entries, MenuCommand::OpenContainingFolder),
            (&MenuOn::EmptySpace, MenuCommand::AnalyzeDiskUsage),
            (&sidebar, MenuCommand::Open),
            (&sidebar, MenuCommand::AnalyzeDiskUsage),
            // The sidebar shows the place with the details panel.
            (&sidebar, MenuCommand::Properties),
            // Connecting to a bookmark and unlocking a volume open the
            // server's or the volume's folder once they finish.
            (&bookmark, MenuCommand::ConnectBookmark),
            (&locked, MenuCommand::Unlock),
            (&locked, MenuCommand::Mount),
        ];
        for (on, command) in navigating {
            assert_eq!(shown_for(true, on, command), Shown::NewTab, "{command:?}");
            assert_eq!(shown_for(false, on, command), Shown::Here, "{command:?}");
        }
        let staying = [
            (&MenuOn::Entries, MenuCommand::Copy),
            (&MenuOn::Entries, MenuCommand::Cut),
            (&MenuOn::Entries, MenuCommand::Rename),
            (&MenuOn::Entries, MenuCommand::CopyPath),
            (&MenuOn::Entries, MenuCommand::MoveToTrash),
            (&MenuOn::Entries, MenuCommand::DeletePermanently),
            // The entries' Properties toggles the panel; nothing navigates.
            (&MenuOn::Entries, MenuCommand::Properties),
            (&MenuOn::Toolbar, MenuCommand::Properties),
            (&MenuOn::Toolbar, MenuCommand::Reload),
            (&MenuOn::Entries, MenuCommand::OpenInNewTab),
            (&MenuOn::Entries, MenuCommand::OpenInSplit),
            (&sidebar, MenuCommand::OpenInSplit),
            (&sidebar, MenuCommand::CopyPath),
            // Removing a bookmark and ejecting a device go nowhere.
            (&bookmark, MenuCommand::RemoveBookmark),
            (&locked, MenuCommand::Eject),
        ];
        for (on, command) in staying {
            assert_eq!(shown_for(true, on, command), Shown::Here, "{command:?}");
        }
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

    #[test]
    fn the_search_options_popover_rows_keep_it_open_and_name_their_toggle() {
        let rows = search_option_rows(
            QueryOptions {
                match_case: false,
                regex: true,
                contents: false,
            },
            false,
            true,
        );
        let menu = MenuState::new(MenuOn::SearchOptions, None, point(px(0.0), px(0.0)), rows);
        let root = menu.nodes();
        assert!(root.first().is_some_and(|node| node.item.is_header));
        assert_eq!(
            root.iter().filter(|node| node.keeps_menu_open()).count(),
            4,
            "the four toggles keep the popover open"
        );
        assert_eq!(
            menu.item_at(&[1]).map(|spec| spec.command),
            Some(MenuCommand::SearchOption(SearchOption::MatchCase))
        );
        assert_eq!(
            menu.item_at(&[2]).map(|spec| (spec.command, spec.checked)),
            Some((MenuCommand::SearchOption(SearchOption::Regex), Some(true)))
        );
    }

    #[test]
    fn a_device_menu_names_its_partitions_with_their_mounts_and_free_space() {
        let partitions = vec![
            DevicePartition {
                name: "vault1".to_owned(),
                mount: PathBuf::from("/home"),
                free: Some("186 GB".to_owned()),
            },
            DevicePartition {
                name: "vault2".to_owned(),
                mount: PathBuf::from("/"),
                free: None,
            },
        ];
        let location = Location::Directory(PathBuf::from("/home"));
        let rows = device_row_menu_rows(&partitions);
        let menu = MenuState::new(
            MenuOn::DeviceMenu {
                row: "volume-0".to_owned(),
                location,
                partitions,
            },
            None,
            point(px(0.0), px(0.0)),
            rows,
        );
        let root = menu.nodes();
        let partition_row = |at: usize| {
            root.get(at).map(|node| {
                (
                    node.item.label.to_string(),
                    node.item.detail.as_deref(),
                    node.item.shortcut.as_deref(),
                )
            })
        };
        assert_eq!(
            partition_row(5),
            Some(("vault1".to_owned(), Some("/home"), Some("186 GB"))),
            "the first partition names its mount point and free space"
        );
        assert_eq!(
            partition_row(6),
            Some(("vault2".to_owned(), Some("/"), None)),
            "no statvfs answer, no invented number"
        );
        // The activation reads the row's index back, and a partition row is
        // no command.
        assert_eq!(menu.custom_action_at(&[5]), Some(0));
        assert_eq!(menu.custom_action_at(&[6]), Some(1));
        assert!(menu.item_at(&[5]).is_none());
    }
}
