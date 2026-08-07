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
