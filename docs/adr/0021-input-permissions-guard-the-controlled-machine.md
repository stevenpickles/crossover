# 0021. Input permissions guard the machine being controlled

Status: Accepted (2026-10-02, maintainer)
Date: 2026-09-29

## Context

The trust store has carried per-peer `keyboard` and `mouse` flags since
Phase 1 ([SECURITY.md](../SECURITY.md) §4), and both are set by pairing.
Neither has ever been enforced: SECURITY.md and the README now say so, and
Phase 9.0 enforced the two clipboard flags beside them (feature/170).

Enforcing them as documented would protect nothing. Their doc comments say
a peer "may receive **our** keyboard input" — they gate *this* machine
driving the peer. That is this machine's own choice, made every time its
user crosses an edge; a flag that stops it adds a way to be surprised, not
a defence.

The direction that matters is the other one, and nothing gates it. When a
paired peer sends a `ControlRequest`, `ControlEngine::on_peer_request`
grants it whenever this machine is not already controlling or controlled:
**any trusted peer may drive this machine's keyboard and pointer, and the
user of this machine has no way to say otherwise.** Of everything a peer
can do, injecting input is the most powerful — it is typing into whatever
window has focus — and it is the one capability with no per-peer control at
all. SECURITY.md's invariant 8 says a paired peer holds exactly the
capabilities its record grants; for input, the record grants nothing and
the code grants everything.

Two further facts shape the answer:

- **The control engine is session-aware**, unlike the clipboard engine. It
  knows which session asked, so a grant can be judged for *that* peer
  rather than over every live peer, as the clipboard grants must be.
- **A denial needs a reason on the wire.** `DenyReason` has only `Busy` and
  `AlreadyControlled`. Reusing either would tell the requesting user
  something false; saying nothing would leave their request to time out
  with no explanation (NFR-3).

## Decision

**The `keyboard` and `mouse` flags on this machine's record of a peer decide
whether that peer may drive this machine's keyboard and pointer.** They are
enforced on the machine being controlled, where the risk is.

1. **Meaning, restated.** `keyboard`: the peer may inject keystrokes here.
   `mouse`: the peer may move and click the pointer here. The stored fields
   are unchanged — no trust-store migration — and both stay set by
   pairing, so a pair nobody has restricted behaves exactly as today.
2. **A request with neither is denied** with a new
   `DenyReason::NotPermitted`, so the requesting side says why instead of
   timing out. A request with at least one is granted as today.
3. **A grant carries what it permits.** While controlled, injected key
   events are dropped unless `keyboard` is granted, and pointer events
   unless `mouse` is. A dropped event is counted, and logged once per
   grant, never per event. A key or button the grant did not permit is
   never pressed, so it can never be left stuck (FR-4.4).
4. **Withdrawing a grant mid-control takes effect at once.** If the peer
   holding the grant loses both flags, control is revoked from this side,
   exactly as the escape chord revokes it (ADR 0009): held input released,
   `ControlRelease` sent. If it loses one, events of that kind stop, and
   anything of that kind still held is released first.
5. **Per session, published by the application.** The application reads
   the grants from the trust store for each live session's peer and
   publishes them to the control driver: before `SessionEstablished`, as
   the clipboard grants are, and again on the trust-store poll. An unknown
   peer, or a store that will not load, grants nothing.
6. **Users set them** with `crossover peers allow-input` / `deny-input
   <device-id> [--keyboard] [--mouse]` — both kinds when neither is named —
   and `crossover peers` shows both for every peer.
7. **The wire change rides the next protocol version.** `NotPermitted` is
   appended to `DenyReason`, which an older peer cannot decode, so under
   [ADR 0017](0017-protocol-version-3.md)'s rule the version and its floor
   move together. [ADR 0016](0016-image-interchange-format.md)'s amendment
   already moves the version for the image-format bits; this lands in the
   same version rather than costing a second lockstep upgrade.

## Alternatives Considered

**Enforce the flags as documented** (outbound: may this machine drive the
peer). The smallest change and no reinterpretation. Rejected because it
defends against nothing: the only actor it constrains is this machine's own
user, and it leaves the inbound direction — the actual risk — as open as
before while letting the documentation claim input permissions exist.

**Add new `accept_keyboard` / `accept_mouse` flags** and keep the old ones
for the outbound direction. Precise, but a trust-store format change with a
frozen decoder for the old format (the `file_receive` precedent), to keep
two flags whose only job is to limit the local user's own choices. The
reinterpretation needs no migration because the old meaning was never
enforced, so nothing that worked before changes.

**One `input` flag instead of two.** Simpler for users. Rejected because
the two flags already exist, and "may point but not type" is a real
arrangement — a presentation machine a colleague may steer but not type
into.

**Deny with `Busy`.** No protocol change. Rejected: it tells the requesting
user the peer is occupied when it has refused them, which is exactly the
misleading diagnostic NFR-3 exists to prevent.

## Consequences

**A trusted peer can no longer take over this machine's input against its
user's wishes**, once they say so. Pairing still grants both, so nothing
changes until a user narrows it.

**The stored flags change meaning without changing shape.** Their doc
comments, SECURITY.md §4, and the README say the new meaning; because the
old meaning was never enforced, no existing behaviour moves.

**The protocol version moves**, together with ADR 0016's image-format bits.
Both machines upgrade in lockstep, as ADR 0017 already requires.

**The control driver gains a per-session policy input**, the first policy
the control engine takes from the application. It is sans-io like the
clipboard engine's grants, so every rule above is a unit test over the
action list, including the mid-control withdrawal paths that must never
leave a key down.

## Implementation note (feature/183)

Built as decided, in protocol version 7 beside ADR 0016's format bits: v7
had not shipped, so the new `DenyReason` needs no second bump. A dev build
of v7 from before this change cannot decode `NotPermitted`, so dev builds
on both machines must be from the same commit, as ADR 0017 already
requires of any pair.

- `ControlEngine` holds an `InputGrant { keyboard, mouse }` per session,
  absent meaning none. The grant is checked first in `on_peer_request`,
  ahead of the busy and single-holder rules.
- `on_peer_batch` applies only the permitted kinds and reports a drop
  once per grant.
- `on_input_grant` gives the grant up when both kinds are withdrawn, and
  when one is withdrawn releases only what that kind holds.
- The application publishes each session's grant to the control driver
  before `SessionEstablished`, and again on every trust-store poll.
- `crossover peers allow-input` / `deny-input [--keyboard] [--mouse]` set
  the flags, and `crossover peers` shows them for every peer.
