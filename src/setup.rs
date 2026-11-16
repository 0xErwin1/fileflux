//! The setup window: an honest stand-in that proves the themed window works.
//! It is not the file explorer; browsing comes next.

use bolt::primitives::Text;
use gpui::{Context, IntoElement, ParentElement, Render, Styled, Window, div, px};
use gpui_component::ActiveTheme;

use crate::theme;

/// The setup screen: a themed sidebar beside a content area that says what
/// this build is.
pub(crate) struct SetupScreen;

impl SetupScreen {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl Render for SetupScreen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The installed palette; the theme is already resolved by the time a
        // window renders (`theme::install` ran before the window opened).
        let colors = bolt::Colors::for_current(cx);
        let brand = theme::brand();
        let accent = if cx.theme().is_dark() {
            brand.accent_tint
        } else {
            brand.accent
        };

        div()
            .size_full()
            .flex()
            .bg(colors.desk)
            .text_color(colors.foreground)
            // Sidebar: where the places tree will go once browsing exists.
            .child(
                div()
                    .w(px(240.0))
                    .h_full()
                    .flex()
                    .flex_col()
                    .bg(colors.background)
                    .border_r_1()
                    .border_color(colors.line)
                    .p_3()
                    .child(Text::heading(brand.name.clone()).text_color(accent)),
            )
            // Content: an honest statement of the current scope.
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .bg(colors.panel)
                    .child(Text::title("Fileflux is ready"))
                    .child(Text::body("File browsing comes next.").muted_foreground()),
            )
    }
}
