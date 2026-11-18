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
