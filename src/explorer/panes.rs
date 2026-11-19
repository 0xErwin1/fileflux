//! The pure dual-pane logic: the two tab lists and which one has the focus,
//! when the window is wide enough for two panes, and what F5 / Shift+F5 do.
//!
//! Nothing here touches GPUI or the filesystem; [`super::view::Explorer`]
//! keeps its tabs in a [`Panes`] and asks it what to do.
//!
//! The panes are symmetric: each owns a [`TabList`] and every tab keeps its
//! own location, history, view, sort, filters and selection. The *focused*
//! pane is the one the keyboard, the toolbar, the breadcrumbs, the search and
//! the sidebar act on, so every per-tab method of [`Panes`] (`active`,
//! `open`, `close`, ...) answers for the focused pane. The methods that look
//! at every tab (`iter`, `iter_mut`, `get`, `position`) cover both panes,
//! including the tabs of a pane that is parked while the view is not split:
//! live refresh, watches and device draining need every tab.

use std::path::PathBuf;

use fileflux::core::Location;
use fileflux::core::settings::{SESSION_VERSION, Session, SessionSplit, SessionTab};

use super::ops::paste::ClipMode;
use super::tabs::{Placement, TabList};

/// The narrowest window (in logical pixels) that draws two panes side by
/// side. Each pane needs its toolbar and about 380 px of entries; with the
/// docked sidebar that is roughly 1100 px. Narrower windows never stack the
/// panes (the toolbar of each would eat the height): the split can only be
/// turned on from a wide window, and a window that shrinks while split shows
/// one pane at a time, the focused one, until it widens again.
pub(crate) const SPLIT_MIN_WIDTH: f32 = 1100.0;

/// How the panes are laid out at a window width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaneLayout {
    /// The view is not split.
    Single,
    /// Both panes, side by side.
    SideBySide,
    /// Split, but the window is too narrow: only the focused pane draws.
    OnePaneAtATime,
}

/// The layout of a window `width` pixels wide.
pub(crate) fn layout(split: bool, width: f32) -> PaneLayout {
    match (split, width >= SPLIT_MIN_WIDTH) {
        (false, _) => PaneLayout::Single,
        (true, true) => PaneLayout::SideBySide,
        (true, false) => PaneLayout::OnePaneAtATime,
    }
}

/// Whether a window `width` pixels wide may turn the split on.
pub(crate) fn may_split(width: f32) -> bool {
    width >= SPLIT_MIN_WIDTH
}

/// The notice that explains why a narrow window refuses the split.
pub(crate) fn split_refusal(width: f32) -> String {
    format!(
        "Split view needs a window at least {SPLIT_MIN_WIDTH:.0} px wide (this one is {width:.0} px)"
    )
}

/// One of the two panes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    Left,
    Right,
}

impl Side {
    /// The pane on the other side.
    pub(crate) const fn other(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }

    /// Position, for element ids.
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Left => 0,
            Self::Right => 1,
        }
    }
}

/// One value for each pane.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PerPane<T> {
    left: T,
    right: T,
}

impl<T> PerPane<T> {
    pub(crate) fn new(left: T, right: T) -> Self {
        Self { left, right }
    }

    /// The value of `side`.
    pub(crate) fn get(&self, side: Side) -> &T {
        match side {
            Side::Left => &self.left,
            Side::Right => &self.right,
        }
    }

    /// The value of `side`, mutably.
    pub(crate) fn get_mut(&mut self, side: Side) -> &mut T {
        match side {
            Side::Left => &mut self.left,
            Side::Right => &mut self.right,
        }
    }
}

/// The window's tabs: a left pane that always exists, and a right pane that
/// exists once the view has been split. Turning the split off *parks* the
/// right pane's tabs (they stay open and live, ready for the next split)
/// instead of closing them.
///
/// Invariant: `split` implies the right pane exists, and the focus is on the
/// right pane only while `split`.
#[derive(Debug)]
pub(crate) struct Panes<T> {
    left: TabList<T>,
    right: Option<TabList<T>>,
    split: bool,
    focus: Side,
}

impl<T> Panes<T> {
    /// A single pane holding `first`.
    pub(crate) fn new(first: T) -> Self {
        Self {
            left: TabList::new(first),
            right: None,
            split: false,
            focus: Side::Left,
        }
    }

    /// Whether both panes are shown.
    pub(crate) fn is_split(&self) -> bool {
        self.split
    }

    /// Whether the right pane's tabs exist (shown or parked).
    pub(crate) fn has_right(&self) -> bool {
        self.right.is_some()
    }

    /// The focused pane.
    pub(crate) fn focus(&self) -> Side {
        self.focus
    }

    /// The focused pane's tabs.
    pub(crate) fn focused(&self) -> &TabList<T> {
        match (self.focus, &self.right) {
            (Side::Right, Some(right)) => right,
            _ => &self.left,
        }
    }

    /// The focused pane's tabs, mutably.
    pub(crate) fn focused_mut(&mut self) -> &mut TabList<T> {
        match (self.focus, &mut self.right) {
            (Side::Right, Some(right)) => right,
            _ => &mut self.left,
        }
    }

    /// The shown pane that does not have the focus: where F5 sends files.
    pub(crate) fn other(&self) -> Option<&TabList<T>> {
        if !self.split {
            return None;
        }
        match self.focus {
            Side::Left => self.right.as_ref(),
            Side::Right => Some(&self.left),
        }
    }

    /// Gives the focus to `side`. `false` — nothing changes — when that pane
    /// is not shown.
    pub(crate) fn focus_side(&mut self, side: Side) -> bool {
        let shown = match side {
            Side::Left => true,
            Side::Right => self.split,
        };
        if shown {
            self.focus = side;
        }
        shown
    }

    /// F6: moves the focus to the other pane. `false` while not split.
    pub(crate) fn switch_focus(&mut self) -> bool {
        self.split && self.focus_side(self.focus.other())
    }

    /// Splits the view. A pane parked by an earlier [`Self::unsplit`] comes
    /// back with its tabs; otherwise the right pane starts from `second`.
    /// `false` — nothing changes — when there is neither. The focus stays
    /// where it was.
    pub(crate) fn split(&mut self, second: Option<T>) -> bool {
        if self.right.is_none() {
            match second {
                Some(tab) => self.right = Some(TabList::new(tab)),
                None => return false,
            }
        }
        self.split = true;
        true
    }

    /// Turns the split off. The pane that had the focus becomes the single
    /// pane and the other one is parked, tabs intact. `true` when the pane
    /// that stays is not the one in the left slot (the focus was on the
    /// right pane): the tab on screen then changed hands, so the caller
    /// re-reads it and puts the keyboard on its entries.
    pub(crate) fn unsplit(&mut self) -> bool {
        if !self.split {
            return false;
        }
        let swapped = self.focus == Side::Right;
        if swapped && let Some(right) = self.right.as_mut() {
            std::mem::swap(&mut self.left, right);
        }
        self.focus = Side::Left;
        self.split = false;
        swapped
    }

    /// The last step of reopening a saved session's second pane, once its
    /// tabs are open: the pane is shown or parked again, and the focus
    /// returns to it only when it was focused (and shown).
    pub(crate) fn finish_restore(&mut self, shown: bool, focused: bool) {
        self.focus_side(Side::Left);
        if !shown {
            self.unsplit();
        } else if focused {
            self.focus_side(Side::Right);
        }
    }

    /// The focused pane's tab count.
    pub(crate) fn len(&self) -> usize {
        self.focused().len()
    }

    /// Where the focused pane's tabs start in the window-wide order of
    /// [`Self::iter`]: every index this type takes or returns (`get`,
    /// `position`, `activate`, `close`, `active_index`) counts in that one
    /// order, left pane first, so an index can never mean a tab of the other
    /// pane.
    pub(crate) fn focused_offset(&self) -> usize {
        match (self.focus, &self.right) {
            (Side::Right, Some(_)) => self.left.len(),
            _ => 0,
        }
    }

    /// The window-wide position of the focused pane's active tab.
    pub(crate) fn active_index(&self) -> usize {
        self.focused_offset() + self.focused().active_index()
    }

    /// The focused pane's active tab.
    pub(crate) fn active(&self) -> &T {
        self.focused().active()
    }

    /// The focused pane's active tab, mutably.
    pub(crate) fn active_mut(&mut self) -> &mut T {
        self.focused_mut().active_mut()
    }

    /// Opens `item` in the focused pane; returns its position there.
    pub(crate) fn open(&mut self, item: T, placement: Placement) -> usize {
        self.focused_mut().open(item, placement)
    }

    /// Closes the tab at the window-wide `index` (see [`TabList::close`]).
    /// `None` — nothing changes — when it is not a tab of the focused pane.
    pub(crate) fn close(&mut self, index: usize) -> Option<T> {
        let local = index.checked_sub(self.focused_offset())?;
        self.focused_mut().close(local)
    }

    /// Activates the tab at the window-wide `index`. `false` — nothing
    /// changes — when it is not a tab of the focused pane.
    pub(crate) fn activate(&mut self, index: usize) -> bool {
        match index.checked_sub(self.focused_offset()) {
            Some(local) => self.focused_mut().activate(local),
            None => false,
        }
    }

    /// Cycles the focused pane's active tab.
    pub(crate) fn cycle(&mut self, step: isize) {
        self.focused_mut().cycle(step);
    }

    /// Alt+1..9 in the focused pane.
    pub(crate) fn jump(&mut self, number: usize) -> bool {
        self.focused_mut().jump(number)
    }

    /// Every tab of both panes, the left pane first (a parked pane
    /// included).
    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> {
        self.left
            .iter()
            .chain(self.right.iter().flat_map(TabList::iter))
    }

    /// Every tab of both panes, mutably.
    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.left
            .iter_mut()
            .chain(self.right.iter_mut().flat_map(TabList::iter_mut))
    }

    /// The tab at the window-wide `index` (see [`Self::focused_offset`]).
    pub(crate) fn get(&self, index: usize) -> Option<&T> {
        self.iter().nth(index)
    }

    /// The window-wide position of the first tab `matches` accepts.
    pub(crate) fn position(&self, matches: impl Fn(&T) -> bool) -> Option<usize> {
        self.iter().position(matches)
    }
}

/// The pane that must take the focus before a drop is handled: the pane the
/// drop landed in, which is not necessarily the one that has the focus (the
/// press that started the drag focused the pane the entries came from).
/// `None` when the focus is already right, or the view shows one pane.
pub(crate) fn drop_focus(split: bool, focus: Side, dropped_in: Side) -> Option<Side> {
    (split && focus != dropped_in).then_some(dropped_in)
}

/// The session to save for `panes`: the left slot's tabs first, and the
/// second pane (shown or parked) with whether it was shown and focused.
/// `tab` saves one tab.
pub(crate) fn session_of<T>(panes: &Panes<T>, tab: impl Fn(&T) -> SessionTab) -> Session {
    let saved = |list: &TabList<T>| list.iter().map(&tab).collect::<Vec<_>>();
    Session {
        version: SESSION_VERSION,
        active: panes.left.active_index(),
        tabs: saved(&panes.left),
        split: panes.right.as_ref().map(|right| SessionSplit {
            shown: panes.split,
            focused: panes.split && panes.focus == Side::Right,
            active: right.active_index(),
            tabs: saved(right),
        }),
    }
}

/// What F5 (copy) and Shift+F5 (move) do: the sources and the folder of the
/// other pane they go to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Transfer {
    pub(crate) sources: Vec<PathBuf>,
    pub(crate) dest: PathBuf,
}

/// Decides F5 / Shift+F5. `here` is the focused pane's location, `other` the
/// other pane's (`None` while the view is not split) and `selection` the
/// visible selection. The `Err` is the toast text that says why nothing
/// happens: the destination must be a real folder, so the read-only views
/// (Trash, Recent, search results) refuse, and the Trash is no source for a
/// copy or move (Restore is).
pub(crate) fn plan_transfer(
    mode: ClipMode,
    here: &Location,
    other: Option<&Location>,
    selection: Vec<PathBuf>,
) -> Result<Transfer, String> {
    let verb = match mode {
        ClipMode::Copy => "copy",
        ClipMode::Cut => "move",
    };
    let Some(other) = other else {
        return Err(format!("Split the view (F3) to {verb} between two panes"));
    };
    if here == &Location::Trash {
        return Err(format!(
            "Can't {verb} out of the Trash. Restore the items instead"
        ));
    }
    let dest = match other {
        Location::Directory(dest) => dest.clone(),
        Location::Recent => {
            return Err(format!(
                "Can't {verb} into Recent. Open a folder in the other pane first"
            ));
        }
        Location::Search { .. } => {
            return Err(format!(
                "Can't {verb} into search results. Open a folder in the other pane first"
            ));
        }
        Location::Trash => {
            return Err(format!(
                "Can't {verb} into the Trash. Use Delete to trash items"
            ));
        }
    };
    if selection.is_empty() {
        return Err(format!("Select something to {verb} first"));
    }
    // A selected folder cannot go into itself or anything inside it.
    if selection.iter().any(|source| dest.starts_with(source)) {
        return Err(format!("Can't {verb} a folder into itself"));
    }
    // Items already in the destination folder have nowhere to go.
    if selection
        .iter()
        .any(|source| source.parent() == Some(dest.as_path()))
    {
        return Err(format!(
            "Can't {verb} here: the items already live in the other pane's folder"
        ));
    }
    Ok(Transfer {
        sources: selection,
        dest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::arrange::Arrangement;
    use crate::explorer::persist::session_tab;
    use crate::explorer::state::View;
    use fileflux::core::settings::restore_session;
    use std::path::Path;

    fn dir(path: &str) -> Location {
        Location::Directory(PathBuf::from(path))
    }

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    fn split_panes() -> Panes<&'static str> {
        let mut panes = Panes::new("l1");
        panes.open("l2", Placement::Foreground);
        assert!(panes.split(Some("r1")));
        panes
    }

    fn all(panes: &Panes<&'static str>) -> Vec<&'static str> {
        panes.iter().copied().collect()
    }

    #[test]
    fn per_pane_values_are_independent() {
        let mut values = PerPane::new(1, 2);
        *values.get_mut(Side::Right) += 10;
        assert_eq!((*values.get(Side::Left), *values.get(Side::Right)), (1, 12));
    }

    #[test]
    fn a_new_window_has_one_focused_left_pane() {
        let panes = Panes::new("a");
        assert!(!panes.is_split());
        assert!(!panes.has_right());
        assert_eq!(panes.focus(), Side::Left);
        assert_eq!(*panes.active(), "a");
        assert!(panes.other().is_none());
    }

    #[test]
    fn splitting_needs_a_tab_for_a_pane_that_never_existed() {
        let mut panes = Panes::new("a");
        assert!(!panes.split(None), "no second tab, nothing to show");
        assert!(!panes.is_split());
        assert!(panes.split(Some("b")));
        assert!(panes.is_split());
        assert_eq!(panes.other().map(|list| *list.active()), Some("b"));
        assert_eq!(panes.focus(), Side::Left, "the focus stays");
    }

    #[test]
    fn f6_switches_between_the_panes_and_does_nothing_without_a_split() {
        let mut panes = Panes::new("a");
        assert!(!panes.switch_focus());
        assert_eq!(panes.focus(), Side::Left);
        assert!(panes.split(Some("b")));
        assert!(panes.switch_focus());
        assert_eq!(panes.focus(), Side::Right);
        assert_eq!(*panes.active(), "b");
        assert_eq!(panes.other().map(|list| *list.active()), Some("a"));
        assert!(panes.switch_focus());
        assert_eq!(panes.focus(), Side::Left);
    }

    #[test]
    fn a_hidden_pane_cannot_take_the_focus() {
        let mut panes = Panes::new("a");
        assert!(!panes.focus_side(Side::Right));
        assert_eq!(panes.focus(), Side::Left);
        assert!(panes.focus_side(Side::Left));
        assert!(panes.split(Some("b")));
        assert!(panes.focus_side(Side::Right));
        assert_eq!(panes.focus(), Side::Right);
    }

    #[test]
    fn tab_commands_act_on_the_focused_pane_only() {
        let mut panes = split_panes();
        // Left has l1 + l2 (l2 active), right has r1.
        assert_eq!(panes.len(), 2);
        panes.open("l3", Placement::Foreground);
        assert_eq!(panes.len(), 3);
        assert!(panes.switch_focus());
        assert_eq!(panes.len(), 1);
        panes.open("r2", Placement::Foreground);
        assert_eq!(panes.len(), 2);
        assert!(panes.close(panes.focused_offset()).is_some());
        assert_eq!(panes.len(), 1);
        assert!(panes.switch_focus());
        assert_eq!(panes.len(), 3, "the left pane kept all of its tabs");
    }

    #[test]
    fn each_pane_keeps_its_own_active_tab() {
        let mut panes = split_panes();
        panes.activate(0);
        assert_eq!(*panes.active(), "l1");
        assert!(panes.switch_focus());
        assert_eq!(*panes.active(), "r1");
        assert!(panes.switch_focus());
        assert_eq!(*panes.active(), "l1");
    }

    #[test]
    fn iteration_covers_both_panes_left_first() {
        let panes = split_panes();
        assert_eq!(all(&panes), ["l1", "l2", "r1"]);
        assert_eq!(panes.get(2), Some(&"r1"));
        assert_eq!(panes.position(|tab| *tab == "r1"), Some(2));
    }

    #[test]
    fn iteration_mut_reaches_every_tab_of_both_panes() {
        let mut panes = split_panes();
        let mut seen = 0;
        for _ in panes.iter_mut() {
            seen += 1;
        }
        assert_eq!(seen, 3);
    }

    #[test]
    fn unsplitting_keeps_the_focused_pane_and_parks_the_other() {
        let mut panes = split_panes();
        assert!(panes.switch_focus());
        panes.unsplit();
        assert!(!panes.is_split());
        assert_eq!(panes.focus(), Side::Left);
        assert_eq!(*panes.active(), "r1", "the focused pane stayed");
        assert!(panes.has_right());
        // Nothing was closed: the parked tabs are still iterated.
        assert_eq!(all(&panes).len(), 3);
        assert!(panes.other().is_none(), "a parked pane is no target");
        assert!(!panes.switch_focus());
    }

    #[test]
    fn splitting_again_brings_the_parked_pane_back() {
        let mut panes = split_panes();
        panes.unsplit();
        assert_eq!(*panes.active(), "l2");
        assert!(panes.split(None), "the parked pane needs no new tab");
        assert!(panes.is_split());
        assert_eq!(panes.other().map(|list| *list.active()), Some("r1"));
        assert_eq!(all(&panes), ["l1", "l2", "r1"]);
    }

    #[test]
    fn the_split_layout_follows_the_width() {
        assert_eq!(layout(false, 2000.0), PaneLayout::Single);
        assert_eq!(layout(true, SPLIT_MIN_WIDTH), PaneLayout::SideBySide);
        assert_eq!(
            layout(true, SPLIT_MIN_WIDTH - 1.0),
            PaneLayout::OnePaneAtATime
        );
        assert!(may_split(SPLIT_MIN_WIDTH));
        assert!(!may_split(SPLIT_MIN_WIDTH - 1.0));
        assert_eq!(
            split_refusal(900.0),
            "Split view needs a window at least 1100 px wide (this one is 900 px)"
        );
    }

    #[test]
    fn f5_copies_and_shift_f5_moves_into_the_other_panes_folder() {
        let selection = paths(&["/a/x", "/a/y"]);
        for mode in [ClipMode::Copy, ClipMode::Cut] {
            let transfer = plan_transfer(mode, &dir("/a"), Some(&dir("/b")), selection.clone());
            assert_eq!(
                transfer,
                Ok(Transfer {
                    sources: selection.clone(),
                    dest: PathBuf::from("/b"),
                })
            );
        }
    }

    #[test]
    fn a_transfer_needs_a_split() {
        assert_eq!(
            plan_transfer(ClipMode::Copy, &dir("/a"), None, paths(&["/a/x"])),
            Err("Split the view (F3) to copy between two panes".to_owned())
        );
        assert_eq!(
            plan_transfer(ClipMode::Cut, &dir("/a"), None, paths(&["/a/x"])),
            Err("Split the view (F3) to move between two panes".to_owned())
        );
    }

    #[test]
    fn the_read_only_views_refuse_as_a_destination() {
        let search = Location::Search {
            root: PathBuf::from("/b"),
            query: "q".to_owned(),
        };
        let cases = [
            (
                Location::Trash,
                "Can't copy into the Trash. Use Delete to trash items",
            ),
            (
                Location::Recent,
                "Can't copy into Recent. Open a folder in the other pane first",
            ),
            (
                search,
                "Can't copy into search results. Open a folder in the other pane first",
            ),
        ];
        for (other, message) in cases {
            assert_eq!(
                plan_transfer(ClipMode::Copy, &dir("/a"), Some(&other), paths(&["/a/x"])),
                Err(message.to_owned())
            );
        }
        assert_eq!(
            plan_transfer(
                ClipMode::Cut,
                &dir("/a"),
                Some(&Location::Recent),
                paths(&["/a/x"])
            ),
            Err("Can't move into Recent. Open a folder in the other pane first".to_owned())
        );
    }

    #[test]
    fn the_trash_is_no_source_and_an_empty_selection_has_nothing_to_send() {
        assert_eq!(
            plan_transfer(
                ClipMode::Cut,
                &Location::Trash,
                Some(&dir("/b")),
                paths(&["/t/x"])
            ),
            Err("Can't move out of the Trash. Restore the items instead".to_owned())
        );
        assert_eq!(
            plan_transfer(ClipMode::Copy, &dir("/a"), Some(&dir("/b")), Vec::new()),
            Err("Select something to copy first".to_owned())
        );
    }

    #[test]
    fn a_search_or_recent_source_is_fine() {
        let search = Location::Search {
            root: PathBuf::from("/a"),
            query: "q".to_owned(),
        };
        for here in [search, Location::Recent] {
            assert!(
                plan_transfer(ClipMode::Copy, &here, Some(&dir("/b")), paths(&["/a/x"])).is_ok()
            );
        }
    }

    #[test]
    fn a_transfer_into_the_folder_the_items_live_in_is_refused() {
        for mode in [ClipMode::Copy, ClipMode::Cut] {
            let verb = if mode == ClipMode::Copy {
                "copy"
            } else {
                "move"
            };
            assert_eq!(
                plan_transfer(mode, &dir("/a"), Some(&dir("/a")), paths(&["/a/x"])),
                Err(format!(
                    "Can't {verb} here: the items already live in the other pane's folder"
                ))
            );
        }
        // One item from the destination is enough (a search can mix folders).
        let search = Location::Search {
            root: PathBuf::from("/"),
            query: "q".to_owned(),
        };
        assert!(
            plan_transfer(
                ClipMode::Copy,
                &search,
                Some(&dir("/b")),
                paths(&["/a/x", "/b/y"])
            )
            .is_err()
        );
    }

    #[test]
    fn a_selected_folder_cannot_go_into_itself_or_below() {
        for dest in ["/a/docs", "/a/docs/deep/er"] {
            assert_eq!(
                plan_transfer(
                    ClipMode::Cut,
                    &dir("/a"),
                    Some(&dir(dest)),
                    paths(&["/a/docs"])
                ),
                Err("Can't move a folder into itself".to_owned())
            );
        }
        // A sibling sharing a name prefix is not inside it.
        assert!(
            plan_transfer(
                ClipMode::Copy,
                &dir("/a"),
                Some(&dir("/a/docs2")),
                paths(&["/a/docs"])
            )
            .is_ok()
        );
    }

    #[test]
    fn indices_count_window_wide_and_never_reach_the_other_pane() {
        let mut panes = split_panes(); // flat: l1 l2 | r1, focus left
        let at = 2;
        assert_eq!(panes.position(|tab| *tab == "r1"), Some(at));
        assert_eq!(panes.get(at), Some(&"r1"));
        // Focused left: the right pane's tab is out of reach.
        assert!(!panes.activate(at));
        assert_eq!(panes.close(at), None);
        assert_eq!(all(&panes), ["l1", "l2", "r1"]);
        assert_eq!(panes.active_index(), 1, "l2 is active");
        // Focused right: the same index works and the left tabs do not.
        assert!(panes.switch_focus());
        assert_eq!(panes.active_index(), 2);
        assert!(!panes.activate(0));
        assert_eq!(panes.close(1), None);
        assert!(panes.activate(at));
        panes.open("r2", Placement::Foreground); // flat: l1 l2 | r1 r2
        assert_eq!(panes.active_index(), 3);
        assert_eq!(panes.close(at), Some("r1"));
        assert_eq!(all(&panes), ["l1", "l2", "r2"]);
    }

    #[test]
    fn unsplitting_says_whether_the_remaining_tab_changed() {
        let mut panes = split_panes();
        assert!(!panes.unsplit(), "the left pane stays: nothing changed");
        assert!(!panes.unsplit(), "already one pane");
        let mut panes = split_panes();
        assert!(panes.switch_focus());
        assert!(panes.unsplit(), "the right pane took the left slot");
        assert_eq!(*panes.active(), "r1");
        assert_eq!(panes.focus(), Side::Left);
    }

    #[test]
    fn a_drop_focuses_the_pane_it_landed_in() {
        assert_eq!(drop_focus(true, Side::Left, Side::Right), Some(Side::Right));
        assert_eq!(drop_focus(true, Side::Right, Side::Left), Some(Side::Left));
        assert_eq!(drop_focus(true, Side::Right, Side::Right), None);
        assert_eq!(drop_focus(false, Side::Left, Side::Right), None);
    }

    fn saved(panes: &Panes<Location>) -> Session {
        session_of(panes, |location| {
            session_tab(location, View::List, &Arrangement::default())
        })
    }

    fn rebuild(restored: &fileflux::core::settings::Restored) -> Option<Panes<Location>> {
        let mut tabs = restored.tabs.iter().map(|tab| tab.location.clone());
        let mut panes = Panes::new(tabs.next()?);
        for location in tabs {
            panes.open(location, Placement::Background);
        }
        assert!(panes.activate(restored.active));
        if let Some(split) = &restored.split {
            let mut tabs = split.tabs.iter().map(|tab| tab.location.clone());
            assert!(panes.split(tabs.next()));
            panes.focus_side(Side::Right);
            for location in tabs {
                panes.open(location, Placement::Background);
            }
            let at = panes.focused_offset() + split.active;
            assert!(panes.activate(at));
            panes.finish_restore(split.shown, split.focused);
        }
        Some(panes)
    }

    fn layout_of(panes: &Panes<Location>) -> (bool, Side, Vec<Location>, [usize; 2]) {
        let active = |side| match side {
            Side::Left => Some(&panes.left),
            Side::Right => panes.right.as_ref(),
        };
        let active = |side| active(side).map_or(0, TabList::active_index);
        (
            panes.is_split(),
            panes.focus(),
            panes.iter().cloned().collect(),
            [active(Side::Left), active(Side::Right)],
        )
    }

    #[test]
    fn the_split_survives_a_save_and_restore() -> Result<(), String> {
        let home = Some(Path::new("/h"));
        for (shown, focus_right) in [(true, true), (true, false), (false, false)] {
            let mut panes = Panes::new(dir("/l1"));
            panes.open(dir("/l2"), Placement::Foreground);
            panes.activate(0);
            assert!(panes.split(Some(dir("/r1"))));
            panes.focus_side(Side::Right);
            panes.open(Location::Trash, Placement::Background);
            panes.open(dir("/r3"), Placement::Foreground);
            if !focus_right {
                panes.focus_side(Side::Left);
            }
            if !shown {
                panes.unsplit();
            }
            let session = saved(&panes);
            let restored = restore_session(&session, home);
            let split = restored.split.as_ref().ok_or("the split is saved")?;
            assert_eq!(split.shown, shown);
            assert_eq!(split.focused, shown && focus_right);
            let again = rebuild(&restored).ok_or("the panes rebuild")?;
            assert_eq!(layout_of(&again), layout_of(&panes));
        }
        Ok(())
    }

    #[test]
    fn a_window_that_never_split_saves_no_second_pane() {
        let panes = Panes::new(dir("/a"));
        assert!(saved(&panes).split.is_none());
        assert!(restore_session(&saved(&panes), None).split.is_none());
    }
}
