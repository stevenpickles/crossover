# 0017. Protocol version 3: the offer's shape changed, so the floor moves with it

Status: Accepted
Date: 2026-08-17

## Context

[ADR 0015](0015-spooled-virtual-file-paste.md) puts a file descriptor on
`ClipboardOffer`. Implementing it exposed something the ADR did not
anticipate: **the cost is not confined to file transfers.**

postcard encodes an `Option` as a discriminant byte, so
`descriptor: Option<FileDescriptor>` adds a byte to **every offer of every
type** — a text item, an image, a peer that never sends a file, a session
where the file capability was never negotiated. A feature bit cannot gate it,
because the byte is read by the decoder before any capability is consulted;
the decoder does not know what was negotiated and must not have to.

A v2 peer receiving a v3 offer therefore reads a trailing byte, fails the
payload, and by [PROTOCOL.md](../PROTOCOL.md) §7 that is fatal to the
session.

This is a different kind of change from
[ADR 0014](0014-chunked-rich-clipboard-transfer.md)'s, which is why that one
needed no version bump. Chunked clipboard added a *new message type* and an
*appended enum variant*, both reachable only once a feature bit is
negotiated: a peer that predates them never sees them. This change alters a
message that already travels between every pair of peers.

The released v0.1.0 (2026-08-16) speaks protocol 2.

## Decision

**`PROTOCOL_VERSION` and `MIN_SUPPORTED_PROTOCOL_VERSION` both move to 3.**

A v2 peer is refused at `Hello` with a version-range mismatch — a clean,
diagnosable refusal naming both ranges — rather than establishing a session
that dies on the first offer.

The floor tracks the ceiling rather than carrying compatibility code. v2's
offers cannot be decoded by v3 and v3's cannot be decoded by v2, so
supporting both would mean maintaining two decoders for the message that
carries the most attacker-influenced input in the protocol. Peers here are
deployed in lockstep — the project has two machines and one maintainer — so
the compatibility that code would buy is compatibility with nobody.

## Alternatives Considered

**Conditional serialization.** A hand-written `Serialize`/`Deserialize` for
`ClipboardOffer` emitting the descriptor only when the content type is
`File`, keeping v2 bytes byte-identical for text and images. It genuinely
avoids the break, and it was the strongest alternative.

Rejected because it puts non-obvious, hand-rolled decoding on the one path
that parses hostile input. Every other message in this protocol derives its
codec, is fuzzed as a single shape, and is dull on purpose (NFR-1, and the
fuzz targets in `fuzz/`). A decoder whose layout depends on a field it has
already read is precisely the shape that hides length-confusion bugs, and it
would need its own fuzz target *and* differential testing against the derived
form to be trusted. That is a large, permanent complexity cost paid to avoid
one coordinated upgrade of two machines.

**A separate message type for file offers.** Leaves `ClipboardOffer`
untouched and needs no bump. Rejected because it splits one transaction
across two message shapes: [ADR 0005](0005-clipboard-transaction-flow.md)'s
state machine treats an offer as one thing, and the receiver would have to
reconcile two offer types against a single accept/decline path — new
protocol surface, and a second place for the conflict rules to disagree.

**Ceiling 3, floor 2 — negotiate down.** Advertise v3, accept v2 sessions,
and simply never send file offers to a v2 peer. Rejected because it does not
work: the descriptor byte is *structural*, not conditional, so a v3 build's
offers are unreadable by a v2 peer whatever it intends to send. Making it
work requires the conditional serialization above, and inherits its costs.

## Consequences

**v0.1.0 cannot talk to a build carrying this change.** Both peers must be
upgraded together, and a mixed pair does not connect at all. The failure mode
is the good one — a refusal at `Hello` that names the version ranges, before
any session state exists — but it is a hard break and belongs in the release
notes, not only here.

**The soak rig gains a deployment constraint.** Upgrading one machine and not
the other now produces a pair that cannot pair, which looks like a
configuration fault unless you know to check versions. `crossover version`
reports the protocol range for exactly this reason.

**The decoder stays one implementation.** One shape per message, one fuzz
target per parser, no conditional layout to reason about.

**This is the second bump, and both raised the floor** (v1 → v2 in Phase 5 for
the control-transfer layout change). That is now the project's de facto
policy pre-1.0: the protocol is versioned and negotiated, but not
backward-compatible across a layout change, because there are no deployed
peers to be compatible with. A future contributor should not assume otherwise
— and once there *are* peers in the world that cannot be upgraded in lockstep,
this decision needs revisiting rather than repeating.
