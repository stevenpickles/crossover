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
  the slower `park_delay` = **1 s** cadence, and by the **settle read**
  that follows a local change notification, until `park_budget` = **20 s**
  elapses. `Unavailable` and `UnsupportedType` are unchanged: neither is a
  statement that will be different in a second.
- Only when the parked budget runs out does `ClipboardUnavailable`
  travel. **No protocol change**: the verdicts, their meanings, and the
  requirement that a transaction closes only on the destination's
  `ClipboardApplied` are all exactly as before.

Once a second is the cadence for the timer because the parked phase must be
a good neighbour: re-taking the machine-global lock five times a second for
twenty seconds is precisely how Crossover made other applications'
clipboard calls fail in the two-machine soak ([SOAK.md](../SOAK.md), ADR
0006's context).

**The read revives it, and the notification deliberately does not.** The
first design retried on the notification itself, on the reasoning that a
notification is the best evidence available that whoever held the clipboard
has let go. That reasoning is sound and the conclusion was wrong, because
it ignores *why* the clipboard usually changes: on Windows a
`WM_CLIPBOARDUPDATE` almost always means new content just landed. A parked
install taking that moment would write over the copy the user made an
instant earlier — and worse, silently: the settle read would then find our
own content, recognize it through the applied-hash memory, suppress it as a
loop, and report nothing at all. The user's copy would be gone with no
diagnostic anywhere, which is a strictly worse fault than the one being
fixed.

So the notification only starts the settle clock (ADR 0006), and the
**read** decides, because the read is the first moment anything knows what
the clipboard actually holds:

| What the read finds | What the parked install gets |
|---|---|
| Content we ourselves installed (loop-suppressed) | Retried now — the clipboard is free and holds nothing of the user's |
| Content unchanged since we last looked | Retried now, same reasoning |
| Genuinely new content | **Superseded** — the user outranks it |
| Nothing readable | Left alone; its own timer decides |

The cost is one settle window — 300 ms — with the 1 s parked timer as the
backstop underneath. That is a small price for never guessing wrong about
whose content is on the clipboard.

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
- **A local copy** supersedes it, and this is new. A read that finds
  content which is neither a duplicate nor our own write means this
  machine's user put it there. Installing a peer item over it fifteen
  seconds later would destroy content the user just made — a worse fault
  than the one being fixed. Inside the fast budget the question does not
  arise, so only a *parked* install loses this way.
- **An item that already matches this clipboard** closes it. The echo
  guard answers such an item `Applied` without writing anything, which is
  correct — and an older install left pending behind that answer would
  later write its own content over the clipboard both machines had just
  agreed on, with its own-write notification loop-suppressed so nothing
  noticed. Silent, permanent divergence from a path whose whole purpose is
  agreement.
- **The session ending** drops it, without a verdict and without counting
  a failure. There is nobody to send a verdict to, and the outbound slot
  beside it is cleared uncounted for the same reason. This one is not
  about ranking: an install that can live for twenty seconds can outlive
  its session, land during the *next* one, and overwrite whatever the user
  did in between — answering a peer that stopped waiting long ago. The
  content is not lost, because the peer re-announces on reconnect (ADR
  0006, trigger 3) and the read revival below is what makes sure that
  re-announcement is heard.

**The reconnect re-read is not a local copy**, and saying so took a change
of mechanism. `on_session_established` used to clear the dedup hash so the
re-read would announce regardless — which threw away the one fact the rule
above needs, making every reconnect's re-read of unchanged content
indistinguishable from the user copying something. A parked install would
therefore be superseded by the reconnect that was trying to deliver it: the
2026-09-01 scenario, defeated by its own fix. The hash is now kept and a
re-announce flag set beside it, so the read can say "announce this anyway"
and "this is not new" at the same time, which are two different statements
and always were.

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
- **It does not protect a copy this build cannot read.** This is the one
  known residual of the rule above, and it is the same fault the
  notification-versus-read ordering fixes, narrowed to a case the engine
  cannot currently see.

  The failure shape: the user copies something in a format Crossover does
  not sync — an application's private format, RTF-only content, anything
  the provider will not render — so the read answers `Ok(None)`. The
  engine cannot tell that from *the clipboard is empty*, and an empty
  clipboard is not content worth protecting, so it treats `None` as no
  evidence either way and leaves the parked install to its timer. That
  timer fires within a second and installs the peer's item over the user's
  copy. It is narrower than the original defect — it needs an unsyncable
  copy during a parked window rather than any copy at all — but the outcome
  is the same, and it is equally silent.

  A related consequence of the same blindness: `current_local_hash` is
  **not** cleared on `None`, so after an unreadable copy the engine still
  believes the last content it could read is on the clipboard. A later read
  of that same content therefore looks unchanged rather than new — correct
  for dedup, and one more reason the engine cannot infer the truth here.

  The fix direction is a provider-level change and belongs to its own
  branch: `ClipboardProvider::read` distinguishes **`Empty`** from
  **`Unreadable`**, and `Unreadable` joins "genuinely new content" as a
  local copy that supersedes a parked install. Deliberately not done here,
  because it widens a platform trait that three backends implement, which
  is not a change to make inside a retry fix.
