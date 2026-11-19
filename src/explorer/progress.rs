//! Operation progress: the worker's [`Sink`], the registry of running
//! operations, and the reusable non-modal progress card.
//!
//! Any operation that runs off the UI thread registers here, reports through
//! its [`Sink`] (done/total, the current item name) and polls the sink's
//! cancel flag between items. The card (bottom-right, stacking) appears only
//! once an operation has run for [`SHOW_AFTER`]; faster operations give their
//! feedback through the summary toast alone, so nothing flashes. The card
//! shows a title, a bar, the progress text with speed or ETA when they mean
//! something, the current item and a Cancel button. Paste reuses it
//! by registering a [`Unit::Bytes`] operation with the core transfer's
//! progress callback and cancel flag.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use bolt::controls::Button;
use bolt::tokens::Spacing;
use fileflux::core::places::format_bytes;
use gpui::{
    AnyElement, App, InteractiveElement as _, IntoElement as _, ParentElement as _, Styled as _,
    Task, WeakEntity, div, prelude::FluentBuilder as _, px, relative,
};

use super::view::Explorer;

/// How long an operation runs before its card appears.
pub(crate) const SHOW_AFTER: Duration = Duration::from_millis(400);
/// ETA and speed need this much elapsed time to be more than noise.
const ESTIMATE_AFTER: Duration = Duration::from_secs(2);
/// Speed needs at least this much elapsed time.
const SPEED_AFTER: Duration = Duration::from_secs(1);
/// Card width, in pixels.
const CARD_WIDTH: f32 = 340.0;

/// What the progress numbers count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Unit {
    /// Whole items (trash, delete): "3 of 10 items".
    #[default]
    Items,
    /// Bytes (copy, move): "1.2 MB of 4 MB", with speed.
    Bytes,
}

/// The numbers a worker publishes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProgressState {
    pub(crate) done: u64,
    pub(crate) total: u64,
    /// The item being worked on, for display.
    pub(crate) current: Option<String>,
    pub(crate) unit: Unit,
}

/// The worker's side of an operation: shared progress and the cancel flag.
/// Clones share both, so the UI keeps one clone to read and to cancel.
#[derive(Clone, Debug)]
pub(crate) struct Sink {
    state: Arc<Mutex<ProgressState>>,
    cancel: Arc<AtomicBool>,
}

impl Sink {
    pub(crate) fn new(unit: Unit) -> Sink {
        Sink {
            state: Arc::new(Mutex::new(ProgressState {
                unit,
                ..ProgressState::default()
            })),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    fn with_state<T>(&self, update: impl FnOnce(&mut ProgressState) -> T) -> T {
        // A panicked worker leaves plain numbers behind: still readable.
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        update(&mut state)
    }

    /// Sets what "done" is counted against.
    pub(crate) fn set_total(&self, total: u64) {
        self.with_state(|state| state.total = total);
    }

    /// Publishes the progress made and the item now being worked on.
    pub(crate) fn advance(&self, done: u64, current: &str) {
        self.with_state(|state| {
            state.done = done;
            state.current = Some(current.to_owned());
        });
    }

    pub(crate) fn snapshot(&self) -> ProgressState {
        self.with_state(|state| state.clone())
    }

    /// Asks the worker to stop before its next item.
    pub(crate) fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub(crate) fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// The flag itself, for core operations that poll it between chunks.
    pub(crate) fn cancel_flag(&self) -> &AtomicBool {
        &self.cancel
    }
}

/// Whether an operation that has run for `elapsed` shows its card.
pub(crate) fn card_visible(elapsed: Duration) -> bool {
    elapsed >= SHOW_AFTER
}

/// The bar's fill, `None` while the total is unknown (indeterminate).
pub(crate) fn fraction(state: &ProgressState) -> Option<f32> {
    if state.total == 0 {
        return None;
    }
    Some((state.done as f64 / state.total as f64).min(1.0) as f32)
}

/// "3 of 10 items" or "1.5 KB of 3 MB".
pub(crate) fn describe(state: &ProgressState) -> String {
    match state.unit {
        Unit::Items => format!(
            "{} of {} item{}",
            state.done,
            state.total,
            if state.total == 1 { "" } else { "s" }
        ),
        Unit::Bytes => format!(
            "{} of {}",
            format_bytes(state.done),
            format_bytes(state.total)
        ),
    }
}

/// "about 30 s left", only once the estimate is more than noise.
pub(crate) fn eta_label(state: &ProgressState, elapsed: Duration) -> Option<String> {
    if elapsed < ESTIMATE_AFTER || state.done == 0 || state.done >= state.total {
        return None;
    }
    let remaining = elapsed.as_secs_f64() * (state.total - state.done) as f64 / state.done as f64;
    Some(if remaining < 60.0 {
        format!("about {} s left", remaining.round() as u64)
    } else {
        format!(
            "about {} min left",
            ((remaining + 30.0) / 60.0).floor() as u64
        )
    })
}

/// "2 MB/s" for byte operations once a second has passed.
pub(crate) fn speed_label(state: &ProgressState, elapsed: Duration) -> Option<String> {
    if state.unit != Unit::Bytes || elapsed < SPEED_AFTER || state.done == 0 {
        return None;
    }
    let rate = state.done as f64 / elapsed.as_secs_f64();
    Some(format!("{}/s", format_bytes(rate as u64)))
}

/// One running operation.
pub(crate) struct ActiveOp {
    pub(crate) id: u64,
    pub(crate) title: String,
    pub(crate) started: Instant,
    pub(crate) sink: Sink,
    pub(crate) cancellable: bool,
    /// Repaints the card while the operation runs; dropping it stops it.
    ticker: Option<Task<()>>,
}

impl ActiveOp {
    pub(crate) fn elapsed(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.started)
    }
}

/// Every operation that is running right now.
#[derive(Default)]
pub(crate) struct Registry {
    ops: Vec<ActiveOp>,
    next_id: u64,
}

impl Registry {
    /// Registers an operation and returns its id and the sink its worker
    /// reports through.
    pub(crate) fn start(
        &mut self,
        title: impl Into<String>,
        unit: Unit,
        cancellable: bool,
    ) -> (u64, Sink) {
        self.next_id += 1;
        let sink = Sink::new(unit);
        self.ops.push(ActiveOp {
            id: self.next_id,
            title: title.into(),
            started: Instant::now(),
            sink: sink.clone(),
            cancellable,
            ticker: None,
        });
        (self.next_id, sink)
    }

    /// Keeps the repaint task alive for as long as the operation runs.
    pub(crate) fn attach_ticker(&mut self, id: u64, ticker: Task<()>) {
        if let Some(op) = self.ops.iter_mut().find(|op| op.id == id) {
            op.ticker = Some(ticker);
        }
    }

    /// The title of a running operation, for the dialogs that name it.
    pub(crate) fn title(&self, id: u64) -> Option<&str> {
        self.ops
            .iter()
            .find(|op| op.id == id)
            .map(|op| op.title.as_str())
    }

    pub(crate) fn is_running(&self, id: u64) -> bool {
        self.ops.iter().any(|op| op.id == id)
    }

    /// Forgets a finished operation.
    pub(crate) fn finish(&mut self, id: u64) {
        self.ops.retain(|op| op.id != id);
    }

    /// Asks one operation to stop; it ends at its next item.
    pub(crate) fn cancel(&self, id: u64) {
        if let Some(op) = self.ops.iter().find(|op| op.id == id) {
            op.sink.request_cancel();
        }
    }

    /// The operations whose card is showing, oldest first.
    pub(crate) fn visible(&self, now: Instant) -> impl Iterator<Item = &ActiveOp> {
        self.ops
            .iter()
            .filter(move |op| card_visible(op.elapsed(now)))
    }

    /// The compact status-bar text while any card is showing.
    pub(crate) fn status_text(&self, now: Instant) -> Option<String> {
        let mut visible = self.visible(now);
        let first = visible.next()?;
        let more = visible.count();
        Some(if more == 0 {
            format!(
                "{} \u{b7} {}",
                first.title,
                describe(&first.sink.snapshot())
            )
        } else {
            format!("{} operations running", more + 1)
        })
    }
}

/// The stack of cards, bottom-right above the status bar; `None` while no
/// card is due.
pub(crate) fn render_cards(
    registry: &Registry,
    now: Instant,
    weak: &WeakEntity<Explorer>,
    cx: &App,
) -> Option<AnyElement> {
    let cards: Vec<AnyElement> = registry
        .visible(now)
        .map(|op| render_card(op, now, weak, cx))
        .collect();
    if cards.is_empty() {
        return None;
    }
    Some(
        div()
            .id("explorer-progress-cards")
            .absolute()
            .right(px(24.0))
            .bottom(px(52.0))
            .flex()
            .flex_col()
            .gap(Spacing::SM)
            .children(cards)
            .into_any_element(),
    )
}

fn render_card(op: &ActiveOp, now: Instant, weak: &WeakEntity<Explorer>, cx: &App) -> AnyElement {
    let colors = bolt::colors(cx);
    let tint = bolt::tokens::ChromeColors::tint(gpui_component::ActiveTheme::theme(cx));
    let state = op.sink.snapshot();
    let elapsed = op.elapsed(now);
    let cancelling = op.sink.cancelled();
    let bar = fraction(&state);
    let mut meta = vec![describe(&state)];
    meta.extend(speed_label(&state, elapsed));
    meta.extend(eta_label(&state, elapsed));
    let id = op.id;
    let weak = weak.clone();

    div()
        .id(("explorer-progress-card", id))
        .relative()
        .w(px(CARD_WIDTH))
        .flex()
        .flex_col()
        .gap(px(8.0))
        .p(px(12.0))
        .bg(colors.raised)
        .border_1()
        .border_color(colors.line_2)
        .text_size(px(12.0))
        // The 2 px tint edge of the artboard's toast.
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .w(px(2.0))
                .bg(tint),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(12.0))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(colors.strong)
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(if cancelling {
                            format!("Cancelling\u{2026} {}", op.title)
                        } else {
                            op.title.clone()
                        }),
                )
                .when(op.cancellable, |row| {
                    row.child(
                        Button::new(("explorer-progress-cancel", id), "Cancel")
                            .ghost()
                            .disabled(cancelling)
                            .on_click(move |_event, window, cx| {
                                if let Err(error) = weak.update(cx, |this, cx| {
                                    this.cancel_operation(id, window, cx);
                                }) {
                                    log::debug!("the card outlived its explorer: {error}");
                                }
                            }),
                    )
                }),
        )
        .child(
            div()
                .h(px(4.0))
                .w_full()
                .bg(colors.background)
                .child(div().h_full().bg(tint).w(relative(bar.unwrap_or(0.3)))),
        )
        .child(
            div()
                .flex()
                .gap(px(8.0))
                .text_color(colors.muted)
                .child(meta.join(" \u{b7} ")),
        )
        .children(state.current.map(|current| {
            div()
                .truncate()
                .text_color(colors.foreground)
                .child(current)
        }))
        .into_any_element()
}

#[cfg(test)]
#[path = "progress_tests.rs"]
mod tests;
