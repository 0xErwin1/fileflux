//! What floats above the explorer: the context menu, the confirmation
//! dialog, and the inline rename editor the entry views embed.
//!
//! All three reuse Bolt: the menu is `bolt::composites` menu rows in
//! Bolt's menu frame under its dismiss overlay, the dialog is
//! `bolt::modals::Modal` with `Button`s, and the editor is Bolt's `Input`.

use bolt::composites::{
    LevelMetrics, MenuTreeEvent, MenuTreeLayout, level_height, render_menu_overlay,
    render_menu_tree,
};
use bolt::controls::{Button, Checkbox, Input, InputEnter, InputEscape};
use bolt::modals::Modal;
use bolt::tokens::{Borders, MenuMetrics, Spacing};
use gpui::{
    AnyElement, App, Context, InteractiveElement as _, IntoElement as _, MouseButton,
    ParentElement as _, Pixels, Point, Size, StatefulInteractiveElement as _, Styled as _,
    WeakEntity, Window, deferred, div, point, prelude::FluentBuilder as _, px, size,
};

use super::actions::RenameView;
use super::archive::ArchiveAction;
use super::icon;
use super::ops::{DialogAction, DialogChoice, DialogKind};
use super::view::{Explorer, MENU_CONTEXT};
use fileflux::core::archive::Format;

/// The menu and the dialog, when open. Also hands the keyboard back to the
/// entries when the menu or dialog was closed by something that had no
/// window to do it with (a click on the scrim).
pub(crate) fn render(
    this: &mut Explorer,
    window: &mut Window,
    cx: &mut Context<Explorer>,
) -> Vec<AnyElement> {
    if this.menu.is_none() && this.menu_focus.is_focused(window) {
        this.entries_focus().focus(window, cx);
    }
    if !this.dialogs.is_open() && this.dialog_focus.contains_focused(window, cx) {
        this.entries_focus().focus(window, cx);
    }
    let mut layers = Vec::new();
    layers.extend(render_menu(this, window, cx));
    layers.extend(render_dialog(this, cx));
    layers
}

/// Width of the root panel: wide enough for "Delete permanently…" and its
/// shortcut; a longer target title truncates.
const MENU_WIDTH: f32 = 260.0;

/// The row heights of the current theme, the way the cascade resolves them,
/// to place the root panel before it draws.
fn level_metrics(cx: &App) -> LevelMetrics {
    LevelMetrics {
        row_height: bolt::fonts::ui_px(cx, MenuMetrics::ROW_HEIGHT),
        detail_row_height: bolt::fonts::ui_px(cx, MenuMetrics::DETAIL_ROW_HEIGHT),
        header_height: bolt::fonts::ui_px(cx, MenuMetrics::HEADER_ICON)
            + MenuMetrics::HEADER_PADDING_TOP
            + MenuMetrics::HEADER_PADDING_BOTTOM,
        separator_height: Borders::THIN + MenuMetrics::SEPARATOR_MARGIN_Y * 2.0,
        padding_y: MenuMetrics::PADDING_Y,
    }
}

/// Where the root panel opens: at `wanted`, moved left and up as far as the
/// window's edges require. Only the flyouts flip and clamp on their own.
fn root_origin(
    wanted: Point<Pixels>,
    panel: Size<Pixels>,
    viewport: Size<Pixels>,
) -> Point<Pixels> {
    let margin = MenuMetrics::FLYOUT_MARGIN;
    let x = wanted
        .x
        .min(viewport.width - panel.width - margin)
        .max(margin);
    let y = wanted
        .y
        .min(viewport.height - panel.height - margin)
        .max(margin);
    point(x, y)
}

fn render_menu(this: &Explorer, window: &Window, cx: &mut Context<Explorer>) -> Vec<AnyElement> {
    let Some(menu) = this.menu.as_ref() else {
        return Vec::new();
    };
    let weak = cx.entity().downgrade();
    let nodes = menu.nodes();
    let viewport = window.bounds().size;
    let width = px(MENU_WIDTH);
    let height = level_height(&nodes, &level_metrics(cx));
    let layout = MenuTreeLayout {
        origin: root_origin(menu.position, size(width, height), viewport),
        width,
        viewport,
        keyboard_focus: menu.keyboard,
    };
    let dismiss = weak.clone();
    let handle = window.window_handle();
    let tree = render_menu_tree(
        "explorer-menu",
        &nodes,
        &menu.nav,
        layout,
        move |event, cx| {
            let result = match event {
                // Running a row needs the window (it may open a dialog or move
                // the keyboard focus), and this callback runs while the window
                // is busy dispatching the click: run the row once it is free.
                MenuTreeEvent::Activated { path, .. } => {
                    let weak = weak.clone();
                    cx.defer(move |cx| {
                        let outcome = handle.update(cx, |_, window, cx| {
                            weak.update(cx, |this, cx| this.activate_menu_path(&path, window, cx))
                        });
                        if let Err(error) = outcome.and_then(|inner| inner) {
                            log::debug!("the menu outlived its explorer: {error}");
                        }
                    });
                    Ok(())
                }
                MenuTreeEvent::Opened { level, index } => weak
                    .update(cx, |this, cx| this.menu_open_flyout(level, index, cx))
                    .map_err(|error| error.to_string()),
                MenuTreeEvent::Hovered { level, index } => weak
                    .update(cx, |this, cx| this.menu_row_hovered(level, index, cx))
                    .map_err(|error| error.to_string()),
            };
            if let Err(error) = result {
                log::debug!("the menu outlived its explorer: {error}");
            }
        },
        cx,
    );
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
            div()
                .id("explorer-menu")
                .absolute()
                .size_full()
                .key_context(MENU_CONTEXT)
                .track_focus(&this.menu_focus)
                .child(tree),
        )
        .with_priority(2)
        .into_any_element(),
    ]
}

fn render_dialog(this: &Explorer, cx: &mut Context<Explorer>) -> Vec<AnyElement> {
    let Some(dialog) = this.dialogs.current() else {
        return Vec::new();
    };
    let colors = bolt::colors(cx);
    let weak = cx.entity().downgrade();
    let text = &dialog.text;
    // The danger button ignores the pointer (and is drawn disabled) while
    // the dialog is fresh; see `ops::ARM_DELAY`.
    let armed = this.dialogs.armed_at(std::time::Instant::now());
    let apply_all = match dialog.action {
        DialogAction::Conflict { apply_all, .. } => Some(apply_all),
        _ => None,
    };

    let open_with = match &dialog.action {
        DialogAction::OpenWith(state) => Some(state),
        _ => None,
    };
    let archive = match &dialog.action {
        DialogAction::Archive(state) => Some(state),
        _ => None,
    };
    let pattern = match &dialog.action {
        DialogAction::SelectPattern(state) => Some(state),
        _ => None,
    };
    let connect = match &dialog.action {
        DialogAction::Connect(state) => Some(state),
        _ => None,
    };
    let prompt = match &dialog.action {
        DialogAction::MountPrompt(state) => Some(state),
        _ => None,
    };
    let secret_prompt = prompt.is_some_and(|state| state.prompt.is_secret());
    // A question of gio is answered by its own buttons; Continue has nothing
    // to send.
    let question = matches!(prompt, Some(state) if state.input.is_none());
    // Waiting for a device to be free destroys nothing: not a danger button.
    let calm = open_with.is_some()
        || archive.is_some()
        || pattern.is_some()
        || connect.is_some()
        || prompt.is_some()
        || matches!(dialog.kind, DialogKind::DeviceBusy { .. });
    let body = div()
        .flex()
        .flex_col()
        .gap(Spacing::MD)
        .child(div().text_color(colors.foreground).child(text.body.clone()))
        // "Open with…": one clickable row per application.
        // The list of every application is narrowed by a filter box.
        .children(
            open_with
                .and_then(|state| state.filter_input.as_ref())
                .map(|input| {
                    Input::new(input)
                        .id("explorer-open-with-filter")
                        .aria_label("Filter applications")
                        .w_full()
                        .small()
                }),
        )
        // "Compress…" and "Extract to…": one text field, and for Compress
        // the format choice.
        .children(pattern.map(|state| {
            Input::new(&state.input)
                .id("explorer-select-pattern-input")
                .aria_label("Name pattern")
                .w_full()
                .small()
        }))
        // "Connect to Server…": the address, the reason it was refused and
        // the servers of this session as quick picks.
        .children(connect.map(|state| {
            Input::new(&state.input)
                .id("explorer-connect-input")
                .aria_label("Server address")
                .w_full()
                .small()
                .error(state.error.clone())
        }))
        .children(
            connect
                .filter(|state| !state.recent.is_empty())
                .map(|state| {
                    div()
                        .flex()
                        .flex_col()
                        .gap(Spacing::SM)
                        .child(div().text_color(colors.muted).child("Recent servers"))
                        .children(state.recent.iter().enumerate().map(|(index, address)| {
                            let pick_weak = weak.clone();
                            Button::new(("explorer-connect-recent", index), address.clone())
                                .secondary()
                                .on_click(move |_event, window, cx| {
                                    if let Err(error) = pick_weak.update(cx, |this, cx| {
                                        this.pick_recent_server(index, window, cx);
                                    }) {
                                        log::debug!("the dialog outlived its explorer: {error}");
                                    }
                                })
                        }))
                }),
        )
        // A prompt of gio: the field (masked for a password) or, for a
        // question, one button per choice.
        .children(prompt.and_then(|state| state.input.as_ref()).map(|input| {
            Input::new(input)
                .id("explorer-mount-input")
                .aria_label("Answer")
                .secret(secret_prompt)
                .w_full()
                .small()
        }))
        .children(
            prompt
                .and_then(|state| match &state.prompt {
                    fileflux::core::network::Prompt::Question { choices, .. } => Some(choices),
                    fileflux::core::network::Prompt::Text { .. } => None,
                })
                .map(|choices| {
                    div().flex().flex_col().gap(Spacing::SM).children(
                        choices.iter().enumerate().map(|(index, (number, choice))| {
                            let number = *number;
                            let pick_weak = weak.clone();
                            Button::new(("explorer-mount-choice", index), choice.clone())
                                .secondary()
                                .on_click(move |_event, window, cx| {
                                    if let Err(error) = pick_weak.update(cx, |this, cx| {
                                        this.answer_mount_choice(number, window, cx);
                                    }) {
                                        log::debug!("the dialog outlived its explorer: {error}");
                                    }
                                })
                        }),
                    )
                }),
        )
        .children(archive.map(|state| {
            Input::new(&state.input)
                .id("explorer-archive-input")
                .aria_label(match state.action {
                    ArchiveAction::Compress { .. } => "Archive name",
                    ArchiveAction::ExtractTo { .. } => "Destination folder",
                })
                .w_full()
                .small()
        }))
        .children(
            archive
                .and_then(|state| match &state.action {
                    ArchiveAction::Compress { format, .. } => Some(*format),
                    ArchiveAction::ExtractTo { .. } => None,
                })
                .map(|chosen| {
                    div().flex().items_center().gap(Spacing::SM).children(
                        Format::ALL.into_iter().enumerate().map(|(index, format)| {
                            let pick_weak = weak.clone();
                            let button =
                                Button::new(("explorer-archive-format", index), format.suffix());
                            let button = if format == chosen {
                                button.primary()
                            } else {
                                button.secondary()
                            };
                            button.on_click(move |_event, _window, cx| {
                                if let Err(error) = pick_weak.update(cx, |this, cx| {
                                    this.set_archive_format(format, cx);
                                }) {
                                    log::debug!("the dialog outlived its explorer: {error}");
                                }
                            })
                        }),
                    )
                }),
        )
        .children(open_with.map(|state| {
            let selected = state.selected;
            div()
                .id("explorer-open-with-list")
                .flex()
                .flex_col()
                .max_h(px(280.0))
                .overflow_y_scroll()
                .bg(colors.background)
                .border_1()
                .border_color(colors.line)
                .children(text.names.iter().enumerate().map(|(index, name)| {
                    let click_weak = weak.clone();
                    div()
                        .id(("explorer-open-with-app", index))
                        .px(Spacing::MD)
                        .py(px(6.0))
                        .cursor_pointer()
                        .truncate()
                        .text_color(colors.foreground)
                        .when(index == selected, |row| row.bg(colors.line))
                        .hover(|row| row.bg(colors.line))
                        .child(name.clone())
                        // A click selects the row, as it does in every list;
                        // a double click opens the file with it, like Enter.
                        .on_click(move |event, window, cx| {
                            let confirm = open_with_click_opens(event.click_count());
                            if let Err(error) = click_weak.update(cx, |this, cx| {
                                if confirm {
                                    this.choose_open_with(index, window, cx);
                                } else {
                                    this.select_open_with(index, cx);
                                }
                            }) {
                                log::debug!("the dialog outlived its explorer: {error}");
                            }
                        })
                }))
        }))
        .children((open_with.is_none() && !text.names.is_empty()).then(|| {
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
        }))
        .children(open_with.is_some().then(|| {
            let toggle_weak = weak.clone();
            Checkbox::new("explorer-open-with-remember")
                .checked(dialog.open_with_remember())
                .label(dialog.remember_label().unwrap_or_default())
                .on_click(move |checked, _window, cx| {
                    let checked = *checked;
                    if let Err(error) = toggle_weak.update(cx, |this, cx| {
                        this.set_open_with_remember(checked, cx);
                    }) {
                        log::debug!("the dialog outlived its explorer: {error}");
                    }
                })
        }))
        .children(apply_all.map(|checked| {
            let toggle_weak = weak.clone();
            Checkbox::new("explorer-conflict-apply-all")
                .checked(checked)
                .label("Apply to all remaining conflicts")
                .on_click(move |checked, _window, cx| {
                    let checked = *checked;
                    if let Err(error) = toggle_weak.update(cx, |this, cx| {
                        this.set_apply_all(checked, cx);
                    }) {
                        log::debug!("the dialog outlived its explorer: {error}");
                    }
                })
        }));

    let close_weak = weak.clone();
    // One button per choice; `choose_dialog` is the single place a press
    // is acted on.
    let press = |choice: DialogChoice| {
        let weak = weak.clone();
        move |_event: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
            if let Err(error) = weak.update(cx, |this, cx| {
                this.choose_dialog(choice, window, cx);
            }) {
                log::debug!("the dialog outlived its explorer: {error}");
            }
        }
    };
    let conflict = apply_all.is_some();
    let footer = div()
        .flex()
        .items_center()
        .gap(Spacing::MD)
        .child(
            Button::new("explorer-dialog-cancel", "Cancel")
                .secondary()
                .focus_handle(&this.dialog_cancel_focus)
                .on_click(press(DialogChoice::Cancel)),
        )
        .when(conflict, |footer| {
            footer
                .child(
                    Button::new("explorer-dialog-keep", "Keep both")
                        .secondary()
                        .focus_handle(&this.dialog_keep_focus)
                        .on_click(press(DialogChoice::KeepBoth)),
                )
                .child(
                    Button::new("explorer-dialog-skip", "Skip")
                        .secondary()
                        .focus_handle(&this.dialog_skip_focus)
                        .on_click(press(DialogChoice::Skip)),
                )
        })
        .child(
            {
                let confirm = Button::new("explorer-dialog-confirm", text.confirm);
                if calm {
                    confirm.primary()
                } else {
                    confirm.danger()
                }
            }
            .disabled(!armed || question)
            .focus_handle(&this.dialog_confirm_focus)
            .on_click(press(DialogChoice::Confirm)),
        );

    let modal = Modal::new(text.title.clone()).id("explorer-dialog");
    let modal = if secret_prompt {
        modal.icon(icon(gpui_kit_assets::IconName::Lock))
    } else if connect.is_some() || prompt.is_some() {
        modal.icon(icon(gpui_kit_assets::IconName::Network))
    } else if pattern.is_some() {
        modal.icon(icon(gpui_kit_assets::IconName::SquareCheck))
    } else if archive.is_some() {
        modal.icon(icon(gpui_kit_assets::IconName::Archive))
    } else if open_with.is_some() {
        modal.icon(icon(gpui_kit_assets::IconName::FolderOpen))
    } else if calm {
        modal.icon(icon(gpui_kit_assets::IconName::Usb))
    } else {
        modal
            .danger()
            .icon(icon(gpui_kit_assets::IconName::TriangleAlert))
    };
    vec![
        modal
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

/// The point a subbar button's menu anchors to: just under the click.
pub(crate) fn under_button(position: gpui::Point<gpui::Pixels>) -> gpui::Point<gpui::Pixels> {
    point(position.x - px(24.0), position.y + px(16.0))
}

/// The point menus anchor to when the toolbar button opens one.
pub(crate) fn below(position: gpui::Point<gpui::Pixels>) -> gpui::Point<gpui::Pixels> {
    point(position.x - px(180.0), position.y + px(14.0))
}

/// Whether a click on a row of the "Open with…" list opens the file with it:
/// a double click does, a single click only selects the row. Enter does not
/// come through here; it confirms the selected row (`confirm_dialog`).
pub(crate) fn open_with_click_opens(click_count: usize) -> bool {
    click_count >= 2
}

#[cfg(test)]
mod tests {
    use super::open_with_click_opens;

    #[test]
    fn a_single_click_selects_and_a_double_click_opens() {
        assert!(!open_with_click_opens(1));
        assert!(open_with_click_opens(2));
        assert!(open_with_click_opens(3));
    }
}
