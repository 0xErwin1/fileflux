//! The tab strip: a 30 px row at the top of the document island, drawn only
//! while two or more tabs are open.
//!
//! Square Bolt tabs (design/system.md: radius 0, 1 px lines, a 2 px tint
//! marker): the active tab sits on the raised fill with strong text and a
//! tint bar along its bottom edge; the others are muted text that lifts to
//! the raised fill on hover. A tab shows the folder's glyph and name (the
//! full path is its tooltip) and a close button while it is active or
//! hovered; a middle click closes it. Tabs share the row and shrink down to
//! a floor before they clip.

use bolt::primitives::Icon;
use bolt::tokens::ChromeColors;
use fileflux::core::Location;
use gpui::{Context, Div, MouseButton, SharedString, Stateful, Styled, div, prelude::*, px};
use gpui_component::ActiveTheme;
use gpui_component::tooltip::Tooltip;

use super::icon;
use super::tabs::{tab_label, tab_tooltip};
use super::view::Explorer;

/// Height of the strip and of every tab.
pub(crate) const TAB_HEIGHT: f32 = 30.0;
/// The narrowest a tab shrinks to before the row clips.
const TAB_MIN_WIDTH: f32 = 96.0;
/// The widest a tab grows to.
const TAB_MAX_WIDTH: f32 = 220.0;
/// Edge of the square close button.
const CLOSE_SIZE: f32 = 18.0;

/// The strip, or nothing while there is a single tab.
pub(crate) fn render(this: &Explorer, cx: &mut Context<Explorer>) -> Option<Stateful<Div>> {
    if this.tabs.len() < 2 {
        return None;
    }
    let colors = bolt::colors(cx);
    let home = this
        .places
        .as_ref()
        .map(|places| places.home.as_path())
        .or(this.home.as_deref());
    let mut strip = div()
        .id("explorer-tabs")
        .flex_none()
        .flex()
        .items_stretch()
        .h(px(TAB_HEIGHT))
        .overflow_hidden()
        .border_b_1()
        .border_color(colors.line);
    let active = this.tabs.active_index();
    for (index, tab) in this.tabs.focused().iter().enumerate() {
        let location = tab.state.current();
        strip = strip.child(render_tab(
            index,
            index == active,
            location,
            tab_label(location, home),
            tab_tooltip(location, this.trash_root.as_deref()),
            cx,
        ));
    }
    Some(strip)
}

/// One tab.
fn render_tab(
    index: usize,
    active: bool,
    location: &Location,
    label: String,
    tooltip: String,
    cx: &mut Context<Explorer>,
) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    let tint = ChromeColors::tint(cx.theme());
    let group: SharedString = format!("explorer-tab-group-{index}").into();
    let glyph = match location {
        Location::Trash => gpui_kit_assets::IconName::Trash,
        Location::Recent => gpui_kit_assets::IconName::Clock,
        Location::Directory(_) => gpui_kit_assets::IconName::Folder,
        Location::Search { .. } => gpui_kit_assets::IconName::FolderSearch,
    };
    let close_group = group.clone();
    div()
        .id(("explorer-tab", index))
        .group(group.clone())
        .relative()
        .flex_1()
        .min_w(px(TAB_MIN_WIDTH))
        .max_w(px(TAB_MAX_WIDTH))
        .h_full()
        .flex()
        .items_center()
        .gap(px(8.0))
        .pl(px(12.0))
        .pr(px(6.0))
        .border_r_1()
        .border_color(colors.line)
        .text_size(px(13.0))
        .cursor_pointer()
        .when(active, |tab| {
            tab.bg(colors.raised).text_color(colors.strong)
        })
        .when(!active, |tab| {
            tab.text_color(colors.muted)
                .hover(|tab| tab.bg(colors.raised).text_color(colors.foreground))
        })
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        .on_click(cx.listener(move |this, _event, window, cx| {
            this.activate_tab(index, window, cx);
        }))
        .on_mouse_up(
            MouseButton::Middle,
            cx.listener(move |this, _event, window, cx| {
                this.close_tab(index, window, cx);
            }),
        )
        .child(Icon::new(icon(glyph)).size(px(14.0)).color(if active {
            tint
        } else {
            colors.muted
        }))
        .child(div().flex_1().min_w_0().truncate().child(label))
        .child(
            div()
                .id(("explorer-tab-close", index))
                .flex_none()
                .size(px(CLOSE_SIZE))
                .flex()
                .items_center()
                .justify_center()
                .text_color(colors.muted)
                .hover(|button| button.bg(colors.line).text_color(colors.strong))
                // Hidden (not removed) until the tab is active or hovered,
                // so the label never shifts.
                .when(!active, |button| {
                    button
                        .invisible()
                        .group_hover(close_group, |button| button.visible())
                })
                .tooltip(|window, cx| Tooltip::new("Close tab (Ctrl+W)").build(window, cx))
                .on_click(cx.listener(move |this, _event, window, cx| {
                    cx.stop_propagation();
                    this.close_tab(index, window, cx);
                }))
                .child(Icon::new(icon(gpui_kit_assets::IconName::X)).size(px(12.0))),
        )
        .when(active, |tab| {
            // The 2 px tint marker along the bottom edge.
            tab.child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .h(px(2.0))
                    .bg(tint),
            )
        })
}
