//! Selection commands beyond clicking: Select all, Invert selection and
//! "Select items matching…" (a glob over the visible names). They act on the
//! active pane's tab and only ever touch the selection.

use bolt::controls::{InputEvent, InputState};
use gpui::{AppContext as _, Context, Entity, Window};

use super::ops::{DialogAction, DialogState, Severity};
use super::view::Explorer;

/// The open "Select items matching…" dialog: its one text field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SelectDialog {
    pub(crate) input: Entity<InputState>,
}

impl DialogState {
    /// The "Select items matching…" dialog.
    pub(crate) fn select_pattern(dialog: SelectDialog) -> DialogState {
        DialogState::new(
            super::ops::DialogKind::SelectPattern,
            &[],
            DialogAction::SelectPattern(dialog),
        )
    }
}

impl Explorer {
    /// Scrolls the cursor row into view and redraws.
    fn reveal_cursor(&mut self, cx: &mut Context<Self>) {
        if let Some(position) = self.tab().state.cursor_position() {
            self.scroll_selected_row(position, cx);
        }
        cx.notify();
    }

    /// Ctrl+A: every visible entry.
    pub(crate) fn select_all(&mut self, cx: &mut Context<Self>) {
        self.tab_mut().state.select_all_visible();
        cx.notify();
    }

    /// Ctrl+Shift+I: selected and unselected swap over the visible entries.
    pub(crate) fn invert_selection(&mut self, cx: &mut Context<Self>) {
        self.tab_mut().state.invert_selection_visible();
        self.reveal_cursor(cx);
    }

    /// Ctrl+S: asks for a pattern, prefilled with the last one.
    pub(crate) fn select_matching(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let seed = self.select_pattern.clone();
        let input = cx.new(|cx| {
            let mut input = InputState::new(window, cx);
            input.set_placeholder("Name pattern, e.g. *.jpg", window, cx);
            input.set_value(seed, window, cx);
            input
        });
        cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, window, cx| {
                // Only the live dialog's own field may confirm.
                let live = this.dialogs.current().is_some_and(|dialog| {
                    matches!(&dialog.action, DialogAction::SelectPattern(state) if state.input == *input)
                });
                if live && matches!(event, InputEvent::PressEnter { .. }) {
                    this.confirm_dialog(window, cx);
                }
            },
        )
        .detach();
        let field = input.clone();
        self.offer_dialog(
            DialogState::select_pattern(SelectDialog { input }),
            window,
            cx,
        );
        cx.defer_in(window, move |_this, window, cx| {
            field.update(cx, |input, cx| {
                input.focus(window, cx);
                input.select_all(window, cx);
            });
        });
    }

    /// Confirming the dialog: selects the matches and closes it; with no
    /// match the selection stays and a toast says so.
    pub(crate) fn confirm_select_pattern(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(DialogAction::SelectPattern(dialog)) =
            self.dialogs.current().map(|dialog| dialog.action.clone())
        else {
            return;
        };
        let pattern = dialog.input.read(cx).value().to_string();
        self.dialogs.close();
        self.dialog_closed(window, cx);
        if pattern.trim().is_empty() {
            return;
        }
        self.select_pattern.clone_from(&pattern);
        if self.tab_mut().state.select_matching(&pattern) == 0 {
            self.say(
                Severity::Info,
                format!("No items match {pattern}"),
                window,
                cx,
            );
        }
        self.reveal_cursor(cx);
    }
}
