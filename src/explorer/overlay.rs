//! What floats above the explorer: the context menu, the confirmation
//! dialog, and the inline rename editor the entry views embed.
//!
//! All three reuse Bolt: the menu is `bolt::composites` menu rows in
//! Bolt's menu frame under its dismiss overlay, the dialog is
//! `bolt::modals::Modal` with `Button`s, and the editor is Bolt's `Input`.

use bolt::composites::{
    MenuItem, render_menu_container, render_menu_item, render_menu_overlay, render_separator,
};
use bolt::controls::{Button, Input, InputEnter, InputEscape};
use bolt::modals::Modal;
use bolt::tokens::Spacing;
use gpui::{
    Anchor, AnyElement, App, Context, InteractiveElement as _, IntoElement as _, MouseButton,
    ParentElement as _, Styled as _, WeakEntity, Window, anchored, deferred, div, point,
    prelude::FluentBuilder as _, px,
};

use super::actions::RenameView;
use super::icon;
use super::ops::{MenuCommand, MenuRow};
use super::view::{Explorer, MENU_CONTEXT};

/// The menu and the dialog, when open. Also hands the keyboard back to the
/// entries when the menu or dialog was closed by something that had no
/// window to do it with (a click on the scrim).
pub(crate) fn render(
    this: &mut Explorer,
    window: &mut Window,
    cx: &mut Context<Explorer>,
) -> Vec<AnyElement> {
    if this.menu.is_none() && this.menu_focus.is_focused(window) {
        this.entries_focus.focus(window, cx);
    }
    if this.dialog.is_none() && this.dialog_focus.contains_focused(window, cx) {
        this.entries_focus.focus(window, cx);
    }
    let mut layers = Vec::new();
    layers.extend(render_menu(this, cx));
    layers.extend(render_dialog(this, cx));
    layers
}

fn command_icon(command: MenuCommand) -> gpui_kit_assets::IconName {
    use gpui_kit_assets::IconName;
    match command {
        MenuCommand::Open => IconName::FolderOpen,
        MenuCommand::Rename => IconName::Pencil,
        MenuCommand::CopyPath | MenuCommand::CopyFolderPath => IconName::Link,
        MenuCommand::Properties => IconName::Info,
        MenuCommand::MoveToTrash | MenuCommand::DeletePermanently | MenuCommand::EmptyTrash => {
            IconName::Trash
        }
        MenuCommand::NewFolder => IconName::FolderPlus,
        MenuCommand::NewFile => IconName::FilePlus,
        MenuCommand::OpenTerminal => IconName::Terminal,
        MenuCommand::Restore => IconName::Undo2,
    }
}

fn render_menu(this: &Explorer, cx: &mut Context<Explorer>) -> Vec<AnyElement> {
    let Some(menu) = this.menu.as_ref() else {
        return Vec::new();
    };
    let weak = cx.entity().downgrade();
    let children: Vec<AnyElement> = menu
        .rows
        .iter()
        .enumerate()
        .map(|(index, row)| match row {
            MenuRow::Separator => render_separator(cx).into_any_element(),
            MenuRow::Item(spec) => {
                let mut item = MenuItem::new(spec.label).icon(icon(command_icon(spec.command)));
                if let Some(shortcut) = spec.shortcut {
                    item = item.shortcut(shortcut);
                }
                if spec.danger {
                    item = item.danger();
                }
                let command = spec.command;
                let click_weak = weak.clone();
                let hover_weak = weak.clone();
                render_menu_item(
                    "explorer-menu",
                    &item,
                    index,
                    menu.selected == Some(index),
                    move |_event, window, cx| {
                        if let Err(error) = click_weak.update(cx, |this, cx| {
                            this.run_menu_command(command, window, cx);
                        }) {
                            log::debug!("the menu outlived its explorer: {error}");
                        }
                    },
                    move |cx| {
                        if let Err(error) = hover_weak.update(cx, |this, cx| {
                            if let Some(menu) = this.menu.as_mut()
                                && menu.selected != Some(index)
                            {
                                menu.selected = Some(index);
                                cx.notify();
                            }
                        }) {
                            log::debug!("the menu outlived its explorer: {error}");
                        }
                    },
                    cx,
                )
                .into_any_element()
            }
        })
        .collect();

    let dismiss = weak.clone();
    let panel = render_menu_container(children, cx).min_w(px(250.0));
    vec![
        // Under the menu: any click elsewhere closes it.
        deferred(div().absolute().size_full().child(render_menu_overlay(
            "explorer-menu-overlay",
            move |_, cx| {
                if let Err(error) = dismiss.update(cx, |this, cx| {
                    this.menu = None;
                    cx.notify();
                }) {
                    log::debug!("the menu outlived its explorer: {error}");
                }
            },
        )))
        .with_priority(1)
        .into_any_element(),
        deferred(
            anchored()
                .position(menu.position)
                .anchor(Anchor::TopLeft)
                .snap_to_window()
                .child(
                    div()
                        .id("explorer-menu")
                        .occlude()
                        .key_context(MENU_CONTEXT)
                        .track_focus(&this.menu_focus)
                        .child(panel),
                ),
        )
        .with_priority(2)
        .into_any_element(),
    ]
}

fn render_dialog(this: &Explorer, cx: &mut Context<Explorer>) -> Vec<AnyElement> {
    let Some(dialog) = this.dialog.as_ref() else {
        return Vec::new();
    };
    let colors = bolt::colors(cx);
    let weak = cx.entity().downgrade();
    let text = &dialog.text;

    let body = div()
        .flex()
        .flex_col()
        .gap(Spacing::MD)
        .child(div().text_color(colors.foreground).child(text.body.clone()))
        .children((!text.names.is_empty()).then(|| {
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .p(Spacing::MD)
                .bg(colors.background)
                .border_1()
                .border_color(colors.line)
                .children(text.names.iter().map(|name| {
                    div()
                        .truncate()
                        .font_family(bolt::fonts::editor_family(cx))
                        .text_size(px(12.0))
                        .text_color(colors.muted)
                        .child(name.clone())
                }))
        }));

    let cancel_weak = weak.clone();
    let confirm_weak = weak.clone();
    let close_weak = weak;
    let footer = div()
        .flex()
        .items_center()
        .gap(Spacing::MD)
        .child(
            Button::new("explorer-dialog-cancel", "Cancel")
                .secondary()
                .focus_handle(&this.dialog_cancel_focus)
                .on_click(move |_event, window, cx| {
                    if let Err(error) = cancel_weak.update(cx, |this, cx| {
                        this.cancel_dialog(window, cx);
                    }) {
                        log::debug!("the dialog outlived its explorer: {error}");
                    }
                }),
        )
        .child(
            Button::new("explorer-dialog-confirm", text.confirm)
                .danger()
                .on_click(move |_event, window, cx| {
                    if let Err(error) = confirm_weak.update(cx, |this, cx| {
                        this.confirm_dialog(window, cx);
                    }) {
                        log::debug!("the dialog outlived its explorer: {error}");
                    }
                }),
        );

    vec![
        Modal::new(text.title.clone())
            .id("explorer-dialog")
            .danger()
            .icon(icon(gpui_kit_assets::IconName::TriangleAlert))
            .focus_handle(&this.dialog_focus)
            .on_close(move |window, cx| {
                if let Err(error) = close_weak.update(cx, |this, cx| {
                    this.cancel_dialog(window, cx);
                }) {
                    log::debug!("the dialog outlived its explorer: {error}");
                }
            })
            .body(body)
            .footer(footer)
            .into_any_element(),
    ]
}

/// The inline rename editor, in place of the name: Bolt's input with the
/// refusal reason under it. Escape cancels (the input swallows its own
/// Escape, so the frame captures it); a press inside never reaches the
/// entry behind it, which would move the focus and commit the rename.
pub(crate) fn renamer(
    view: &RenameView,
    weak: &WeakEntity<Explorer>,
    caption: bool,
    cx: &App,
) -> AnyElement {
    let weak = weak.clone();
    let danger = bolt::colors(cx).danger;
    div()
        .id("explorer-renamer-frame")
        .w_full()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .capture_action({
            let weak = weak.clone();
            // A single-line input lets Enter through to its ancestors, and
            // the entries area binds Enter to "open the selection": commit
            // here and swallow the key, or the old entry would be opened.
            move |_: &InputEnter, window: &mut Window, cx: &mut App| {
                if let Err(error) =
                    weak.update(cx, |this, cx| this.commit_rename(false, window, cx))
                {
                    log::debug!("the editor outlived its explorer: {error}");
                }
                cx.stop_propagation();
            }
        })
        .capture_action(move |_: &InputEscape, window: &mut Window, cx: &mut App| {
            if let Err(error) = weak.update(cx, |this, cx| this.cancel_rename(window, cx)) {
                log::debug!("the editor outlived its explorer: {error}");
            }
            cx.stop_propagation();
        })
        // A list row has no room for the caption under the field: the
        // refusal shows as a danger frame there and in the status bar.
        .when(!caption && view.error.is_some(), |frame| {
            frame.border_1().border_color(danger)
        })
        .child(
            Input::new(&view.input)
                .id("explorer-renamer")
                .aria_label("New name")
                .w_full()
                .small()
                .error(view.error.clone().filter(|_| caption)),
        )
        .into_any_element()
}

/// The point menus anchor to when the toolbar button opens one.
pub(crate) fn below(position: gpui::Point<gpui::Pixels>) -> gpui::Point<gpui::Pixels> {
    point(position.x - px(180.0), position.y + px(14.0))
}
