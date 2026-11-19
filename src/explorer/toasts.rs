//! How long toasts stay, how many show at once, and where the stack sits.
//! Plain data and arithmetic: [`super::actions`] pushes the toasts and
//! [`super::view`] draws the stack, both through these rules.
//!
//! The stack sits in the bottom-right corner of the window, above the status
//! bar, so it never covers the subbar buttons or the middle of the grid. A
//! toast closes by itself after [`toast_timeout`]; Bolt's host pauses that
//! timer while the pointer rests on the toast. At most [`MAX_VISIBLE`] show:
//! a new one beyond it closes the oldest (an error last, so it is not lost
//! before it is read). Bolt's host draws the stack top-down, so the stack's
//! top is placed from its measured height ([`stack_top`]) and the stack
//! grows upward from the corner.

use std::time::Duration;

use super::ops::Severity;

/// How long an info or success toast stays.
pub(crate) const INFO_TOAST: Duration = Duration::from_secs(3);
/// How long a toast with an Undo button stays.
pub(crate) const UNDO_TOAST: Duration = Duration::from_secs(6);
/// How long an error (or a warning) stays; the close button ends it earlier.
pub(crate) const ERROR_TOAST: Duration = Duration::from_secs(8);
/// The most toasts shown at once.
pub(crate) const MAX_VISIBLE: usize = 3;

/// The width of one toast card (Bolt's `Feedback::TOAST_WIDTH`).
pub(crate) const CARD_WIDTH: f32 = 440.0;
/// The inset Bolt's host keeps from the top and right of the region it is
/// mounted in; the stack is shifted back by it to sit where it is wanted.
pub(crate) const HOST_INSET: f32 = 16.0;
/// The gap between the stack and the window's right edge and the status bar.
pub(crate) const MARGIN: f32 = 12.0;
/// The status bar's height.
pub(crate) const STATUS_BAR_HEIGHT: f32 = 38.0;

/// How long a toast of this kind stays before it closes by itself.
pub(crate) fn toast_timeout(severity: Severity, has_undo: bool) -> Duration {
    match severity {
        Severity::Error | Severity::Warning => ERROR_TOAST,
        Severity::Success | Severity::Info if has_undo => UNDO_TOAST,
        Severity::Success | Severity::Info => INFO_TOAST,
    }
}

/// The toasts to close so that one more fits: `shown` are the open ones,
/// oldest first. Non-errors go first, oldest first; an error closes only
/// when nothing else is left to.
pub(crate) fn make_room(shown: &[(u64, Severity)]) -> Vec<u64> {
    let excess = (shown.len() + 1).saturating_sub(MAX_VISIBLE);
    let quiet = shown
        .iter()
        .filter(|(_, severity)| *severity != Severity::Error);
    let errors = shown
        .iter()
        .filter(|(_, severity)| *severity == Severity::Error);
    quiet
        .chain(errors)
        .take(excess)
        .map(|(id, _)| *id)
        .collect()
}

/// The `top` of the zero-height wrapper that holds Bolt's host, so that the
/// stack's bottom edge, `stack_height` tall, rests `MARGIN` above the status
/// bar in a window `window_height` tall. The host sits [`HOST_INSET`] under
/// its wrapper's top.
pub(crate) fn stack_top(window_height: f32, stack_height: f32) -> f32 {
    window_height - STATUS_BAR_HEIGHT - MARGIN - stack_height - HOST_INSET
}

#[cfg(test)]
mod tests {
    use super::*;
    use Severity::{Error, Info, Success, Warning};

    #[test]
    fn timeouts_follow_the_kind_and_the_undo_button() {
        assert_eq!(toast_timeout(Info, false), Duration::from_secs(3));
        assert_eq!(toast_timeout(Success, false), Duration::from_secs(3));
        assert_eq!(toast_timeout(Success, true), Duration::from_secs(6));
        assert_eq!(toast_timeout(Info, true), Duration::from_secs(6));
        assert_eq!(toast_timeout(Error, false), Duration::from_secs(8));
        assert_eq!(toast_timeout(Warning, false), Duration::from_secs(8));
        assert_eq!(
            toast_timeout(Error, true),
            Duration::from_secs(8),
            "an error stays as long as an error does"
        );
    }

    #[test]
    fn at_most_three_show_and_the_oldest_quiet_one_gives_way() {
        assert!(make_room(&[]).is_empty());
        assert!(make_room(&[(1, Info), (2, Info)]).is_empty());
        assert_eq!(make_room(&[(1, Info), (2, Success), (3, Info)]), [1]);
        assert_eq!(
            make_room(&[(1, Error), (2, Info), (3, Success)]),
            [2],
            "an error is kept while a quiet toast can go"
        );
        assert_eq!(make_room(&[(1, Error), (2, Error), (3, Error)]), [1]);
        // More open than the limit (a burst): enough close to fit one more.
        assert_eq!(
            make_room(&[(1, Info), (2, Info), (3, Info), (4, Info)]),
            [1, 2]
        );
    }

    #[test]
    fn the_stack_rests_above_the_status_bar_whatever_its_height() {
        let window = 900.0;
        for stack in [0.0, 60.0, 190.0] {
            let top = stack_top(window, stack);
            // The host's top edge, its height and the margin end at the bar.
            assert_eq!(
                top + HOST_INSET + stack + MARGIN + STATUS_BAR_HEIGHT,
                window
            );
        }
        assert!(
            stack_top(900.0, 190.0) < stack_top(900.0, 60.0),
            "grows upward"
        );
    }
}
