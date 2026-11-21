use std::time::Duration;

use fileflux::core::places::format_bytes;

use super::*;

fn state(done: u64, total: u64, unit: Unit) -> ProgressState {
    ProgressState {
        done,
        total,
        current: None,
        unit,
    }
}

/// The card only appears for an operation that outlasts ~400 ms; instant
/// operations give their feedback through the summary toast alone.
#[test]
fn the_card_shows_only_after_the_threshold() {
    assert!(!card_visible(Duration::ZERO));
    assert!(!card_visible(Duration::from_millis(399)));
    assert!(card_visible(Duration::from_millis(400)));
    assert!(card_visible(Duration::from_secs(30)));
}

#[test]
fn the_bar_fraction_is_unknown_without_a_total_and_clamped_to_one() {
    assert_eq!(fraction(&state(0, 0, Unit::Items)), None);
    assert_eq!(fraction(&state(3, 10, Unit::Items)), Some(0.3));
    assert_eq!(fraction(&state(10, 10, Unit::Items)), Some(1.0));
    assert_eq!(fraction(&state(12, 10, Unit::Items)), Some(1.0));
}

#[test]
fn the_progress_text_counts_items_or_bytes() {
    assert_eq!(describe(&state(3, 10, Unit::Items)), "3 of 10 items");
    assert_eq!(describe(&state(0, 1, Unit::Items)), "0 of 1 item");
    assert_eq!(
        describe(&state(1_500, 3_000_000, Unit::Bytes)),
        format!("{} of {}", format_bytes(1_500), format_bytes(3_000_000))
    );
}

#[test]
fn work_that_cannot_be_counted_says_working() {
    assert_eq!(describe(&state(0, 0, Unit::Items)), "Working\u{2026}");
    assert_eq!(describe(&state(0, 1, Unit::Items)), "0 of 1 item");
}

/// ETA and speed only appear when they mean something: after a couple of
/// seconds, with work done and left, and speed only for bytes.
#[test]
fn eta_and_speed_appear_only_when_meaningful() {
    let items = state(25, 100, Unit::Items);
    assert_eq!(eta_label(&items, Duration::from_secs(1)), None, "too early");
    assert_eq!(
        eta_label(&state(0, 100, Unit::Items), Duration::from_secs(10)),
        None,
        "nothing done yet"
    );
    assert_eq!(
        eta_label(&state(100, 100, Unit::Items), Duration::from_secs(10)),
        None,
        "already finished"
    );
    assert_eq!(
        eta_label(&items, Duration::from_secs(10)),
        Some("about 30 s left".to_owned())
    );
    assert_eq!(
        eta_label(&state(1, 10, Unit::Items), Duration::from_secs(60)),
        Some("about 9 min left".to_owned())
    );

    assert_eq!(speed_label(&items, Duration::from_secs(10)), None);
    let bytes = state(20_000_000, 100_000_000, Unit::Bytes);
    assert_eq!(speed_label(&bytes, Duration::from_millis(500)), None);
    assert_eq!(
        speed_label(&bytes, Duration::from_secs(10)),
        Some(format!("{}/s", format_bytes(2_000_000)))
    );
}

/// The sink is the worker's side of the card: clones share one state and one
/// cancel flag, which only ever goes from unset to set.
#[test]
fn the_sink_shares_progress_and_cancellation_between_clones() {
    let sink = Sink::new(Unit::Items);
    let worker = sink.clone();
    assert!(!sink.cancelled());
    worker.set_total(4);
    worker.advance(1, "a.txt");
    assert_eq!(
        sink.snapshot(),
        ProgressState {
            done: 1,
            total: 4,
            current: Some("a.txt".to_owned()),
            unit: Unit::Items,
        }
    );
    sink.request_cancel();
    assert!(worker.cancelled());
}

/// Only a byte transfer (copy, move) has a pause control: its worker checks
/// a gate between chunks. Item jobs (trash, delete, permissions, links,
/// undo) and archive tools poll a bare cancel flag, so their card hides it.
#[test]
fn the_pause_button_shows_only_for_byte_transfers() {
    assert!(pausable(Unit::Bytes));
    assert!(!pausable(Unit::Items));
    assert_eq!(pause_label(false), "Pause");
    assert_eq!(pause_label(true), "Resume");
}

#[test]
fn the_sink_pauses_and_resumes_the_workers_gate() {
    let sink = Sink::new(Unit::Bytes);
    let worker = sink.clone();
    assert!(!sink.paused());
    sink.pause();
    assert!(worker.paused());
    assert!(worker.pause_gate().is_paused());
    sink.resume();
    assert!(!worker.paused());
}

/// A cancel reaches a worker blocked in the gate: the gate stays closed but
/// the worker is released to see the flag.
#[test]
fn cancelling_a_paused_sink_releases_a_blocked_worker() {
    let sink = Sink::new(Unit::Bytes);
    sink.pause();
    let worker = sink.clone();
    let blocked = std::thread::spawn(move || {
        worker.pause_gate().wait_while_paused(worker.cancel_flag());
        worker.cancelled()
    });
    std::thread::sleep(Duration::from_millis(100));
    sink.request_cancel();
    assert_eq!(blocked.join().ok(), Some(true));
}

fn active(registry: &Registry, now: Instant) -> Duration {
    registry
        .ops
        .first()
        .map(|op| op.active_elapsed(now))
        .unwrap_or_default()
}

fn is_paused(registry: &Registry) -> bool {
    registry.ops.first().is_some_and(|op| op.sink.paused())
}

fn registry_with_bytes_op() -> (Registry, u64, Instant) {
    let mut registry = Registry::default();
    let (id, _sink) = registry.start("Copying", Unit::Bytes, true);
    let started = registry
        .ops
        .first()
        .map_or_else(Instant::now, |op| op.started);
    (registry, id, started)
}

/// Active time is wall time minus every pause, including one still going.
#[test]
fn active_time_excludes_paused_time() {
    let (mut registry, id, t0) = registry_with_bytes_op();
    let s = Duration::from_secs;
    assert_eq!(active(&registry, t0 + s(10)), s(10));

    assert!(registry.pause(id, t0 + s(10)));
    assert!(is_paused(&registry));
    assert_eq!(active(&registry, t0 + s(40)), s(10), "frozen");

    assert!(registry.resume(id, t0 + s(40)));
    assert!(!is_paused(&registry));
    assert_eq!(active(&registry, t0 + s(45)), s(15));

    // A second pause adds to the first.
    registry.pause(id, t0 + s(50));
    registry.resume(id, t0 + s(60));
    assert_eq!(active(&registry, t0 + s(70)), s(30));
    // Pausing twice or resuming a running op changes nothing.
    assert!(!registry.resume(id, t0 + s(71)));
    registry.pause(id, t0 + s(80));
    assert!(!registry.pause(id, t0 + s(90)));
    assert_eq!(active(&registry, t0 + s(100)), s(40));
}

/// Speed and ETA come from active time, so a long pause does not drag them
/// down: 20 MB in 10 active seconds is 2 MB/s however long it sat paused.
#[test]
fn speed_and_eta_ignore_the_time_spent_paused() {
    let (mut registry, id, t0) = registry_with_bytes_op();
    let s = Duration::from_secs;
    registry.pause(id, t0 + s(10));
    registry.resume(id, t0 + s(310));
    let now = t0 + s(310);
    let bytes = state(20_000_000, 100_000_000, Unit::Bytes);
    let active = active(&registry, now);
    assert_eq!(active, s(10));
    assert_eq!(
        speed_label(&bytes, active),
        Some(format!("{}/s", format_bytes(2_000_000)))
    );
    assert_eq!(
        eta_label(&bytes, active),
        Some("about 40 s left".to_owned())
    );
    // The card still counts its wall-clock age for when it appears.
    assert_eq!(
        registry
            .ops
            .first()
            .map(|op| op.elapsed(now))
            .unwrap_or_default(),
        s(310)
    );
}

#[test]
fn pausing_an_item_job_or_an_unknown_id_does_nothing() {
    let mut registry = Registry::default();
    let (id, sink) = registry.start("Moving to Trash", Unit::Items, true);
    let now = Instant::now();
    assert!(!registry.pause(id, now));
    assert!(!sink.paused());
    assert!(!registry.pause(id + 1, now));
    assert!(!registry.resume(id + 1, now));
}

#[test]
fn the_card_says_paused_instead_of_speed_and_eta() {
    assert_eq!(status_word(false, false), None);
    assert_eq!(status_word(true, false), Some("Paused"));
    assert_eq!(status_word(true, true), None, "cancelling wins");
    assert_eq!(status_word(false, true), None);
}
