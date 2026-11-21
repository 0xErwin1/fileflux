//! The toolbar of the document island, six groups: the sidebar hamburger
//! (only on the leftmost pane while the sidebar is hidden), the
//! history buttons with disabled states, the breadcrumb path (Ctrl+L or a
//! click on its empty space edits it), the current-folder filename search
//! (Ctrl+F, with its live match count and its options behind a filter
//! button at the field's trailing edge), the segmented grid/list toggle and
//! one `⋯` menu holding Split view, Favorite this folder, Refresh and
//! Properties, and — in a split view — the pane's close control.
//!
//! The controls keep their sizes from `bolt`'s tokens. The row never wraps
//! and never overflows its pane: the breadcrumbs and the search field give
//! way first (down to [`CRUMBS_MIN_WIDTH`] and [`SEARCH_MIN_WIDTH`]), and
//! whatever still does not fit is clipped at the pane edge.

use bolt::controls::{Button, InputEscape, SearchCount, SearchField};
use bolt::primitives::Icon;
use bolt::tokens::{ChromeColors, Spacing};
use fileflux::core::Location;
use gpui::{Context, Div, MouseButton, Stateful, Styled, div, prelude::*, px};
use gpui_component::ActiveTheme as _;
use gpui_component::tooltip::Tooltip;

use super::actions::MenuOn;
use super::dnd::{self, DropTarget};
use super::icon;
use super::panes::Side;
use super::state::{Transition, View};
use super::tabs::Placement;
use super::util::{self, CrumbKind};
use super::view::Explorer;

/// The narrowest the breadcrumb bar and the search field get before the row
/// clips: both shrink from their natural width down to these.
pub(crate) const CRUMBS_MIN_WIDTH: f32 = 72.0;
pub(crate) const SEARCH_MIN_WIDTH: f32 = 56.0;

/// What a pane's toolbar carries besides its own controls.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Chrome {
    /// The hamburger that shows the hidden sidebar (leftmost pane only).
    pub(crate) sidebar_toggle: bool,
    /// The pane this toolbar closes with its × control, in a split view.
    pub(crate) close: Option<Side>,
}

/// The toolbar row: one line, never wider than its pane.
pub(crate) fn render(this: &Explorer, chrome: Chrome, cx: &mut Context<Explorer>) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    let search_options_open = matches!(
        this.menu.as_ref().map(|menu| &menu.on),
        Some(MenuOn::SearchOptions)
    );
    let toolbar_menu_open = matches!(
        this.menu.as_ref().map(|menu| &menu.on),
        Some(MenuOn::Toolbar)
    );

    div()
        .id("explorer-toolbar")
        .flex_none()
        .w_full()
        .min_w_0()
        .overflow_hidden()
        .flex()
        .items_center()
        .gap(Spacing::XXS)
        .pl(px(14.0))
        .pr(px(10.0))
        .min_h(px(50.0))
        .border_b_1()
        .border_color(colors.line)
        // The hamburger that brings the hidden sidebar back: the first
        // control of the leftmost pane only (the sidebar has its own).
        .children(chrome.sidebar_toggle.then(|| {
            Button::new("explorer-sidebar-show", "Show sidebar")
                .ghost()
                .icon_only()
                .icon(icon(gpui_kit_assets::IconName::Menu))
                .tooltip("Show sidebar")
                .on_click(cx.listener(|this, _event, window, cx| {
                    this.toggle_sidebar(window, cx);
                }))
        }))
        // History: back, forward, up — with honest disabled states.
        .child(
            div()
                .id("explorer-nav-buttons")
                .flex_none()
                .flex()
                .gap(Spacing::XS)
                .child(
                    Button::new("explorer-back", "Back")
                        .ghost()
                        .icon_only()
                        .icon(icon(gpui_kit_assets::IconName::ChevronLeft))
                        .tooltip("Go back (Alt+Left)")
                        .disabled(!this.tab().state.navigation().can_back())
                        .on_click(cx.listener(|this, _event, window, cx| {
                            this.navigate(Transition::Back, window, cx);
                        })),
                )
                .child(
                    Button::new("explorer-forward", "Forward")
                        .ghost()
                        .icon_only()
                        .icon(icon(gpui_kit_assets::IconName::ChevronRight))
                        .tooltip("Go forward (Alt+Right)")
                        .disabled(!this.tab().state.navigation().can_forward())
                        .on_click(cx.listener(|this, _event, window, cx| {
                            this.navigate(Transition::Forward, window, cx);
                        })),
                )
                .child(
                    Button::new("explorer-up", "Up")
                        .ghost()
                        .icon_only()
                        .icon(icon(gpui_kit_assets::IconName::ChevronUp))
                        .tooltip("Parent folder (Alt+Up)")
                        .disabled(!this.tab().state.navigation().can_up())
                        .on_click(cx.listener(|this, _event, window, cx| {
                            this.navigate(Transition::Up, window, cx);
                        })),
                ),
        )
        // The current path: breadcrumbs, or — after a click on the bar's
        // empty space or Ctrl+L — the editable field. The frame captures
        // the input's own Escape (`bolt_input::Escape`), which the input
        // consumes before it would ever bubble: here Escape restores the
        // committed path, returns to the breadcrumbs and gives the focus
        // back to the entries area (reqflux sidebar filter pattern).
        .child(if this.tab().path_editing {
            path_editor(this, cx).into_any_element()
        } else {
            crumb_bar(this, cx).into_any_element()
        })
        // Current-folder filename search with its live count, and its
        // options behind the filter button at the field's trailing edge.
        // The frame wrapping the field captures the input's own Escape here
        // too: it clears the query, restores the full listing and returns
        // the entries focus.
        .child(
            div()
                .id("explorer-search-frame")
                // Natural width 13.75 rem, shrinking to its minimum.
                .flex()
                .w(gpui::rems(13.75))
                .min_w(px(SEARCH_MIN_WIDTH))
                .capture_action(cx.listener(|this, _: &InputEscape, window, cx| {
                    this.clear_search_on_escape(window, cx);
                    cx.stop_propagation();
                }))
                .child(
                    SearchField::new("explorer-search", &this.tab().search_input)
                        .aria_label(if this.tab().deep_search {
                            "Search names in the current folder and its subfolders"
                        } else {
                            "Search names in current folder"
                        })
                        .count(search_count(this, cx))
                        .trailing(search_filter_button(this, cx, search_options_open)),
                ),
        )
        // An invalid regular expression: why nothing ran.
        .children(this.tab().search_error.clone().map(|error| {
            div()
                .id("explorer-search-error")
                .flex_none()
                .max_w(px(260.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_color(colors.danger)
                .child(error)
        }))
        // Grid/list/compact view: one segmented control, the pressed segment raised
        // and tinted.
        .child(
            div()
                .id("explorer-view-toggle")
                .flex_none()
                .flex()
                .p(px(2.0))
                .gap(px(2.0))
                .bg(colors.background)
                .border_1()
                .border_color(colors.line)
                .child(view_segment(
                    this,
                    cx,
                    "explorer-view-grid",
                    gpui_kit_assets::IconName::LayoutGrid,
                    "Icon view",
                    View::Grid,
                ))
                .child(view_segment(
                    this,
                    cx,
                    "explorer-view-list",
                    gpui_kit_assets::IconName::List,
                    "List view",
                    View::List,
                ))
                .child(view_segment(
                    this,
                    cx,
                    "explorer-view-compact",
                    gpui_kit_assets::IconName::LayoutList,
                    "Compact view",
                    View::Compact,
                )),
        )
        // The one overflow control: Split view, Favorite this folder,
        // Refresh and Properties, each with its shortcut. It stays raised
        // while its menu is open.
        .child(
            Button::new("explorer-more", "More actions")
                .ghost()
                .icon_only()
                .icon(icon(gpui_kit_assets::IconName::Ellipsis))
                .tooltip("Split view, favorite this folder, refresh and properties")
                .selected(toolbar_menu_open)
                .toggled(toolbar_menu_open)
                .on_click(cx.listener(|this, event: &gpui::ClickEvent, window, cx| {
                    let position = super::overlay::below(event.position());
                    this.open_toolbar_menu(position, window, cx);
                })),
        )
        // In a split view each pane closes itself; the other one stays.
        .children(chrome.close.map(|side| {
            Button::new("explorer-close-pane", "Close pane")
                .ghost()
                .icon_only()
                .icon(icon(gpui_kit_assets::IconName::X))
                .tooltip("Close this pane")
                .on_click(cx.listener(move |this, _event, window, cx| {
                    this.close_pane(side, window, cx);
                }))
        }))
}

/// The search field's filter button: opens the search options popover —
/// Match case, Regular expression, File contents and the subfolder scope —
/// and carries the field's tinted look while any of them is on or the
/// popover is open.
fn search_filter_button(this: &Explorer, cx: &mut Context<Explorer>, open: bool) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    let tint = ChromeColors::tint(cx.theme());
    let on = this.search_options.match_case
        || this.search_options.regex
        || this.search_options.contents
        || this.tab().deep_search;
    div()
        .id("explorer-search-options")
        .flex_none()
        .size(px(24.0))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .text_color(if on || open { tint } else { colors.muted })
        .when(on || open, |button| {
            button.bg(tint.opacity(0.12)).border_1().border_color(tint)
        })
        .tooltip(|window, cx| Tooltip::new("Search options").build(window, cx))
        .on_click(cx.listener(|this, event: &gpui::ClickEvent, window, cx| {
            let position = super::overlay::under_button(event.position());
            this.open_search_options_menu(position, window, cx);
        }))
        .child(Icon::new(icon(gpui_kit_assets::IconName::ListFilter)).size(px(13.0)))
}

/// The editable path field in the breadcrumb bar's place.
fn path_editor(this: &Explorer, cx: &mut Context<Explorer>) -> Stateful<Div> {
    // The tab this frame belongs to: a press in the other pane of a split
    // view moves the focus before this handler runs.
    let id = this.tab().id;
    div()
        .id("explorer-path-frame")
        .flex_1()
        .min_w(px(CRUMBS_MIN_WIDTH))
        .h(px(30.0))
        .capture_action(cx.listener(|this, _: &InputEscape, window, cx| {
            this.restore_path_on_escape(window, cx);
            cx.stop_propagation();
        }))
        .on_mouse_down_out(cx.listener(move |this, _event, window, cx| {
            this.leave_path_edit(id, window, cx);
        }))
        .child(
            bolt::controls::Input::new(&this.tab().path_input)
                .id("explorer-path-input")
                .w_full()
                .aria_label("Current folder path")
                .prefix(Icon::new(icon(gpui_kit_assets::IconName::Folder)))
                .error(
                    this.tab()
                        .state
                        .path_error()
                        .map(super::view::path_argument_message),
                ),
        )
}

/// The breadcrumb bar: a 30 px control with Home (or `/`) first, chevron
/// separators and one clickable segment per folder, the last one strong.
/// Leading segments shrink and clip before the current folder does; a click
/// on the bar's empty space opens the path editor.
fn crumb_bar(this: &Explorer, cx: &mut Context<Explorer>) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    let home = this.places.as_ref().map(|places| places.home.as_path());
    let crumbs = util::breadcrumbs(this.tab().state.current(), home);
    let last = crumbs.len().saturating_sub(1);

    let mut bar = div()
        .id("explorer-crumbs")
        .flex_1()
        .min_w(px(CRUMBS_MIN_WIDTH))
        .h(px(30.0))
        .flex()
        .items_center()
        .px(px(4.0))
        .overflow_hidden()
        .bg(colors.background)
        .border_1()
        .border_color(colors.line)
        .hover(|bar| bar.border_color(colors.line_2))
        .cursor_text()
        .tooltip(|window, cx| {
            Tooltip::new("Click empty space or press Ctrl+L to type a path").build(window, cx)
        })
        .on_click(cx.listener(|this, _event, window, cx| {
            this.focus_path(window, cx);
        }));

    for (index, crumb) in crumbs.into_iter().enumerate() {
        if index > 0 {
            bar = bar.child(
                Icon::new(icon(gpui_kit_assets::IconName::ChevronRight))
                    .size(px(12.0))
                    .color(colors.line_2),
            );
        }
        let is_last = index == last;
        let target = crumb.target.clone();
        let drop = match &crumb.target {
            Location::Directory(path) => Some(DropTarget::Folder(path.clone())),
            Location::Trash
            | Location::Recent
            | Location::Search { .. }
            | Location::Usage { .. } => None,
        };
        let mut segment = div()
            .id(("explorer-crumb", index))
            .h(px(22.0))
            .px(px(7.0))
            // Transparent until a drop is hovering (no layout shift).
            .border_1()
            .border_color(gpui::transparent_black())
            .flex()
            .items_center()
            .gap(px(6.0))
            .text_size(px(13.0))
            .text_color(if is_last { colors.strong } else { colors.muted })
            .on_click(cx.listener({
                let target = target.clone();
                move |this, _event, window, cx| {
                    // The segment is a target of its own: never the bar's edit
                    // click behind it.
                    cx.stop_propagation();
                    if !is_last {
                        this.navigate(Transition::Visit(target.clone()), window, cx);
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(move |this, _event, window, cx| {
                    cx.stop_propagation();
                    this.open_tab(target.clone(), Placement::Background, window, cx);
                }),
            );
        segment = if is_last {
            // The current folder keeps its room and clips last.
            segment.flex_shrink_0().max_w_full().truncate()
        } else {
            segment
                .min_w(px(18.0))
                .truncate()
                .cursor_pointer()
                .hover(|segment| segment.text_color(colors.foreground))
        };
        if crumb.kind == CrumbKind::Home {
            segment = segment.child(
                Icon::new(icon(gpui_kit_assets::IconName::House))
                    .size(px(14.0))
                    .color(if is_last { colors.strong } else { colors.muted }),
            );
        }
        if let Some(drop) = drop {
            segment = dnd::drop_target(segment, drop, &cx.entity().downgrade(), cx);
        }
        bar = bar.child(segment.child(crumb.label));
    }
    bar
}

/// One segment of the view toggle: a 28×24 icon, raised and tinted while
/// it is the active presentation.
fn view_segment(
    this: &Explorer,
    cx: &mut Context<Explorer>,
    id: &'static str,
    glyph: gpui_kit_assets::IconName,
    tip: &'static str,
    view: View,
) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    let pressed = this.tab().state.view() == view;
    let tint = ChromeColors::tint(cx.theme());
    div()
        .id(id)
        .w(px(28.0))
        .h(px(24.0))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .when(pressed, |segment| segment.bg(colors.raised))
        .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
        .on_click(cx.listener(move |this, _event, _window, cx| {
            this.set_view(view, cx);
        }))
        .child(Icon::new(icon(glyph)).size(px(15.0)).color(if pressed {
            tint
        } else {
            colors.muted
        }))
}

/// The live search count: "n of m", "no matches", or nothing without a
/// query.
fn search_count(this: &Explorer, _cx: &mut Context<Explorer>) -> Option<SearchCount> {
    // A subfolder search counts what it found; the status bar says more.
    if this.tab().state.search_query().is_some() {
        let state = &this.tab().state;
        // While the order of the results is made, all of them count.
        let found = if state.is_arranging() {
            state.listing().entries.len()
        } else {
            state.visible_shared().len()
        };
        let status = state.search_status();
        return Some(SearchCount::new(match (found, status.searching) {
            (0, true) => "searching\u{2026}".to_owned(),
            (0, false) => "no matches".to_owned(),
            (found, _) => format!("{found} found"),
        }));
    }
    let query = this.tab().state.query();
    if query.is_empty() {
        return None;
    }
    let total = this.tab().state.listing().entries.len();
    let shown = this.tab().state.visible_shared().len();
    if shown == 0 {
        return Some(SearchCount::new("no matches"));
    }
    if shown == total {
        return Some(SearchCount::new(format!("{total} of {total}")));
    }
    Some(SearchCount::new(format!("{shown} of {total}")))
}

#[cfg(test)]
mod tests {
    use crate::explorer::ops::{
        MenuCommand, MenuItemSpec, MenuRow, SearchOption, search_option_rows, toolbar_menu_rows,
    };
    use fileflux::core::search_match::QueryOptions;

    /// (label, command, checked, enabled) of every item row.
    fn items(rows: &[MenuRow]) -> Vec<(&'static str, MenuCommand, Option<bool>, bool)> {
        rows.iter()
            .filter_map(|row| match row {
                MenuRow::Item(spec) => Some((spec.label, spec.command, spec.checked, spec.enabled)),
                _ => None,
            })
            .collect()
    }

    fn spec_of<'a>(rows: &'a [MenuRow], label: &str) -> Option<&'a MenuItemSpec> {
        rows.iter().find_map(|row| match row {
            MenuRow::Item(spec) if spec.label == label => Some(spec),
            _ => None,
        })
    }

    /// The search options popover holds the three query toggles and the
    /// subfolder scope under its uppercase heading, checked from the live
    /// search state.
    #[test]
    fn the_search_options_popover_holds_the_four_toggles_with_checks() {
        let rows = search_option_rows(
            QueryOptions {
                match_case: true,
                regex: false,
                contents: true,
            },
            true,
            true,
        );
        assert_eq!(
            rows.first(),
            Some(&MenuRow::Header("SEARCH OPTIONS")),
            "the panel is headed by the uppercase label"
        );
        assert_eq!(
            items(&rows),
            vec![
                (
                    "Match case",
                    MenuCommand::SearchOption(SearchOption::MatchCase),
                    Some(true),
                    true
                ),
                (
                    "Regular expression",
                    MenuCommand::SearchOption(SearchOption::Regex),
                    Some(false),
                    true
                ),
                (
                    "File contents",
                    MenuCommand::SearchOption(SearchOption::Contents),
                    Some(true),
                    true
                ),
                (
                    "Subfolders",
                    MenuCommand::SearchOption(SearchOption::Subfolders),
                    Some(true),
                    true
                ),
            ]
        );
        assert_eq!(
            rows.iter()
                .filter(|row| **row == MenuRow::Separator)
                .count(),
            1,
            "the subfolder scope sits apart, under a separator"
        );
    }

    /// The subfolder scope stays disabled where the search cannot run, as
    /// the toolbar button it replaces was.
    #[test]
    fn the_subfolders_row_is_disabled_where_the_search_cannot_run() {
        let rows = search_option_rows(QueryOptions::default(), false, false);
        assert_eq!(
            spec_of(&rows, "Subfolders").map(|spec| (spec.checked, spec.enabled)),
            Some((Some(false), false)),
            "the scope row stays disabled where the search cannot run"
        );
    }

    /// The toolbar's ⋯ menu holds Split view, Favorite this folder, Refresh
    /// and Properties, each with the shortcut the app really binds.
    #[test]
    fn the_toolbar_menu_holds_split_favorite_refresh_and_properties() {
        let rows = toolbar_menu_rows(true);
        assert_eq!(rows.len(), 5);
        assert!(matches!(rows.get(1), Some(MenuRow::Separator)));
        assert_eq!(
            items(&rows),
            vec![
                ("Split view", MenuCommand::SplitView, None, true),
                (
                    "Favorite this folder",
                    MenuCommand::FavoriteFolder,
                    None,
                    true
                ),
                ("Refresh", MenuCommand::Reload, None, true),
                ("Properties", MenuCommand::Properties, None, true),
            ]
        );
        assert_eq!(
            spec_of(&rows, "Split view").and_then(|spec| spec.shortcut),
            Some("F3")
        );
        assert_eq!(
            spec_of(&rows, "Favorite this folder").and_then(|spec| spec.shortcut),
            None,
            "the app binds no key for favoriting"
        );
        assert_eq!(
            spec_of(&rows, "Refresh").and_then(|spec| spec.shortcut),
            Some("Ctrl+R")
        );
        assert_eq!(
            spec_of(&rows, "Properties").and_then(|spec| spec.shortcut),
            Some("Ctrl+I")
        );
    }

    /// The favorite row is refused where the star button it replaces was.
    #[test]
    fn the_favorite_row_is_disabled_where_favoriting_is_refused() {
        let rows = toolbar_menu_rows(false);
        assert!(spec_of(&rows, "Favorite this folder").is_some_and(|spec| !spec.enabled));
    }
}
