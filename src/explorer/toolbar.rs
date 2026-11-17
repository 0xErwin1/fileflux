//! The toolbar of the document island: history buttons with disabled
//! states, the editable current path (Ctrl+L), the current-folder filename
//! search (Ctrl+F, with its live match count), the grid/list view toggle,
//! and the session-favorites star.
//!
//! The controls keep their sizes from `bolt`'s tokens; on narrow windows
//! they shrink with flex instead of disappearing, and a toggle parks the
//! sidebar.

use bolt::controls::{Button, InputEscape, SearchCount, SearchField};
use bolt::primitives::Icon;
use bolt::tokens::Spacing;
use fileflux::core::Location;
use gpui::{Context, Div, Stateful, Styled, div, prelude::*, px};

use super::icon;
use super::state::View;
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
        // On narrow windows this toggle parks the sidebar; on wide ones the
        // sidebar keeps its column and the toggle does nothing visually.
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
                            this.navigate(super::state::Transition::Back, window, cx);
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
                            this.navigate(super::state::Transition::Forward, window, cx);
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
                            this.navigate(super::state::Transition::Up, window, cx);
                        })),
                ),
        )
        // The editable current path. The frame captures the input's own
        // Escape (`bolt_input::Escape`), which the input consumes before it
        // would ever bubble: here Escape restores the committed path and
        // gives the focus back to the entries area (reqflux sidebar filter
        // pattern).
        .child(
            div()
                .id("explorer-path-frame")
                .flex_1()
                .min_w(px(160.0))
                .capture_action(cx.listener(|this, _: &InputEscape, window, cx| {
                    this.restore_path_on_escape(window, cx);
                    cx.stop_propagation();
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
                ),
        )
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
        // Grid/list view toggle: selection state per button.
        .child(
            div()
                .id("explorer-view-toggle")
                .flex_none()
                .flex()
                .gap(Spacing::XS)
                .child(
                    Button::new("explorer-view-grid", "Icon view")
                        .ghost()
                        .icon_only()
                        .icon(icon(gpui_kit_assets::IconName::LayoutGrid))
                        .tooltip("Icon view")
                        .selected(this.state.view() == View::Grid)
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.state.set_view(View::Grid);
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("explorer-view-list", "List view")
                        .ghost()
                        .icon_only()
                        .icon(icon(gpui_kit_assets::IconName::List))
                        .tooltip("List view")
                        .selected(this.state.view() == View::List)
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.state.set_view(View::List);
                            cx.notify();
                        })),
                ),
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
                    this.navigate(super::state::Transition::Visit(current), window, cx);
                })),
        )
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
