# 0006. Clipboard transmission is trigger-driven, not change-driven

Status: Accepted
Date: 2026-08-07
Amends: [0005](0005-clipboard-transaction-flow.md) (transaction *shape* is
unchanged; *when* a transaction starts is redefined)

## Context

ADR 0005 fixed how a clipboard item travels. It left implicit an
assumption inherited from the baseline specification: that an item
travels **as soon as it is observed**. The two-machine soak
(docs/SOAK.md) showed what that costs.

With both machines copying every 300 ms, machine A performed **52
clipboard write cycles in one second** while draining a backlog — 60 in
nine seconds. Each cycle takes the machine-global clipboard lock, and
that was enough to make PowerShell's `Set-Clipboard` fail outright on
both machines. Crossover was breaking its own users' copy operations.

Two fixes were applied and both were correct but insufficient: the Win32
critical section was shortened to the clipboard calls alone, and the
driver now coalesces a backlog to apply only the newest item. Neither
addresses the premise, which is that **most of that work should never
have been started**. Every item written between one paste and the next
is speculative: the clipboard is a single-value register, so all but the
last are content nobody can ever paste.

The user-visible model is not "replicate clipboard state changes". It
is:

> Copy here. Move there. Paste.

The moment the far machine's clipboard needs to be current is the moment
the user goes there — which, once input sharing exists, is exactly the
control-transfer boundary (Phase 5). Synergy-family tools transmit the
clipboard on screen switch for this reason.

## Decision

Clipboard transmission is **trigger-driven**. Observation is unchanged —
every local change is still observed, hashed, and turned into a
`ClipboardItem` — but an item is transmitted only when a trigger fires.

Triggers, in the order they arrive in the roadmap:

1. **Settled-change debounce (Phase 2, now).** After the local clipboard
   changes, wait for it to stay unchanged for a debounce interval, then
   transmit the current item. Rapid successive copies produce one
   transmission, not one per copy. This keeps Phase 2's standalone
   clipboard prototype demonstrable with no dependency on input sharing.
2. **Control transfer (Phase 5).** When the pointer crosses to the peer,
   transmit before handing over control — the moment the far clipboard
   must be correct. This becomes the *primary* trigger; the debounce
   remains as the fallback that keeps clipboard-only use working.
3. **Session establishment (already implemented).** Re-announce on
   connect so peers converge after any gap.

Freshness is decided by the existing item identity, never by wall-clock
comparison between machines: a trigger carries `(content_hash,
sequence, origin)`, and a receiver already holding that hash declines
with `AlreadyHave` — a synchronization success that moves zero payload
bytes. Cross-machine "newer" therefore never depends on clock
agreement, which two machines do not have.

Debounce interval: **300 ms**, a named constant. Long enough to collapse
a burst of copies, short enough that copy-then-immediately-switch still
finds the item in flight or already delivered.

## Alternatives Considered

- **Keep transmitting on every change** (the status quo, with the
  coalescing and critical-section fixes as mitigation). Rejected: it
  treats a symptom. The write storm is reachable again from any backlog
  — every reconnect flushes one — and it spends the machine-global lock
  on content that is stale before it lands.
- **Transmit only on control transfer** (the purest form of the idea).
  Rejected as the sole trigger: Phase 2 would have no trigger at all,
  making the Secure Clipboard Prototype undemonstrable, and users who
  copy on one machine then use the other's own keyboard would never
  sync. It is adopted as the primary trigger in Phase 5, not the only
  one.
- **Pull-based exchange on trigger** (receiver asks for the sender's
  hash and pulls on difference). Rejected for now as unnecessary
  mechanism: the offered flow's `AlreadyHave` decline already achieves
  zero-payload dedup, and a pull adds a round trip to the
  latency-sensitive transfer moment. Revisit if Phase 5 measurement
  shows the push wasting bandwidth.

## Consequences

- Easier: dramatically less clipboard and network traffic; the
  contention storm becomes unreachable rather than mitigated; Crossover
  becomes a good neighbour to other applications by construction; the
  conflict-resolution path is exercised far less often in practice.
- Harder: a small, deliberate latency floor (the debounce) between
  copying and the item being available on the peer. Accepted knowingly —
  FR-3.2 already ranks clipboard *correctness* above clipboard latency,
  and 300 ms is well inside the "copy then switch machines" window.
- The engine gains a timer-driven transmission step, so its
  `(state, event)` surface grows a `DebounceElapsed` event. State
  machine purity is preserved: the driver owns the clock, as it already
  does for write retries.
- Phase 5 must wire control transfer into this trigger set; recorded in
  docs/ROADMAP.md so it is not rediscovered.
- The 10,000-update stress gate keeps driving the engine directly, so it
  measures transaction throughput rather than debounce behavior. A
  dedicated test covers the debounce itself.

## Addendum (2026-09-01): a change with no peer is recorded, not transmitted

This ADR redefined *when* a transaction starts and left one case
unstated, because at the time it could not arise: what a trigger means
when there is nobody on the other end. The implementation answered it by
not asking. Every trigger minted a transaction, whatever the state of the
world.

On machine A (2026-08-31, build 84eea30), with the peer asleep for eight
hours — this pair spends whole days apart, by design — every local copy
produced, sixty seconds later:

```
WARN clipboard: outbound clipboard transaction abandoned: no answer
     within the deadline clipboard_id=… byte_count=N
     retained_content=false result="abandoned"
```

Twenty of them in one evening, each with a matching `clipboard_abandoned`.
Nothing had gone wrong. The frame went out as a `Broadcast` that the
application dropped for want of a sink; the deadline
([ADR 0014](0014-chunked-rich-clipboard-transfer.md)) then expired against
a peer that had never been asked, because a deadline cannot distinguish
"did not answer" from "was not there".

The cost is not the log noise. `clipboard_abandoned` is the *only* signal
for a class of silent stall — an offer refused locally, a peer that
accepts and goes quiet — and a counter that also fills up with ordinary
offline evenings cannot be read for the thing it exists to find.
Clipboard reliability is priority #2
([SPECIFICATION.md](../SPECIFICATION.md) §2), and this made its principal
instrument unreadable.

### The rule

**A local change with no live session is recorded, not transmitted.**

- *Recorded*, in full: the content is read, hashed, and stored as the
  current local hash, so loop suppression and dedup behave exactly as
  they did. Observation was never the thing in question.
- *Not transmitted*: no outbound slot, no `transfer_timeout` armed, no
  frame. Nothing exists that a deadline could later abandon, so the
  `abandoned` count means again what it says — **a peer was there and
  did not answer**.
- Counted as `clipboard_offline_changes`, so the copies are visible as
  what they are rather than as an absence.
- Logged once per offline stretch at `info`, and per copy at `debug`. A
  pair can be apart for a working day; one `info` line per copy for eight
  hours buries the lines a soak is read for.

Delivery is not lost, only moved to **trigger 3, which already existed**.
Session establishment marks a re-announcement pending and re-reads the
clipboard — the hash itself is *kept*, so the read still knows whether
what it finds is new, which is what stops a reconnect's re-read costing
a peer its parked install ([ADR 0005](0005-clipboard-transaction-flow.md),
addendum 2026-09-01). The pending mark is what lets the re-read offer
content the dedup would otherwise swallow, so the item that is current
when a peer arrives is offered whole — and that same addendum made the
read survive a contended clipboard. However many copies a gap
contained, the peer is offered the one item anybody could have pasted:
the clipboard is a single-value register, which is this ADR's own
argument for trigger-driven transmission in the first place.

### Liveness is a count, not a flag

A process can hold an inbound and an outbound session at the same time —
the listener and the reconnect supervisor run independently, so a machine
can be serving one peer while dialling another — and both fan
`SessionEstablished` / `SessionLost` into the one clipboard engine. A
boolean would be cleared by the first of two peers to drop, and every
copy after that would be silently held from the peer still connected: a
clipboard that stops working with nothing visible anywhere, which is a
worse fault than the one being fixed.

**What the count buys is exactly one thing, and it is worth being plain
about the limit.** It keeps *new copies* flowing to a session that
survives another session's loss. It does **not** make `on_session_lost`
session-aware: that method still tears down the outbound transaction,
the accepted offer, the pending install, the chunk reassembly and any
file build *unconditionally*, whichever session dropped — and whatever
the count reads, since a loss must drop an install that has not landed
even when the count is already skewed. So a transfer already in flight
to a surviving peer is still destroyed by an unrelated peer's
disconnect, and a file one still counts a `file_send_failed`. That is
unchanged by this addendum, and it is not what this addendum is about —
scoping the teardown means the engine tracking which session each
transfer belongs to, which is a change to the transaction model of
[ADR 0005](0005-clipboard-transaction-flow.md), not to the transmission
trigger.

**Named follow-up:** make `on_session_lost` tear down only the state
belonging to the session that dropped, so a concurrent peer's transfer
survives an unrelated disconnect — its own branch, deliberately not this
one.

The two events are strictly paired at every call site, and the count
saturates rather than wrapping. Saturating is not correcting: an
unpaired `SessionLost` leaves the count one low for good, so the last
real session to drop finds it already at zero and transmission stops
while a peer is still connected. The next establishment restores
*transmission*, not the count — the stall lifts, the skew stays — which
is why the engine warns when it reaches the saturating case rather than
absorbing it silently. That warning is the only evidence such a skew
would ever leave (FR-7.3).

### The file selection is gated the same way

A local `CF_HDROP` copy is gated on liveness *before* the `FileSend`
policy ([ADR 0015](0015-spooled-virtual-file-paste.md)). Nothing was at
risk: the application already publishes `Denied` when no session is live,
so no selection was being walked and no archive packed. What was wrong
was the diagnostic — answering an empty desk with "this peer holds no
clipboard-send grant" names a peer that does not exist, and charges
`files_send_refused` for a permission nobody was asked for. FR-3.6 wants
refusals visible; it does not want them invented.

### What this deliberately does not do

- **It does not queue.** Copies made during a gap are not held for replay.
  Only the current item is offered on connect, which is all that a
  single-value register can meaningfully deliver.
- **It does not change the wire.** A peer cannot tell the difference: with
  no session there is no peer to tell.
- **It does not keep the local sequence counter in step with the peer's.**
  An offline copy no longer consumes a `sequence`, so after a long gap
  this machine's counter lags a peer that stayed busy, and FR-3.5's
  `(sequence, origin)` tie-break in a genuine near-simultaneous race is
  correspondingly more likely to fall the peer's way. This is not a
  regression: the two counters are per-machine observation counts that
  were never synchronised, and the rule's guarantee is that the race
  resolves *deterministically and identically on both sides*, never that
  it resolves fairly. What changes is which arbitrary answer comes up,
  not whether the answer is agreed.
- **It does not touch the debounce, the conflict rule, or the deadline.**
  Every trigger behaves as before once a session is live.
