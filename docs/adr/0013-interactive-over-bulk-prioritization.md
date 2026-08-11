# 0013. Interactive input takes wire priority over bulk transfers

Status: Accepted (Phase 7)
Date: 2026-08-11

## Context

A session is a **single TLS-over-TCP stream**, and today every application
frame reaches it through a chain of plain FIFO channels (capacity 64 at each
hop), with no notion of class or priority anywhere along it:

1. the clipboard driver and the input/control driver each emit
   `SessionCommand::SendFrame` on their own channel
   (`crossover-core/src/clipboard_driver.rs`, `control_driver.rs`);
2. `merge_command_streams` folds those into one `SessionCommand` stream that
   `spawn_command_mux` drains in order (`apps/crossover/src/commands.rs`),
   handing each frame to the target session's `FrameSink`;
3. the sink is either an accepted session's `mpsc::Sender<(u16, Vec<u8>)>` or
   `SupervisorHandle::send` for the supervised outbound session — both feed
   `outbound: mpsc::Receiver<(u16, Vec<u8>)>`;
4. `run_session` (`crossover-core/src/supervision.rs`) drains `outbound` in
   order and is the only place application frames touch the writer.

Keepalive is the one thing already outside that chain: `run_session` writes
`Ping` on its idle tick and `dispatch_frame` answers `Pong` straight to the
writer, never through `outbound`. All of this is fine while every payload is
small.

Phase 7 rich-clipboard transfers (images, files — [ADR 0014](0014-chunked-rich-clipboard-transfer.md))
introduce multi-MB, occasionally much larger, payloads on that same stream. A
large payload queued ahead of an input batch would **head-of-line block** the
pointer and keyboard — directly violating priority #5 (low input latency) and
NFR-5 ("pointer movement feels responsive, typing does not feel delayed"). The
maintainer's requirement is explicit: **background data transfer must never
interfere with live input.**

The constraint is *ordering*, not throughput: the target LAN is 2.5 GbE, so
bandwidth is ample; the problem is that one ordered stream serializes bulk
ahead of input.

## Decision

Keep the single connection; split the session send path into **two priority
classes**:

- **High:** `InputBatch`, `ReleaseAllInput`, the control-transfer messages
  (`ControlRequest`/`ControlResponse`/`ControlRelease`), and keepalive.
  `ReleaseAllInput` belongs here emphatically: a stuck key is a
  release-blocking defect, so it must never queue behind bulk.
- **Background:** bulk clipboard/image/file chunks.

The writer **always drains High before Background**, and emits **at most one
Background chunk before re-checking High** — so a freshly-arrived input batch
goes out ahead of the next chunk. Two FIFO queues preserve each class's
internal ordering (input sequence, chunk sequence) while letting input preempt
bulk *between* chunks.

The split spans the **whole path described above, not just its last hop.** A
chunk parked in a driver's `SessionCommand` channel or in the command mux
head-of-line blocks input exactly as effectively as one parked in `outbound`,
so whatever carries a frame from a driver to the writer must carry its class
with it. Two related rules follow: backpressure must not invert the priority
(today `spawn_command_mux` *awaits* delivery into a session's queue, so one
saturated Background path would stall the single mux task for every class and
every session), and the keepalive writes that already bypass the queue must
stay bypassed — they are High by construction.

This makes chunking load-bearing, not just a memory device: a single large
frame is **unpreemptable** (its `writer.send(message_type, &payload).await`
occupies the writer until fully out), so bulk **must** be chunked and the
chunk is the **preemption unit**. Chunk size is therefore a *latency* knob —
worst-case input delay ≈ one chunk's transmit time — chosen small enough that
at 2.5 GbE it is sub-millisecond.

This does not invert the priority order of SPECIFICATION.md §2, where
clipboard reliability (#2) outranks low input latency (#5). Nothing here drops
or risks a clipboard item: chunks are reordered *behind* input, never
discarded, and the transaction still closes on an acknowledged install. What
is deprioritized is clipboard *latency*, which the priority list never ranks
above input at all — the same trade ADR 0006 already made with the debounce.

Implementation reality to honor: **feed the socket one chunk at a time.** The
TLS/TCP writer and kernel send buffer preserve byte order, so writing a large
chunk parks input bytes behind it *in the kernel buffer*, undoing app-level
priority. Keeping chunks small and writing one at a time (re-checking High
between) keeps the kernel buffer shallow so the prioritization reaches the
wire. The receive side already dispatches per-frame, so interleaved input
frames are applied as they arrive while chunks route to reassembly.

## Alternatives Considered

- **Separate connections / QUIC independent streams** — bulk on its own stream
  so it cannot head-of-line block input at the app layer. Deferred: a second
  TLS handshake and binding two connections to one paired session is a
  protocol/trust change to solve a problem that app-level priority + small
  chunks already solves on a LAN. Kept as the fallback if measurement ever
  shows the scheduler insufficient.
- **Do nothing (single FIFO).** Rejected: the whole point of the requirement.

## Consequences

- Input stays snappy during a bulk transfer; the "background never interferes"
  guarantee becomes real and measurable.
- Reshapes the whole driver-to-writer frame path — the drivers'
  `SessionCommand` channels, the command mux, and the session's `outbound`
  channel — into priority classes. Contained, but it touches every hop and is
  architecturally load-bearing, hence this ADR.
- Foundational for [ADR 0014](0014-chunked-rich-clipboard-transfer.md): the
  chunk size chosen there answers to this ADR's latency budget.

## Open questions (to settle when scheduled)

The decision above is fixed. These were the loose ends; where implementation
has since settled one, the resolution is noted inline — bookkeeping, not a
change of decision.

- Exact chunk size (latency budget vs per-frame overhead). **Still open**,
  and belongs with [ADR 0014](0014-chunked-rich-clipboard-transfer.md)'s
  chunking work.
- Whether small clipboard *text* rides High or Background (it is tiny either
  way). **Settled: Background, along with every other clipboard message.**
  Splitting a transaction across classes would let its acknowledgement
  overtake its data, which the ADR 0005 state machine forbids. See
  `SendPriority::of` in `crossover-core/src/outbound.rs` and
  [ARCHITECTURE.md](../ARCHITECTURE.md) §5.4.
- Backpressure policy on the Background queue, and how much starvation of
  Background under sustained input is acceptable before a transfer is
  considered stalled (strict High-first drain permits unbounded starvation).
  **Settled: block, never drop — bounded by bytes as well as message count —
  and accept unbounded starvation without aging.** Rationale in
  [ARCHITECTURE.md](../ARCHITECTURE.md) §5.4.
- Where to measure input latency under load to prove the guarantee.
  **Settled: `tools/test-peer/tests/priority.rs`**, which saturates every
  queue and the socket and asserts structurally — arrival positions and frame
  counts, not elapsed time ([TESTING.md](../TESTING.md) §1.5). Numeric
  latency remains a measurement (TESTING.md §4), not a gate.
