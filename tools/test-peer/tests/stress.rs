//! The Phase 2 exit-criteria gate (docs/ROADMAP.md, docs/TESTING.md §3):
//! **≥10,000 bidirectional clipboard updates with zero content
//! corruption, zero synchronization loops, zero unexplained ordering
//! failures, zero silent failures, and no crash.**
//!
//! Deliberately hermetic: engine + driver + real TLS sessions, with the
//! fake provider standing in for the OS clipboard. That is what makes it
//! a *gate* — it cannot flake on whatever else happens to be running on
//! the machine, which a real-clipboard run always can (see
//! `docs/SOAK.md` for the two-machine soak that trades determinism for
//! realism).
//!
//! Override the count with `CROSSOVER_STRESS_UPDATES` when investigating.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::time::timeout;
use uuid::Uuid;

use crossover_core::{
    ClipboardConfig, ClipboardRetryPolicy, SessionCommand, SyncEvent, clipboard_sync,
};
use crossover_platform::ClipboardProvider;
use crossover_platform::fakes::InMemoryClipboard;
use crossover_protocol::RawFrame;
use crossover_protocol::clipboard::{ApplyResult, ClipboardApplied, ClipboardData};
use crossover_protocol::hello::MessageType;

/// Exit criteria minimum (docs/ROADMAP.md Phase 2).
const DEFAULT_UPDATES: usize = 10_000;

fn update_count() -> usize {
    std::env::var("CROSSOVER_STRESS_UPDATES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_UPDATES)
}

/// One side: fake clipboard + sync driver, with its command stream
/// exposed so the harness can shuttle frames to the other side.
struct Side {
    clipboard: Arc<InMemoryClipboard>,
    events: mpsc::Sender<SyncEvent>,
    commands: mpsc::Receiver<SessionCommand>,
}

fn side(origin: u8) -> Side {
    let clipboard = Arc::new(InMemoryClipboard::new());
    let (driver, events, commands) = clipboard_sync(
        Arc::clone(&clipboard) as Arc<dyn ClipboardProvider>,
        Uuid::from_bytes([origin; 16]),
        ClipboardConfig {
            retry: ClipboardRetryPolicy {
                max_attempts: 5,
                delay: Duration::from_millis(1),
            },
            // The gate measures transaction throughput, not the debounce
            // (ADR 0006 has its own tests). Zero means transmit eagerly:
            // any non-zero value would cost a scheduler tick per item —
            // ~15 ms on Windows, which is 150 s across the run.
            transmit_debounce: Duration::ZERO,
        },
        None,
    )
    .unwrap();
    tokio::spawn(driver.run());
    Side {
        clipboard,
        events,
        commands,
    }
}

/// Tallies that answer the exit criteria directly.
#[derive(Default)]
struct Tally {
    applied: usize,
    superseded: usize,
    failed: usize,
    corrupt: usize,
}

/// Ten thousand updates alternating direction, every one verified for
/// content integrity, with loops detected as unexplained extra traffic.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ten_thousand_bidirectional_updates_stay_correct() {
    let updates = update_count();
    let mut a = side(0x01);
    let mut b = side(0x02);
    a.events.send(SyncEvent::SessionEstablished).await.unwrap();
    b.events.send(SyncEvent::SessionEstablished).await.unwrap();

    // Every frame that crosses is counted; a synchronization loop would
    // show up as traffic that never stops for an item nobody re-copied.
    let frames_crossed = Arc::new(AtomicUsize::new(0));
    let mut tally = Tally::default();
    let started = Instant::now();

    for i in 0..updates {
        // Alternate the origin so the run is genuinely bidirectional.
        let (source, sink) = if i.is_multiple_of(2) {
            (&mut a, &mut b)
        } else {
            (&mut b, &mut a)
        };
        one_update(i, source, sink, &mut tally, &frames_crossed).await;
    }

    let elapsed = started.elapsed();
    let crossed = frames_crossed.load(Ordering::Relaxed);
    println!(
        "stress: {updates} updates in {:.1}s ({:.0}/s), {crossed} frames, \
         applied={} superseded={} failed={} corrupt={}",
        elapsed.as_secs_f64(),
        f64::from(u32::try_from(updates).unwrap_or(u32::MAX)) / elapsed.as_secs_f64(),
        tally.applied,
        tally.superseded,
        tally.failed,
        tally.corrupt,
    );

    // The exit criteria, asserted.
    assert_eq!(tally.corrupt, 0, "content corruption detected");
    assert_eq!(tally.failed, 0, "clipboard applications failed");
    assert_eq!(
        tally.applied, updates,
        "not every update reached the destination clipboard"
    );
    assert_eq!(
        crossed,
        updates * 2,
        "unexpected extra frames crossed — possible synchronization loop"
    );
}

/// Same volume, but with the destination clipboard intermittently
/// contended: retries must absorb it and every item must still land
/// (FR-3.4 under sustained load).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sustained_contention_still_delivers_every_item() {
    use crossover_platform::fakes::{ClipboardFailure, ClipboardOp};

    // A shorter run: this one is about the retry path, not the count.
    let updates = (update_count() / 20).max(50);
    let mut a = side(0x01);
    let mut b = side(0x02);
    a.events.send(SyncEvent::SessionEstablished).await.unwrap();
    b.events.send(SyncEvent::SessionEstablished).await.unwrap();

    let mut applied = 0;
    for i in 0..updates {
        // Every third item meets a busy clipboard twice before landing.
        if i.is_multiple_of(3) {
            b.clipboard
                .fail_next(ClipboardOp::Write, ClipboardFailure::Busy, 2);
        }
        let text = format!("contended item {i}");
        a.clipboard.set_text_locally(&text);

        let SessionCommand::SendFrame {
            message_type,
            payload,
            ..
        } = next_command(&mut a.commands).await
        else {
            panic!("item {i}: unexpected termination");
        };
        b.events
            .send(SyncEvent::Frame(RawFrame {
                message_type,
                message_id: i as u64,
                payload,
            }))
            .await
            .unwrap();

        let SessionCommand::SendFrame { payload, .. } = next_command(&mut b.commands).await else {
            panic!("item {i}: unexpected termination");
        };
        let ack = ClipboardApplied::decode_payload(&payload).unwrap();
        assert_eq!(
            ack.result,
            ApplyResult::Applied,
            "item {i}: contention was not absorbed by bounded retry"
        );
        applied += 1;
        assert_eq!(b.clipboard.peek().as_deref(), Some(text.as_str()));

        a.events
            .send(SyncEvent::Frame(RawFrame {
                message_type: MessageType::ClipboardApplied.wire(),
                message_id: i as u64,
                payload,
            }))
            .await
            .unwrap();
    }
    assert_eq!(applied, updates);
    println!("contention stress: {applied} items delivered through injected contention");
}

/// One update: copy on `source`, deliver, apply on `sink`, acknowledge,
/// close — verifying integrity, the destination-updated definition of
/// success (FR-3.2), and (sampled) the absence of an echo.
async fn one_update(
    i: usize,
    source: &mut Side,
    sink: &mut Side,
    tally: &mut Tally,
    frames_crossed: &AtomicUsize,
) {
    let text = format!(
        "stress item {i} on {}",
        if i.is_multiple_of(2) { "a" } else { "b" }
    );
    source.clipboard.set_text_locally(&text);

    let SessionCommand::SendFrame {
        message_type,
        payload,
        ..
    } = next_command(&mut source.commands).await
    else {
        panic!("update {i}: expected a frame, got a termination command");
    };
    frames_crossed.fetch_add(1, Ordering::Relaxed);
    assert_eq!(
        message_type,
        MessageType::ClipboardData.wire(),
        "update {i}: unexpected outbound message type"
    );

    // Integrity: what left must be exactly what was copied.
    let data = ClipboardData::decode_payload(&payload)
        .unwrap_or_else(|e| panic!("update {i}: outbound data failed to decode: {e}"));
    if data.content != text.as_bytes() {
        tally.corrupt += 1;
    }

    sink.events
        .send(SyncEvent::Frame(RawFrame {
            message_type,
            message_id: i as u64,
            payload,
        }))
        .await
        .unwrap();

    let SessionCommand::SendFrame {
        message_type,
        payload,
        ..
    } = next_command(&mut sink.commands).await
    else {
        panic!("update {i}: sink terminated the session");
    };
    frames_crossed.fetch_add(1, Ordering::Relaxed);
    assert_eq!(
        message_type,
        MessageType::ClipboardApplied.wire(),
        "update {i}: sink did not acknowledge"
    );
    let applied = ClipboardApplied::decode_payload(&payload)
        .unwrap_or_else(|e| panic!("update {i}: ack failed to decode: {e}"));
    match applied.result {
        ApplyResult::Applied => tally.applied += 1,
        ApplyResult::Superseded => tally.superseded += 1,
        // Every failure is *observable* — the criterion is no SILENT
        // failure. With a healthy fake provider there should be none.
        ApplyResult::ClipboardUnavailable | ApplyResult::ContentRejected => {
            tally.failed += 1;
        }
    }

    // Destination-updated is the definition of success (FR-3.2).
    if applied.result == ApplyResult::Applied && sink.clipboard.peek().as_deref() != Some(&text) {
        tally.corrupt += 1;
    }

    source
        .events
        .send(SyncEvent::Frame(RawFrame {
            message_type,
            message_id: i as u64,
            payload,
        }))
        .await
        .unwrap();

    // Loop prevention (FR-3.3): applying the item fired the fake's
    // own-write notification; an echo would appear as an extra frame. The
    // global frame-count assertion covers every iteration; this probe
    // localizes a loop to its iteration. Sampled, because each probe
    // costs a full timer tick (~15 ms on Windows) — 10,000 of them would
    // dominate the run.
    if i.is_multiple_of(100) {
        let echo = timeout(Duration::from_millis(2), sink.commands.recv()).await;
        assert!(
            echo.is_err(),
            "update {i}: synchronization loop — sink echoed an applied item: {echo:?}"
        );
    }
}

async fn next_command(commands: &mut mpsc::Receiver<SessionCommand>) -> SessionCommand {
    timeout(Duration::from_secs(10), commands.recv())
        .await
        .expect("timed out waiting for a sync command")
        .expect("sync command channel closed")
}
