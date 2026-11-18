//! The toolbar of the document island: history buttons with disabled
//! states, the breadcrumb path (Ctrl+L or a click on its empty space edits
//! it), the current-folder filename
//! search (Ctrl+F, with its live match count), the segmented grid/list toggle,
//! the details-panel toggle and the session-favorites star.
//!
//! The controls keep their sizes from `bolt`'s tokens; on narrow windows
//! they shrink with flex instead of disappearing, and the sidebar toggle
//! hides or shows the sidebar at any width.

use bolt::controls::{Button, InputEscape, SearchCount, SearchField};
use bolt::primitives::Icon;
use bolt::tokens::{ChromeColors, Spacing};
use fileflux::core::Location;
use gpui::{Context, Div, Stateful, Styled, div, prelude::*, px};
use gpui_component::ActiveTheme as _;
use gpui_component::tooltip::Tooltip;

use super::icon;
use super::state::{Transition, View};
use super::util::{self, CrumbKind};
use super::view::Explorer;

/// The toolbar row, wrapped rather than clipped.
pub(crate) fn render(this: &Explorer, cx: &mut Context<Explorer>) -> Stateful<Div> {
    let colors = bolt::colors(cx);

    div()
        .id("explorer-toolbar")
        .flex_none()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(Spacing::SM)
        .px(Spacing::LG)
        .py(Spacing::MD)
        .border_b_1()
        .border_color(colors.line)
        // The sidebar toggle works at every width: it hides or shows the
        // sidebar, and the content island takes the freed space.
        .child(
            Button::new("explorer-sidebar-toggle", "Sidebar")
                .ghost()
                .icon_only()
                .icon(icon(gpui_kit_assets::IconName::PanelLeft))
                .tooltip("Show or hide the places sidebar")
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.sidebar_open = !this.sidebar_open;
                    cx.notify();
                })),
        )
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
                        .disabled(!this.state.navigation().can_back())
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
                        .disabled(!this.state.navigation().can_forward())
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
                        .disabled(!this.state.navigation().can_up())
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
        .child(if this.path_editing {
            path_editor(this, cx).into_any_element()
        } else {
            crumb_bar(this, cx).into_any_element()
        })
        // Current-folder filename search with its live count. The frame
        // wrapping the field captures the input's own Escape here too: it
        // clears the query, restores the full listing and returns the
        // entries focus.
        .child(
            div()
                .id("explorer-search-frame")
                .flex_none()
                .capture_action(cx.listener(|this, _: &InputEscape, window, cx| {
                    this.clear_search_on_escape(window, cx);
                    cx.stop_propagation();
                }))
                .child(
                    SearchField::new("explorer-search", &this.search_input)
                        .aria_label("Search names in current folder")
                        .width(gpui::rems(13.125))
                        .count(search_count(this, cx)),
                ),
        )
        // Grid/list view: one segmented control, the pressed segment raised
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
                )),
        )
        // The details panel toggle: tinted while the panel is open.
        .child(
            Button::new("explorer-inspector-toggle", "Details panel")
                .ghost()
                .icon_only()
                .icon(icon(gpui_kit_assets::IconName::PanelRight))
                .tooltip("Show or hide the details panel (Ctrl+I)")
                .selected(this.inspector_open)
                .toggled(this.inspector_open)
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.inspector_open = !this.inspector_open;
                    cx.notify();
                })),
        )
        // The session favorites toggle for the current directory.
        .child(
            Button::new("explorer-favorite", "Favorite")
                .ghost()
                .icon_only()
                .icon(icon(if this.state.is_favorite() {
                    gpui_kit_assets::IconName::StarFill
                } else {
                    gpui_kit_assets::IconName::Star
                }))
                .tooltip("Add or remove the current folder as a session-only favorite")
                .disabled(this.state.current() == &Location::Trash)
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.toggle_favorite(cx);
                })),
        )
        // Reload, the recoverable-error path.
        .child(
            Button::new("explorer-reload", "Reload")
                .ghost()
                .icon_only()
                .icon(icon(gpui_kit_assets::IconName::RefreshCw))
                .tooltip("Reload current folder (Ctrl+R)")
                .on_click(cx.listener(|this, _event, window, cx| {
                    let current = this.state.current().clone();
                    this.navigate(Transition::Visit(current), window, cx);
                })),
        )
        // The context menu for keyboard-less pointer users: the same menu
        // as a right click, on the current selection or the folder.
        .child(
            Button::new("explorer-more", "More actions")
                .ghost()
                .icon_only()
                .icon(icon(gpui_kit_assets::IconName::Ellipsis))
                .tooltip("More actions (right click, Menu key or Shift+F10)")
                .on_click(cx.listener(|this, event: &gpui::ClickEvent, window, cx| {
                    let position = super::overlay::below(event.position());
                    let on = this.state.activation_candidate();
                    this.open_context_menu(position, on.as_deref(), window, cx);
                })),
        )
}

/// The editable path field in the breadcrumb bar's place.
fn path_editor(this: &Explorer, cx: &mut Context<Explorer>) -> Stateful<Div> {
    div()
        .id("explorer-path-frame")
        .flex_1()
        .min_w(px(160.0))
        .h(px(30.0))
        .capture_action(cx.listener(|this, _: &InputEscape, window, cx| {
            this.restore_path_on_escape(window, cx);
            cx.stop_propagation();
        }))
        .on_mouse_down_out(cx.listener(|this, _event, window, cx| {
            this.leave_path_edit(window, cx);
        }))
        .child(
            bolt::controls::Input::new(&this.path_input)
                .id("explorer-path-input")
                .w_full()
                .aria_label("Current folder path")
                .prefix(Icon::new(icon(gpui_kit_assets::IconName::Folder)))
                .error(
                    this.state
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
    let crumbs = util::breadcrumbs(this.state.current(), home);
    let last = crumbs.len().saturating_sub(1);

    let mut bar = div()
        .id("explorer-crumbs")
        .flex_1()
        .min_w(px(160.0))
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
        let mut segment = div()
            .id(("explorer-crumb", index))
            .h(px(22.0))
            .px(px(7.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .text_size(px(13.0))
            .text_color(if is_last { colors.strong } else { colors.muted })
            .on_click(cx.listener(move |this, _event, window, cx| {
                // The segment is a target of its own: never the bar's edit
                // click behind it.
                cx.stop_propagation();
                if !is_last {
                    this.navigate(Transition::Visit(target.clone()), window, cx);
                }
            }));
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
    let pressed = this.state.view() == view;
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
    let query = this.state.query();
    if query.is_empty() {
        return None;
    }
    let total = this.state.listing().entries.len();
    let shown = this.state.visible_indices().len();
    if shown == 0 {
        return Some(SearchCount::new("no matches"));
    }
    if shown == total {
        return Some(SearchCount::new(format!("{total} of {total}")));
    }
    Some(SearchCount::new(format!("{shown} of {total}")))
}
