//! The toolbar of the document island, six groups: the sidebar hamburger
//! (only on the leftmost pane while the sidebar is hidden), the
//! history buttons with disabled states, the breadcrumb path (Ctrl+L or a
//! click on its empty space edits it), the current-folder filename search
//! (Ctrl+F, with its live match count and its options behind a filter
//! button at the field's trailing edge), the segmented grid/list toggle and
//! one `⋯` menu holding Split view, Favorite this folder, Refresh and
//! Properties, and — in a split view — the pane's close control.
//!
//! The controls keep their sizes from `bolt`'s tokens and none is ever cut:
//! [`plan`] decides from the pane's width whether the six groups fit one row
//! (the breadcrumbs and the search field give way first, down to
//! [`CRUMBS_MIN_WIDTH`] and [`SEARCH_MIN_WIDTH`]) or the search field and the
//! view switch move to a second row under the path. The breadcrumb bar
//! collapses its leading segments into a "…" before it would clip an icon
//! (see [`collapse_crumbs`]).

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

/// The narrowest the breadcrumb bar and the search field get before the
/// search field and the view switch move to a second row. The bar holds a
/// Home icon, a "…", a chevron and the start of the current folder; the field
/// its icon, some text and the filter button.
pub(crate) const CRUMBS_MIN_WIDTH: f32 = 120.0;
pub(crate) const SEARCH_MIN_WIDTH: f32 = 150.0;
/// The search field's natural width (13.75 rem) while the row has room.
const SEARCH_NATURAL_WIDTH: f32 = 220.0;
/// The toolbar row's left and right padding (14 + 10) and the gap between
/// its controls (`Spacing::XXS`).
const ROW_PADDING: f32 = 24.0;
const ROW_GAP: f32 = 6.0;
/// An icon-only button (`ButtonMetrics::ICON_ONLY_WIDTH`).
const BUTTON_WIDTH: f32 = 32.0;
/// Back, forward and up: three buttons 4 px apart.
const NAV_WIDTH: f32 = 3.0 * BUTTON_WIDTH + 2.0 * 4.0;
/// The view switch: three 28 px segments 2 px apart in a 2 px padded, 1 px
/// bordered frame.
const VIEW_TOGGLE_WIDTH: f32 = 3.0 * 28.0 + 2.0 * 2.0 + 2.0 * 2.0 + 2.0;
/// The breadcrumb bar's own border and horizontal padding.
const CRUMB_BAR_INSET: f32 = 2.0 + 2.0 * 4.0;

/// Where the toolbar's controls go at a pane width.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Plan {
    /// The search field and the view switch sit on a second row under the
    /// path: the pane is too narrow for one.
    pub(crate) two_rows: bool,
    /// The width the breadcrumb bar gets.
    pub(crate) crumbs_width: f32,
}

/// How many controls sit on a row, and the width they take together with
/// the row's padding and gaps.
fn row_width(controls: usize, widths: f32) -> f32 {
    ROW_PADDING + widths + ROW_GAP * controls.saturating_sub(1) as f32
}

/// The width of what `chrome` puts on the path's row besides the path (the
/// hamburger, the history buttons, the `⋯` button and the close control),
/// and how many controls that is, the path included.
fn path_row_extras(chrome: Chrome) -> (usize, f32) {
    let hamburger = usize::from(chrome.sidebar_toggle);
    let close = usize::from(chrome.close.is_some());
    // The path, the history buttons, the `⋯` button.
    let controls = 3 + hamburger + close;
    let widths = NAV_WIDTH + BUTTON_WIDTH * (1 + hamburger + close) as f32;
    (controls, widths)
}

/// Lays the toolbar out in a pane `width` pixels wide: one row when the path,
/// the search field and the view switch each get their minimum, else the
/// search field and the view switch on a second row. Nothing is ever wider
/// than the row it is on, from [`two_row_floor`] up.
pub(crate) fn plan(width: f32, chrome: Chrome) -> Plan {
    let (path_controls, path_widths) = path_row_extras(chrome);
    let one_row = row_width(
        path_controls + 2,
        path_widths + CRUMBS_MIN_WIDTH + SEARCH_MIN_WIDTH + VIEW_TOGGLE_WIDTH,
    );
    if width >= one_row {
        // The search field keeps its natural width while the bar keeps its
        // minimum; below that it shrinks.
        let without_path = row_width(path_controls + 2, path_widths + VIEW_TOGGLE_WIDTH);
        let crumbs = (width - without_path - SEARCH_NATURAL_WIDTH).max(CRUMBS_MIN_WIDTH);
        return Plan {
            two_rows: false,
            crumbs_width: crumbs,
        };
    }
    let without_path = row_width(path_controls, path_widths);
    Plan {
        two_rows: true,
        crumbs_width: (width - without_path).max(CRUMBS_MIN_WIDTH),
    }
}

/// The narrowest pane the toolbar fits without clipping anything: the
/// wider of its two rows when it is on two.
#[cfg(test)]
pub(crate) fn two_row_floor(chrome: Chrome) -> f32 {
    let (path_controls, path_widths) = path_row_extras(chrome);
    let path_row = row_width(path_controls, path_widths + CRUMBS_MIN_WIDTH);
    let search_row = row_width(2, SEARCH_MIN_WIDTH + VIEW_TOGGLE_WIDTH);
    path_row.max(search_row)
}

// --- breadcrumb collapse ---------------------------------------------------------------------

/// What a breadcrumb segment's text takes per character at 13 px: an
/// estimate that errs wide, so a bar that fits by it fits for real.
const CRUMB_CHAR_WIDTH: f32 = 7.0;
/// A segment's padding (7 + 7) and border (1 + 1).
const CRUMB_PADDING: f32 = 16.0;
/// A chevron separator: segments sit directly against it.
const CRUMB_CHEVRON: f32 = 12.0;
/// The Home icon (14) and its gap to the label (6).
const CRUMB_HOME_ICON: f32 = 20.0;
/// The "…" segment that stands for the hidden leading ones.
const CRUMB_ELLIPSIS: f32 = CRUMB_PADDING + 1.0 * CRUMB_CHAR_WIDTH + 2.0;

/// The width a breadcrumb segment takes: its label, its Home icon when it
/// has one, and its padding.
pub(crate) fn crumb_width(label: &str, home_icon: bool) -> f32 {
    let icon = if home_icon { CRUMB_HOME_ICON } else { 0.0 };
    CRUMB_PADDING + icon + label.chars().count() as f32 * CRUMB_CHAR_WIDTH
}

/// How many leading segments of a path (`widths`, in order) go behind a "…"
/// so the rest fits `budget`. The current folder (the last one) is never
/// hidden: when it alone is wider than the bar its label truncates, but no
/// icon is ever cut. Nothing is hidden when everything fits.
pub(crate) fn collapse_crumbs(widths: &[f32], budget: f32) -> usize {
    let separators = |count: usize| CRUMB_CHEVRON * count.saturating_sub(1) as f32;
    let total: f32 = widths.iter().sum::<f32>() + separators(widths.len());
    if total <= budget {
        return 0;
    }
    // Otherwise the "…" and its chevron take their place, and segments are
    // kept from the end while they fit.
    let Some((last, earlier)) = widths.split_last() else {
        return 0;
    };
    let mut used = CRUMB_ELLIPSIS + CRUMB_CHEVRON + last;
    let mut kept = 1;
    for width in earlier.iter().rev() {
        if used + CRUMB_CHEVRON + width > budget {
            break;
        }
        used += CRUMB_CHEVRON + width;
        kept += 1;
    }
    widths.len() - kept
}

/// What a pane's toolbar carries besides its own controls.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Chrome {
    /// The hamburger that shows the hidden sidebar (leftmost pane only).
    pub(crate) sidebar_toggle: bool,
    /// The pane this toolbar closes with its × control, in a split view.
    pub(crate) close: Option<Side>,
    /// The width of the pane the toolbar is in.
    pub(crate) pane_width: f32,
}

/// The toolbar: one row, or two in a pane too narrow for one (see [`plan`]),
/// never wider than its pane.
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
    let plan = plan(chrome.pane_width, chrome);

    // The hamburger that brings the hidden sidebar back: the first
    // control of the leftmost pane only (the sidebar has its own).
    let hamburger = chrome.sidebar_toggle.then(|| {
        Button::new("explorer-sidebar-show", "Show sidebar")
            .ghost()
            .icon_only()
            .icon(icon(gpui_kit_assets::IconName::Menu))
            .tooltip("Show sidebar")
            .on_click(cx.listener(|this, _event, window, cx| {
                this.toggle_sidebar(window, cx);
            }))
            .into_any_element()
    });
    let nav = nav_buttons(this, cx);
    // The current path: breadcrumbs, or — after a click on the bar's
    // empty space or Ctrl+L — the editable field. The frame captures
    // the input's own Escape (`bolt_input::Escape`), which the input
    // consumes before it would ever bubble: here Escape restores the
    // committed path, returns to the breadcrumbs and gives the focus
    // back to the entries area (reqflux sidebar filter pattern).
    let path = if this.tab().path_editing {
        path_editor(this, cx).into_any_element()
    } else {
        crumb_bar(this, plan.crumbs_width, cx).into_any_element()
    };
    let search = search_frame(this, plan, search_options_open, cx);
    // An invalid regular expression: why nothing ran. It gives way (and
    // ends in an ellipsis) before it pushes a control out of the row.
    let search_error = this.tab().search_error.clone().map(|error| {
        div()
            .id("explorer-search-error")
            .flex_shrink(1.0)
            .min_w_0()
            .max_w(px(260.0))
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_color(colors.danger)
            .child(error)
            .into_any_element()
    });
    let view_toggle = view_toggle(this, cx);
    // The one overflow control: Split view, Favorite this folder,
    // Refresh and Properties, each with its shortcut. It stays raised
    // while its menu is open.
    let more = Button::new("explorer-more", "More actions")
        .ghost()
        .icon_only()
        .icon(icon(gpui_kit_assets::IconName::Ellipsis))
        .tooltip("Split view, favorite this folder, refresh and properties")
        .selected(toolbar_menu_open)
        .toggled(toolbar_menu_open)
        .on_click(cx.listener(|this, event: &gpui::ClickEvent, window, cx| {
            let position = super::overlay::below(event.position());
            this.open_toolbar_menu(position, window, cx);
        }))
        .into_any_element();
    // In a split view each pane closes itself; the other one stays.
    let close = chrome.close.map(|side| {
        Button::new("explorer-close-pane", "Close pane")
            .ghost()
            .icon_only()
            .icon(icon(gpui_kit_assets::IconName::X))
            .tooltip("Close this pane")
            .on_click(cx.listener(move |this, _event, window, cx| {
                this.close_pane(side, window, cx);
            }))
            .into_any_element()
    });

    let row = || {
        div()
            .w_full()
            .min_w_0()
            .overflow_hidden()
            .flex()
            .items_center()
            .gap(Spacing::XXS)
            .pl(px(14.0))
            .pr(px(10.0))
            .min_h(px(50.0))
    };
    let toolbar = div()
        .id("explorer-toolbar")
        .flex_none()
        .w_full()
        .min_w_0()
        .overflow_hidden()
        .flex()
        .flex_col()
        .border_b_1()
        .border_color(colors.line);
    if plan.two_rows {
        toolbar
            .child(
                row()
                    .children(hamburger)
                    .child(nav)
                    .child(path)
                    .child(more)
                    .children(close),
            )
            .child(
                row()
                    .min_h(px(44.0))
                    .child(search)
                    .children(search_error)
                    .child(view_toggle),
            )
    } else {
        toolbar.child(
            row()
                .children(hamburger)
                .child(nav)
                .child(path)
                .child(search)
                .children(search_error)
                .child(view_toggle)
                .child(more)
                .children(close),
        )
    }
}

/// History: back, forward, up — with honest disabled states.
fn nav_buttons(this: &Explorer, cx: &mut Context<Explorer>) -> gpui::AnyElement {
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
        )
        .into_any_element()
}

/// Current-folder filename search with its live count, and its options
/// behind the filter button at the field's trailing edge. The frame wrapping
/// the field captures the input's own Escape here too: it clears the query,
/// restores the full listing and returns the entries focus. On one row it
/// keeps its natural width (13.75 rem) and shrinks to its minimum; on the
/// second row it fills what the view switch leaves.
fn search_frame(
    this: &Explorer,
    plan: Plan,
    options_open: bool,
    cx: &mut Context<Explorer>,
) -> gpui::AnyElement {
    let frame = div().id("explorer-search-frame").flex();
    let frame = if plan.two_rows {
        frame.flex_1()
    } else {
        frame.w(gpui::rems(13.75))
    };
    frame
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
                .trailing(search_filter_button(this, cx, options_open)),
        )
        .into_any_element()
}

/// Grid/list/compact view: one segmented control, the pressed segment raised
/// and tinted.
fn view_toggle(this: &Explorer, cx: &mut Context<Explorer>) -> gpui::AnyElement {
    let colors = bolt::colors(cx);
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
        ))
        .into_any_element()
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
/// When the path does not fit `width` its leading segments collapse into a
/// "…" (which goes to the nearest hidden folder) instead of being clipped,
/// and an icon is never cut; a click on the bar's empty space opens the path
/// editor.
fn crumb_bar(this: &Explorer, width: f32, cx: &mut Context<Explorer>) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    let home = this.places.as_ref().map(|places| places.home.as_path());
    let crumbs = util::breadcrumbs(this.tab().state.current(), home);
    let last = crumbs.len().saturating_sub(1);
    let widths: Vec<f32> = crumbs
        .iter()
        .map(|crumb| crumb_width(&crumb.label, crumb.kind == CrumbKind::Home))
        .collect();
    let hidden = collapse_crumbs(&widths, width - CRUMB_BAR_INSET);

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

    let chevron = || {
        div().flex_none().child(
            Icon::new(icon(gpui_kit_assets::IconName::ChevronRight))
                .size(px(12.0))
                .color(colors.line_2),
        )
    };
    // The path's own "…" (see `util::breadcrumbs`) already stands for the
    // folders it hides: when it is the first one shown, it is the one.
    let own_ellipsis = crumbs
        .get(hidden)
        .is_some_and(|crumb| crumb.kind == CrumbKind::Ellipsis);
    if hidden > 0
        && !own_ellipsis
        && let Some(nearest) = crumbs.get(hidden - 1).map(|crumb| crumb.target.clone())
    {
        // The hidden leading segments: one click goes to the closest.
        bar = bar.child(
            div()
                .id("explorer-crumb-more")
                .flex_none()
                .h(px(22.0))
                .px(px(7.0))
                .flex()
                .items_center()
                .text_size(px(13.0))
                .text_color(colors.muted)
                .cursor_pointer()
                .hover(|segment| segment.text_color(colors.foreground))
                .tooltip(|window, cx| {
                    Tooltip::new("Show the folders on the way to here").build(window, cx)
                })
                .on_click(cx.listener(move |this, _event, window, cx| {
                    cx.stop_propagation();
                    this.navigate(Transition::Visit(nearest.clone()), window, cx);
                }))
                .child("\u{2026}"),
        );
    }
    for (index, crumb) in crumbs.into_iter().enumerate().skip(hidden) {
        if index > 0 {
            bar = bar.child(chevron());
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
            // The current folder is the only segment whose text gives way: it
            // ends in an ellipsis, its icon and the separators stay whole.
            segment.flex_shrink(1.0).min_w_0().max_w_full()
        } else {
            segment
                .flex_none()
                .cursor_pointer()
                .hover(|segment| segment.text_color(colors.foreground))
        };
        if crumb.kind == CrumbKind::Home {
            segment = segment.child(
                div().flex_none().child(
                    Icon::new(icon(gpui_kit_assets::IconName::House))
                        .size(px(14.0))
                        .color(if is_last { colors.strong } else { colors.muted }),
                ),
            );
        }
        if let Some(drop) = drop {
            segment = dnd::drop_target(segment, drop, &cx.entity().downgrade(), cx);
        }
        bar = bar.child(
            segment.child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(crumb.label),
            ),
        );
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
    use super::*;
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

    fn chromes() -> Vec<Chrome> {
        let mut all = Vec::new();
        for sidebar_toggle in [false, true] {
            for close in [None, Some(Side::Left)] {
                all.push(Chrome {
                    sidebar_toggle,
                    close,
                    pane_width: 0.0,
                });
            }
        }
        all
    }

    /// The width the plan's rows need at the width the plan gives the path.
    fn needed_widths(plan: Plan, chrome: Chrome) -> Vec<f32> {
        let (path_controls, path_widths) = path_row_extras(chrome);
        if plan.two_rows {
            vec![
                row_width(path_controls, path_widths + plan.crumbs_width),
                row_width(2, SEARCH_MIN_WIDTH + VIEW_TOGGLE_WIDTH),
            ]
        } else {
            vec![row_width(
                path_controls + 2,
                path_widths + plan.crumbs_width + SEARCH_MIN_WIDTH + VIEW_TOGGLE_WIDTH,
            )]
        }
    }

    #[test]
    fn no_control_is_wider_than_its_row_at_any_pane_width_from_the_floor_up() {
        for chrome in chromes() {
            let floor = two_row_floor(chrome);
            for width in (floor as u32)..=3000 {
                let width = width as f32;
                let plan = plan(
                    width,
                    Chrome {
                        pane_width: width,
                        ..chrome
                    },
                );
                assert!(
                    plan.crumbs_width >= CRUMBS_MIN_WIDTH,
                    "{chrome:?} at {width}"
                );
                for needed in needed_widths(plan, chrome) {
                    assert!(
                        needed <= width,
                        "{chrome:?} at {width}: a row needs {needed} ({plan:?})"
                    );
                }
            }
        }
    }

    #[test]
    fn the_panes_the_split_accepts_fit_their_toolbar() {
        use bolt::tokens::IslandMetrics;
        use fileflux::core::settings::DEFAULT_SIDEBAR_WIDTH;
        let gap = IslandMetrics::GAP.as_f32();
        // The window widths the split accepts, with the sidebar docked at its
        // default width: each pane is half of what the shell leaves.
        for window in [1100.0_f32, 1130.0, 1260.0, 1500.0] {
            let shell = 2.0 * gap + DEFAULT_SIDEBAR_WIDTH as f32 + gap;
            let (left, right) = crate::explorer::panes::pane_widths(window - shell - gap, 0.5);
            for chrome in chromes() {
                for pane in [left, right] {
                    assert!(
                        two_row_floor(chrome) <= pane,
                        "{window} px window: a {pane} px pane is below the toolbar's floor {}",
                        two_row_floor(chrome)
                    );
                }
            }
        }
    }

    #[test]
    fn a_wide_pane_keeps_one_row_and_a_narrow_one_moves_search_and_view_down() {
        let chrome = Chrome {
            sidebar_toggle: false,
            close: None,
            pane_width: 0.0,
        };
        assert!(!plan(1200.0, chrome).two_rows, "a single wide pane");
        assert!(plan(469.0, chrome).two_rows, "half of a 1260 px window");
        let wide = plan(1200.0, chrome);
        assert!(
            wide.crumbs_width > CRUMBS_MIN_WIDTH,
            "the bar takes what the search field's natural width leaves"
        );
    }

    #[test]
    fn a_path_that_fits_is_not_collapsed() {
        let widths = [60.0, 50.0, 70.0];
        assert_eq!(collapse_crumbs(&widths, 500.0), 0);
        assert_eq!(collapse_crumbs(&[], 10.0), 0);
        assert_eq!(collapse_crumbs(&[80.0], 10.0), 0, "a lone segment stays");
    }

    #[test]
    fn a_long_path_hides_leading_segments_behind_an_ellipsis_and_keeps_the_current_folder() {
        let widths = [60.0, 80.0, 70.0, 90.0, 100.0];
        let total: f32 = widths.iter().sum::<f32>() + CRUMB_CHEVRON * 4.0;
        assert_eq!(collapse_crumbs(&widths, total), 0, "exactly fits");
        for budget in 150..total as u32 {
            let budget = budget as f32;
            let hidden = collapse_crumbs(&widths, budget);
            assert!((1..widths.len()).contains(&hidden), "budget {budget}");
            let shown = widths.get(hidden..).unwrap_or_default();
            let used = CRUMB_ELLIPSIS + shown.iter().map(|w| CRUMB_CHEVRON + w).sum::<f32>();
            assert!(
                used <= budget,
                "budget {budget}: {hidden} hidden uses {used}"
            );
            // Nothing more could have been kept.
            if hidden > 1 {
                assert!(
                    used + CRUMB_CHEVRON + widths.get(hidden - 1).copied().unwrap_or_default()
                        > budget,
                    "budget {budget}: {hidden} hidden is too many"
                );
            }
        }
        // Narrower than the current folder alone: it stays, its label gives way.
        assert_eq!(collapse_crumbs(&widths, 20.0), 4);
    }

    #[test]
    fn a_crumb_is_as_wide_as_its_label_icon_and_padding() {
        assert!(crumb_width("Home", true) > crumb_width("Home", false));
        assert!(crumb_width("Documents", false) > crumb_width("Doc", false));
        assert!(
            crumb_width("", true) >= CRUMB_PADDING + CRUMB_HOME_ICON,
            "the Home icon always has its room"
        );
    }
}
