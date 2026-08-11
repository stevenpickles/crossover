# 0014. Chunked rich-clipboard transfer: images first, native format verbatim

Status: Accepted (Phase 7)
Date: 2026-08-11
Amends: [0005](0005-clipboard-transaction-flow.md) (the transaction *shape* —
Offer/Accept → Data → Applied, and the 64 KiB inline threshold — is unchanged;
"no chunking" is the part this supersedes, and the bounds gain a per-type cap)

## Context

[ADR 0005](0005-clipboard-transaction-flow.md) fixed the text transaction flow
and explicitly deferred chunking: *"Chunking gets its own ADR if rich clipboard
types (images, files — Phase 8) demand it."* (Phase 8 as the roadmap then read;
rich clipboard was re-sequenced to Phase 7 on 2026-08-11.) FR-3.7 designed the
data model and protocol to extend past UTF-8 text. This is that ADR.

Maintainer's use case, which shapes the scope:

- **Images all the time** — screenshots and snips, worst case a few MB. This is
  the value.
- **Files rarely, folders rarer.** Not worth a file-transfer subsystem.
- Machines are on a 2.5 GbE LAN, so transfer *time* for a few-MB image is
  single-digit milliseconds — timing is a simplicity/consistency question, not
  a performance one.

## Decision

### Images: an extension of the existing model, not new machinery

- **Add an image type** to the clipboard transaction. The FR-3.7 groundwork is
  already in place: `ClipboardMeta` — shared by `ClipboardOffer` and
  `ClipboardData` — carries a `ContentType` tag whose only variant today is
  `Utf8Text`, so this is a new variant on an existing tag, not new structure.
  Type-specific validation follows it: the UTF-8 check and the
  `MAX_CLIPBOARD_TEXT_BYTES` bound become per-type rules rather than the single
  rule they are now.
- **Native raster format, verbatim — no transcode, no image codec.** Capture
  the clipboard's raster format and ship the bytes as-is; byte-identical by
  construction (matches FR-3.2), and it avoids DIB pixel-format wrangling and a
  codec dependency. Default to `CF_DIB` for paste compatibility (near-universally
  provided and accepted); carry a compressed format instead if that is all the
  source offers.
- **No compression.** The LAN is faster than any codec would save, and it keeps
  the bytes verbatim.
- **Eager chunked sync, consistent with text.** On copy: `ClipboardOffer`
  (id, type, length, hash) → `ClipboardAccept`/`Decline` (decline on
  already-have-this-hash, so re-pasting the same snip moves zero bytes) → the
  content **streamed as bounded chunks** → the receiver reassembles into
  a buffer **sized from the offered length, validated ≤
  `MAX_CLIPBOARD_IMAGE_BYTES` before allocating** (NFR-1) → sets its clipboard.
  Paste is then a normal, instant local paste — the same "sync the clipboard"
  model text already uses.
- **A chunk is its own message type, not a `ClipboardData`.** Today's
  `ClipboardData` validates at decode that the declared `content_length` equals
  the bytes carried and that `content_hash` covers all of them, which a partial
  payload cannot satisfy. Chunks therefore travel as a distinct clipboard
  message, each one bounded and validated on its own (item id, offset/sequence,
  length), and the item's `content_hash` is verified over the **reassembled**
  bytes before the OS clipboard is touched — PROTOCOL.md §5's bounds invariant,
  preserved unchanged. `ClipboardApplied` still closes the transaction, so
  FR-3.2's end-to-end acknowledgement is untouched.
- **Chunk size answers to [ADR 0013](0013-interactive-over-bulk-prioritization.md):**
  chunks are the preemption unit that keeps live input ahead of the transfer, so
  they are sized for the input-latency budget, not just memory.
- **Reuse the engine's rules; generalize its surface.** The transaction state
  machine, hash-dedup, conflict order, bounded retry, and loop prevention carry
  over unchanged as *rules*. Their surface does not: `ClipboardEngine` is
  text-typed throughout — `Action::WriteClipboard { text: String }`,
  `on_local_read(Option<String>)`, `PendingWrite.text`, and a hardcoded
  `ContentType::Utf8Text` on every item it mints — so those become typed bytes.
  That is a mechanical widening, not a redesign.
- **The platform boundary widens with it.** `ClipboardProvider`
  (`crossover-platform/src/clipboard.rs`) exposes only `read_text`/`write_text`
  and reports non-text content as absent; the Windows backend handles
  `CF_UNICODETEXT` alone. The trait gains typed read/write, and all `CF_DIB`
  handling lives behind it in `crossover-platform-windows` — core and protocol
  crates stay platform-free (NFR-4).

Windows **delayed rendering** (true transfer-only-on-paste) is *not* used for
images — it would make Crossover own the far clipboard and service
render callbacks, real complexity that buys nothing for few-MB images on this
LAN. It is reserved for the files case, where payloads are large enough that
"do not move it unless pasted" pays for itself.

### Files/folders: deliberately minimal, and later

Because files are infrequent, do **not** build a file-transfer product or
Explorer-paste fidelity. Sketched direction (to be settled in its own ADR):

- **Drop-folder model, not clipboard fidelity** — files land in a configured
  folder on the far machine.
- **Folders → zip to a single blob** on the source, sidestepping recursive
  directories and virtual files entirely.
- **Guardrails (security is priority #1):** a configured destination, per-peer
  permission (the trust store already models permissions), sanitized names (no
  paths, no `..`, no drive letters), size/count caps, and a first-time accept
  prompt.

This adds a **filesystem-write surface** and so requires its own ADR **and**
SECURITY.md threat-model additions before any implementation — see the
"Known decisions awaiting an ADR" entry.

## Alternatives Considered

- **Transcode images to PNG on the wire** (the size lever). Rejected by the
  maintainer: adds an image codec and DIB pixel-format handling; verbatim
  pass-through is simpler and byte-identical. Size is a non-issue on 2.5 GbE.
- **One big frame instead of chunks** (bump the frame cap). Rejected: a single
  large frame is unpreemptable, which would violate
  [ADR 0013](0013-interactive-over-bulk-prioritization.md). Chunking is required.
- **Generic transport compression (deflate) of the blob.** Considered to tame
  uncompressed DIB size without a codec; rejected as unnecessary on the LAN, and
  it complicates the verbatim story.
- **Transfer-on-paste via delayed rendering for images.** Deferred to files;
  unnecessary complexity for few-MB images on a fast LAN.

## Consequences

- Images — the frequent, high-value case — become a small, self-contained
  increment resting on existing machinery plus [ADR 0013](0013-interactive-over-bulk-prioritization.md).
- Files remain a separate, later, deliberately-bounded capability with its own
  ADR and security review.
- The clipboard protocol gains an image content type and a chunked data path;
  the before-allocation length validation keeps NFR-1 intact, now over a
  reassembly buffer rather than a single frame.
- Specification updates land with the implementation, not after it: PROTOCOL.md
  §5 currently states the offered flow carries "no chunking, per ADR 0005" and
  §8 justifies the 4 MiB + 64 KiB frame body as "one maximum clipboard item per
  frame" — both are superseded here, and the new image cap and chunk size join
  §8's constants table. `MAX_FRAME_BODY_BYTES` itself does **not** grow; chunks
  are far smaller than a frame.

## Open questions (to settle when scheduled)

The decision above is fixed. Where implementation has since settled one of
these, the resolution is noted inline — bookkeeping, not a change of decision.

- The image size ceiling (`MAX_CLIPBOARD_IMAGE_BYTES`) — where to set it.
  **Settled: 64 MiB**, with the arithmetic (a dual-4K uncompressed DIB is
  63.3 MiB) in [PROTOCOL.md](../PROTOCOL.md) §8.
- How image support is gated for interop. Unknown message types are currently
  ignored rather than fatal, so a peer that does not understand chunks would
  simply never answer — a silent stall, which NFR-3 forbids. Both mechanisms
  exist: `supported_features` in `Hello` (PROTOCOL.md §3's stated route for
  future clipboard types) or another hard version-floor bump as v1→v2 was.
  **Settled: the `Hello` feature bit** (`CHUNKED_CLIPBOARD`), sender-gated on
  the negotiated intersection; no version bump, so text keeps synchronizing
  with a peer that lacks the bit. See [PROTOCOL.md](../PROTOCOL.md) §3.1.
- The chunk size ADR 0013 left to this ADR. **Settled: 64 KiB**
  (`MAX_CHUNK_BYTES`), derived from the input-latency budget; arithmetic in
  [PROTOCOL.md](../PROTOCOL.md) §8.
- Clarifying the Amends header above: the 64 KiB inline threshold is
  unchanged as a **text** rule, but it is now type-scoped — chunked types have
  no inline flow and are always offered, at any size
  ([PROTOCOL.md](../PROTOCOL.md) §5).
- Which clipboard formats to capture and restore, and whether to advertise more
  than one format on the far side for maximum paste compatibility. **Still
  open**: it belongs to the platform slice, where `CF_DIB` handling lands.
- Interaction with clipboard citizenship (FR-3.1a) — how long the far side owns
  the clipboard while reassembling. **Settled: not at all.** Reassembly is
  pure accounting in `crossover-protocol` and never touches the OS clipboard;
  the far side takes the machine-global lock exactly once, for the single
  write of the completed, hash-verified item — the same one write a text item
  costs. A transfer is therefore invisible to other applications' copy and
  paste no matter how long it runs, which is the strongest form of FR-3.1a
  citizenship available.
- Not previously listed, and settled by the engine slice: **how long an
  unfinished transfer may hold its buffers.** Session-scoped cleanup alone is
  not a bound — a session can live for days, and an offer accepted by a peer
  that then dies would pin up to `MAX_CLIPBOARD_IMAGE_BYTES` for all of it.
  Every transaction that retains content now carries a deadline
  (`ClipboardConfig::transfer_timeout`, 60 s by default): expiry releases the
  buffers, answers the origin so its transaction closes instead of stalling
  (NFR-3), and leaves the machine ready for the next transfer. The same
  mechanism closes the pre-existing gap where an accepted **text** offer whose
  `ClipboardData` never arrived had no timeout at all.
