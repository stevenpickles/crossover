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

## Addendum (2026-09-01): the bounded retry has two phases, not one

This ADR required "retry bounds ... a deterministic, documented, tested
rule", and the implementation read that as one fixed schedule: five
attempts, 200 ms apart, then `ClipboardApplied { ClipboardUnavailable }`.
Eight hundred milliseconds of patience, and hardware found the gap in it.

On machine A (2026-09-01, build 84eea30), at **five of eight peer
reconnects**, the peer's re-announced item failed to install within about
a second of the session establishing: `clipboard item could not be
installed … attempt_count=5 result=ClipboardUnavailable`, alongside
`clipboard read still busy` on the same clipboard. Something external held
the machine-global lock for roughly a second — long enough to outlive the
budget by a hair, and every time it did, a user's clipboard item was
permanently gone. Clipboard reliability is priority #2
([SPECIFICATION.md](../SPECIFICATION.md) §2); a contended second is not an
acceptable price for it.

The fault was not the bound. It was that **one schedule was being asked to
model two different situations.** Five attempts at 200 ms is exactly right
for the situation it was designed against: another application between
`OpenClipboard` and `CloseClipboard`, resolved in milliseconds, where
polling hard wins. It is simply not a model of a holder that is *doing
something* for a second or two.

**Two phases, therefore, with the wire contract unchanged.**

- **Fast phase** — `max_attempts` = 5, `delay` = 200 ms. Byte for byte
  what this ADR already specified, for the blip it already modelled.
- **Parked phase** — entered when the fast phase is exhausted and the
  failure is still `Busy`. The install is *not* failed. It is retried on
  the slower `park_delay` = **1 s** cadence, and immediately on **every
  local change notification**, until `park_budget` = **20 s** elapses.
  `Unavailable` and `UnsupportedType` are unchanged: neither is a
  statement that will be different in a second.
- Only when the parked budget runs out does `ClipboardUnavailable`
  travel. **No protocol change**: the verdicts, their meanings, and the
  requirement that a transaction closes only on the destination's
  `ClipboardApplied` are all exactly as before.

The change notification is the *primary* revival and the slow timer is the
backstop, in that order deliberately. A notification is the best evidence
available that whoever held the clipboard has let go, and it usually
arrives long before the next tick. Once a second is the cadence for the
tick because the parked phase must be a good neighbour: re-taking the
machine-global lock five times a second for twenty seconds is precisely
how Crossover made other applications' clipboard calls fail in the
two-machine soak ([SOAK.md](../SOAK.md), ADR 0006's context).

### The budget arithmetic, and whose deadline actually fixes it

The number that constrains the parked budget is not ours. It is the
**origin's**: an outbound transaction is abandoned after
`TRANSFER_TIMEOUT` = 60 s (ADR 0014). A receiver still retrying past that
point is answering a transaction nobody is listening to — the origin has
already released the item, counted an `abandoned`, and moved on — so the
verdict, whenever it arrives, lands on nothing.

The whole install budget must therefore finish comfortably *inside* 60 s:

| | |
|---|---|
| Fast phase | 5 × 200 ms = **1.0 s** (0.8 s of waiting, plus the attempts) |
| Parked phase | **20 s** |
| Last scheduled attempt past the budget check | ≤ 1 × `park_delay` = **1 s** |
| Worst case | **≈ 22 s** — roughly a third of the origin's patience |

The remaining two thirds absorb the network and any queueing on either
side. `the_whole_install_budget_fits_inside_the_origins_patience` asserts
the relation with a factor of two to spare, so raising either budget
without the other fails the build rather than producing a class of silent
stall. Twenty seconds is also an order of magnitude past the observed
one-second hold, which is what makes this a fix for the *class* of fault
rather than for one second.

### What outranks a parked install

An install that can live for twenty seconds needs answers to two
questions an install that lived for 800 ms did not.

- **A newer inbound item** supersedes it, as it always did — but it is now
  *told so*, with the `Superseded` verdict the coalescing driver and the
  conflict rule already use. This path previously dropped the write with a
  debug line and no verdict at all, leaving the origin to wait out its own
  deadline. That was survivable at 800 ms and is not at twenty seconds:
  this ADR's invariant is that **every transaction ends in a typed verdict
  within a bounded time**, and the addendum keeps it rather than
  stretching it.
- **A local copy** supersedes it, and this is new. Reaching a fresh local
  item means this machine's user put something on this machine's clipboard
  that is neither a duplicate nor our own write. Installing a peer item
  over it fifteen seconds later would destroy content the user just made —
  a worse fault than the one being fixed. Inside the fast budget the
  question does not arise, so only a *parked* install loses this way.

### The read side has the same two phases, for the same reason

The install was only half of what the 2026-09-01 evidence showed. The
mechanism that re-announces this machine's item on reconnect is a
**read**, and the read had a bounded nudge cycle with no revival at all:
past `MAX_CONSECUTIVE_BUSY_READS` the driver waited for the next change
notification, and with the clipboard holding content copied while the peer
was away, that is a change which never comes. Items copied during the gap
were simply never offered when the peer returned.

Two latent faults compounded it. The nudge re-enqueued the same
`LocalChanged` event the OS listener raises, so a genuine change was
indistinguishable from the driver talking to itself; and the consecutive
counter reset only on a *successful* read, so one contended episode left
every later busy read with no nudge scheduled at all. The nudge now has
its own event, the counter resets on a successful read, on a genuine
notification, and on session establishment — the re-announce read must not
inherit an earlier episode's exhausted budget — and past the fast nudges
the driver drops to a one-second cadence for twenty seconds instead of
going quiet. The soak's cure was slowing the churn, not abandoning the
read.

### What this deliberately does not do

- **It does not make the receiver retry forever.** "Keep trying until it
  works" has no bound, and an unbounded transaction is what ADR 0014's
  deadline exists to prevent.
- **It does not change the verdicts.** The origin still hears exactly
  `Applied`, `Superseded`, `ClipboardUnavailable`, `ContentRejected` or
  `Stored`. A receiver that parks and a receiver that does not are
  indistinguishable on the wire, which is what makes this deployable
  against an unchanged peer.
- **It does not identify the holder.** Naming what has the clipboard is a
  platform-layer concern and is handled separately (feature/162).
