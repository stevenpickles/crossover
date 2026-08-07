# Crossover Wire Protocol

The application protocol between authenticated peers. This document is a
skeleton: it fixes the invariants and message semantics; exact encodings are
settled when the serialization format ADR is accepted
([adr/README.md](adr/README.md), deferred decision 1).

The protocol crate (`crossover-protocol`) implements everything here and is
testable without sockets ([ARCHITECTURE.md](ARCHITECTURE.md) §3).

---

## 1. Ground rules

- The protocol runs only inside an established, mutually authenticated
  TLS 1.3 session ([SECURITY.md](SECURITY.md)). Nothing here substitutes for
  transport security.
- **Network input is never trusted**, even from an authenticated peer.
  Every frame and field is validated; malformed input must not panic,
  allocate unboundedly, or corrupt state (NFR-1).
- The protocol never depends on TCP packet boundaries.
- Versioned from the first release; compatibility behavior is deterministic.

## 2. Framing

Conceptual frame layout:

```
frame_length     // validated against MAX_FRAME_SIZE *before* allocation
message_type
message_id       // monotonic per session, for logging & duplicate detection
payload          // serialized message body
```

Requirements:

- A `MAX_FRAME_SIZE` and a maximum size for **every** variable-length field
  are defined as named constants in `crossover-protocol`.
- Frames with invalid lengths, unknown critical structure, or oversized
  fields are rejected; per §7, rejection of malformed framing is fatal to
  the session (fail closed).
- Unknown *message types* within a known-valid frame are skippable when the
  version negotiation permits it (§3), enabling forward compatibility.

## 3. Session establishment and versioning

After TLS establishment and peer authorization, each side sends `Hello`:

```
Hello
    protocol_version        // highest supported
    min_protocol_version    // lowest supported
    device_id
    device_name
    operating_system
    supported_features      // explicit feature flags
```

- The session runs at the highest mutually supported version; if the ranges
  don't intersect, the session terminates with a diagnostic. No silent
  downgrade below either side's minimum.
- Capabilities beyond the base protocol (future clipboard types, multiple
  displays, …) are negotiated via `supported_features`, never assumed.
- Behavior for unknown fields and unknown messages at each version is part
  of the version's definition. Breaking changes require an ADR,
  compatibility tests, and documentation updates.

## 4. Message classes

Four logical classes, initially multiplexed over the single TLS connection:

| Class | Contents | Delivery requirement |
|-------|----------|----------------------|
| CONTROL | Hello, control-transfer negotiation, keepalive, ReleaseAllInput, session management | Ordered, lossless |
| INPUT | Key transitions, pointer motion/buttons/scroll | Keys: ordered, lossless. Pointer motion: coalescable (§6) |
| CLIPBOARD | Clipboard transaction messages | Ordered, lossless, acknowledged |
| TELEMETRY | Latency probes, statistics | Best effort |

The architecture permits moving classes onto separate connections later if
measurement shows head-of-line blocking; the class tag exists from v1 so
that split requires no message redesign.

## 5. Clipboard transactions

Semantics required by FR-3.x: **a sync succeeds only when the destination
OS clipboard was updated.**

Inline flow (content ≤ `CLIPBOARD_INLINE_MAX_BYTES` = 64 KiB — the common
case; ADR 0005):

```
A observes local clipboard change, creates ClipboardItem
A -> B   ClipboardData      { id, origin, sequence, content_type,
                              content_length, content_hash, content }
B        validates length + hash, writes OS clipboard (bounded retries)
B -> A   ClipboardApplied   { id, result }        // success or typed failure
```

Offered flow (larger items, up to `MAX_CLIPBOARD_TEXT_BYTES` = 4 MiB;
oversized items are rejected gracefully — no chunking, per ADR 0005):

```
A -> B   ClipboardOffer     { id, content_type, content_length, content_hash }
B -> A   ClipboardAccept | ClipboardDecline   // decline carries a typed
                                              // reason; already-have-hash
                                              // counts as a sync success
A -> B   ClipboardData
B -> A   ClipboardApplied
```

Invariants (enforced by the core clipboard engine, wire-visible here):

- **Acknowledgement**: every `ClipboardData` receives a `ClipboardApplied`
  (or typed failure) — no fire-and-forget.
- **Loop prevention**: `origin` + `content_hash` let the receiver recognize
  its own subsequent OS clipboard-change notification as the applied remote
  item and suppress re-sending (FR-3.3).
- **Conflict policy**: latest observed item wins, decided by (sequence,
  origin) deterministically; documented and tested (FR-3.5). Logical clocks
  only if a real defect demands them.
- **Bounds**: `content_length` is validated against the negotiated maximum
  before any allocation, and `content_hash` is verified before the OS
  clipboard is touched (FR-3.6).

## 6. Input events

Platform-neutral events (see FR-4.1 for the key-identity model):

```
KeyDown / KeyUp / KeyRepeat      { physical_key, os_key, text?, sequence }
PointerMove                      { x, y, sequence }        // coalescable
PointerButtonDown / Up           { button, sequence }
PointerScroll                    { dx, dy, sequence }
```

- Key transitions and button transitions are ordered and lossless; pointer
  motion is transient — under backpressure, intermediate positions are
  dropped in favor of the newest (Priority: input correctness > latency >
  motion durability).
- `ReleaseAllInput` (CONTROL class) instructs the destination to synthesize
  release events for everything the sender believes is pressed; it is also
  executed locally by the destination on session loss (FR-4.4).
- Sockets carrying INPUT/CONTROL traffic set `TCP_NODELAY`; latency is then
  managed by coalescing, not by Nagle buffering.

## 7. Error handling

- Malformed frame or framing-level violation → terminate session (fail
  closed), log diagnostic with session id and reason.
- Valid frame, unknown non-critical message → skip if negotiation permits,
  count it, log at debug.
- Valid message, semantically impossible state (e.g., `ClipboardApplied`
  for an unknown id, control ack in wrong state) → reject; repeated
  violations terminate the session.
- Every rejection path is reachable in tests via the malformed-input
  suites in [TESTING.md](TESTING.md) (fuzz + protocol tests).

## 8. Constants to be fixed by ADR / early implementation

| Constant | Notes |
|----------|-------|
| Serialization format | postcard (ADR 0001) |
| Default TCP port | 27677 (ADR 0004), `DEFAULT_PORT` in `crossover-protocol` |
| Frame body maximum | 4 MiB + 64 KiB (ADR 0005): one maximum clipboard item plus envelope per frame |
| Max clipboard text / inline threshold | 4 MiB / 64 KiB (ADR 0005), named constants in `crossover-protocol` |
| Keepalive interval / timeout | 5 s / 15 s defaults in `crossover-core::supervision` |
