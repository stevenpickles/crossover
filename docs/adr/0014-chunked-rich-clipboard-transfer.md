# 0014. Chunked rich-clipboard transfer: images first, native format verbatim

Status: Proposed (Phase 8 — design captured 2026-08-11, not yet scheduled)
Date: 2026-08-11

## Context

[ADR 0005](0005-clipboard-transaction-flow.md) fixed the text transaction flow
and explicitly deferred chunking: *"Chunking gets its own ADR if rich clipboard
types (images, files — Phase 8) demand it."* FR-3.7 designed the data model and
protocol to extend past UTF-8 text. This is that ADR.

Maintainer's use case, which shapes the scope:

- **Images all the time** — screenshots and snips, worst case a few MB. This is
  the value.
- **Files rarely, folders rarer.** Not worth a file-transfer subsystem.
- Machines are on a 2.5 GbE LAN, so transfer *time* for a few-MB image is
  single-digit milliseconds — timing is a simplicity/consistency question, not
  a performance one.

## Decision (proposed)

### Images: an extension of the existing model, not new machinery

- **Add an image type** to the clipboard transaction — the FR-3.7 groundwork: a
  type tag on `ClipboardOffer`/`ClipboardData` so payloads are not assumed
  UTF-8.
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
  already-have-this-hash, so re-pasting the same snip moves zero bytes) →
  `ClipboardData` **streamed as bounded chunks** → the receiver reassembles into
  a buffer **sized from the offered length, validated ≤
  `MAX_CLIPBOARD_IMAGE_BYTES` before allocating** (NFR-1) → sets its clipboard.
  Paste is then a normal, instant local paste — the same "sync the clipboard"
  model text already uses.
- **Chunk size answers to [ADR 0013](0013-interactive-over-bulk-prioritization.md):**
  chunks are the preemption unit that keeps live input ahead of the transfer, so
  they are sized for the input-latency budget, not just memory.
- Reuse the engine's transaction state machine, hash-dedup, bounds, and
  loop-prevention as-is.

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
- The clipboard protocol gains a type tag and a chunked data path; the
  before-allocation length validation keeps NFR-1 intact for the larger frames.

## Open questions (to settle when scheduled)

- The image size ceiling (`MAX_CLIPBOARD_IMAGE_BYTES`) — where to set it.
- Which clipboard formats to capture and restore, and whether to advertise more
  than one format on the far side for maximum paste compatibility.
- Interaction with clipboard citizenship (FR-3.1a) — how long the far side owns
  the clipboard while reassembling.
