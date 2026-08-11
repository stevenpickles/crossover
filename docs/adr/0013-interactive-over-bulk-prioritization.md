# 0013. Interactive input takes wire priority over bulk transfers

Status: Proposed (Phase 7 — design captured 2026-08-11, not yet scheduled)
Date: 2026-08-11

## Context

A session is a **single TLS-over-TCP stream**, and today the send path is a
single FIFO channel — `outbound: mpsc::Receiver<(u16, Vec<u8>)>` drained in
order in `run_session` (`crossover-core/src/supervision.rs`), with input
batches, control, and clipboard all writing to the one writer through it. That
is fine while every payload is small.

Phase 7 rich-clipboard transfers (images, files — [ADR 0014](0014-chunked-rich-clipboard-transfer.md))
introduce multi-MB, occasionally much larger, payloads on that same stream. A
large payload queued ahead of an input batch would **head-of-line block** the
pointer and keyboard — directly violating priority #5 (low input latency) and
NFR-5 ("pointer movement feels immediate"). The maintainer's requirement is
explicit: **background data transfer must never interfere with live input.**

The constraint is *ordering*, not throughput: the target LAN is 2.5 GbE, so
bandwidth is ample; the problem is that one ordered stream serializes bulk
ahead of input.

## Decision (proposed)

Keep the single connection; split the session send path into **two priority
classes**:

- **High:** input batches, control (request/grant/release), keepalive.
- **Background:** bulk clipboard/image/file chunks.

The writer **always drains High before Background**, and emits **at most one
Background chunk before re-checking High** — so a freshly-arrived input batch
goes out ahead of the next chunk. Two FIFO queues preserve each class's
internal ordering (input sequence, chunk sequence) while letting input preempt
bulk *between* chunks.

This makes chunking load-bearing, not just a memory device: a single large
frame is **unpreemptable** (its `writer.send(&payload).await` occupies the
writer until fully out), so bulk **must** be chunked and the chunk is the
**preemption unit**. Chunk size is therefore a *latency* knob — worst-case
input delay ≈ one chunk's transmit time — chosen small enough that at 2.5 GbE
it is sub-millisecond.

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
- Reshapes the session's single `outbound` channel into priority classes — a
  small code change but an architecturally load-bearing one, hence this ADR.
- Foundational for [ADR 0014](0014-chunked-rich-clipboard-transfer.md): the
  chunk size chosen there answers to this ADR's latency budget.

## Open questions (to settle when scheduled)

- Exact chunk size (latency budget vs per-frame overhead).
- Whether small clipboard *text* rides High or Background (it is tiny either
  way).
- Backpressure policy on the Background queue; where to measure input latency
  under load to prove the guarantee.
