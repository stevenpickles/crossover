# 0005. Clipboard transactions: 2-message inline, Offer/Accept above 64 KiB

Status: Accepted
Date: 2026-08-07

## Context

[PROTOCOL.md](../PROTOCOL.md) §5 deferred the clipboard transaction shape:
a uniform 4-message flow (Offer → Accept → Data → Applied) as the baseline
specification drafted it, versus a 2-message flow (Data → Applied) for the
common case. The non-negotiable semantic is fixed either way: **a sync
succeeds only when the destination OS clipboard was updated**, confirmed
end to end (FR-3.2).

Two facts drive the choice. Most text clipboard items are tiny — a URL, a
paragraph, a code snippet — where two extra round trips buy nothing and
cost copy-to-paste latency on every single copy. And the genuine value of
an offer round exists only for large items: the receiver can decline a
hash it already holds (re-copies of the same content transfer zero
payload bytes) and controls its memory commitment before megabytes
arrive.

## Decision

Split by size, with the threshold and bounds as named protocol constants:

- **Inline flow** for items whose content is ≤ `CLIPBOARD_INLINE_MAX_BYTES`
  = **64 KiB**: `ClipboardData` → `ClipboardApplied`. One round trip;
  covers the overwhelming majority of real clipboard traffic.
- **Offered flow** for larger items: `ClipboardOffer` (id, type, length,
  hash) → `ClipboardAccept` or `ClipboardDecline` (with a typed reason —
  including already-have-this-hash, which is a success for
  synchronization purposes) → `ClipboardData` → `ClipboardApplied`.
- **`MAX_CLIPBOARD_TEXT_BYTES` = 4 MiB.** Larger items are rejected
  gracefully on both send and receive with an observable diagnostic
  (FR-3.6) — never truncated, never silently dropped.
- **`MAX_FRAME_BODY_BYTES` rises to 4 MiB + 64 KiB** so one maximum item
  plus its envelope always fits a single frame. **No chunking in
  Phase 2**: reassembly state for >4 MiB *text* is complexity without a
  demonstrated need. Chunking gets its own ADR if rich clipboard types
  (images, files — Phase 8) demand it.
- `ClipboardApplied` carries a typed result: applied, or a typed failure
  (e.g. clipboard-unavailable-after-bounded-retries), so the origin can
  distinguish success from every failure mode (NFR-3).
- Retry bounds and the latest-observed-wins conflict tie-break remain the
  engine's to implement, but must be centrally defined constants and a
  deterministic, documented, tested rule (FR-3.4, FR-3.5) — not
  scattered literals.

## Alternatives Considered

- **Uniform 4-message flow** (the baseline draft's shape): uniform code
  path, but taxes every small copy with two extra round trips for a
  benefit (pre-transfer dedup, memory control) that only materializes on
  large items. Rejected: clipboard latency is felt by users on exactly
  the small items the tax hits hardest.
- **Uniform 2-message flow**: simplest possible, but a peer re-copying
  the same 4 MiB item would retransfer all of it every time, and the
  receiver commits maximum-size buffers on the sender's say-so alone.
  Rejected: the offer round is worth its cost precisely and only above a
  threshold.
- **Chunked transfer for arbitrary sizes**: solves a problem text does
  not have; deferred with its own ADR trigger.

## Consequences

- Easier: minimal latency for the common case; large-item dedup by hash
  before transfer; single-frame items keep the framing layer and its
  bounds story unchanged (one frame = one message, no reassembly state).
- Harder: two code paths through the engine's transaction state machine,
  both of which must be exercised by the Phase 2 test suites, including
  the threshold boundary itself.
- The frame-size ceiling grows to ~4.06 MiB; the decoder's
  before-allocation length validation and buffer cap (already in place)
  are what make that safe against NFR-1.
