# 0016. Image interchange: the receiver names the format, the sender produces it

Status: Accepted (2026-09-28, with the amendment below)
Date: 2026-08-16

## Context

[ADR 0014](0014-chunked-rich-clipboard-transfer.md) carries images in **the
source clipboard's own raster format, verbatim** — no transcode, no codec,
no compression. On Windows that format is `CF_DIB`, and between two Windows
machines it works: the bytes the OS gave us are the bytes the peer installs.

The cross-platform risk catalogues broke that assumption from both sides.
[M-5](../platform-risks-macos.md) and [L-9](../platform-risks-linux.md)
independently found that **nothing outside Windows understands `CF_DIB`**:
macOS pasteboards deal in `public.tiff` and `public.png`, X11 advertises
`image/png`. A `ContentType::Image(ImageFormat::Dib)` arriving at a Mac is
unrenderable. So "verbatim" and "cross-platform" cannot both hold, and the
phase that was supposed to add "new implementations of the clipboard trait,
not new protocol design" needs a protocol decision after all.

### What is actually on a clipboard

The question "what happens if I copy a JPEG?" has a surprising answer that
constrains this decision, so it is written down here rather than assumed.

Copying an image *from an application* — a browser's "Copy image", a viewer,
a screenshot tool — does not place the source file's compressed bytes on the
clipboard. The application decodes the image and publishes **pixels**:
`CF_DIB` on Windows, usually alongside a registered `"PNG"` rendering. The
JPEG's own bytes are typically not on the clipboard at all.

So today, copying a JPEG on Windows sends a **DIB** — the decoded,
uncompressed pixels. A 2 MB JPEG becomes tens of megabytes on the wire. The
`ImageFormat::Jpeg` variant exists on the wire but this build's Windows
reader never produces it and its writer refuses it, because Windows has no
registered JPEG clipboard convention worth guessing at.

(Copying a JPEG *file* in a file manager is a different thing entirely — a
file list, which is [ADR 0015](0015-spooled-virtual-file-paste.md)'s
territory, not an image.)

### The property worth protecting

Crossover does not parse image content. The Windows implementation reads a
DIB *header* to compute the blob's logical length — needed because loop
prevention keys on a content hash and a length that drifts would make a
round trip unstable — and treats everything else as opaque bytes.

That is a deliberate security posture, not an accident. Image decoders are a
classic memory-safety attack surface, and a peer's clipboard content is
attacker-influenced by definition: anything that can put bytes on the far
machine's clipboard can hand us an image. Introducing a decoder on the
**receive** path would mean parsing hostile input in the process that also
injects input and holds the device identity.

## Decision

**PNG is the mandatory baseline every implementation must be able to
install. Beyond that, the receiver advertises which formats it can install,
and the sender produces the most preferred one it can — converting from its
own local content. A receiver never decodes what a peer sent it.**

Concretely:

1. **Capability is negotiated, not assumed.** `FeatureFlags` is a `u64` with
   sixty-three bits unused, so image formats are advertised as additional
   bits in the existing `Hello` — no new message, no structural change, and
   a peer that does not understand the bits simply does not set them.
   Advertising a format means "I can install this", in the same
   say-what-you-can-do discipline `CHUNKED_CLIPBOARD` already follows.

2. **PNG is the baseline.** Every implementation that advertises chunked
   clipboard at all must accept `Png`. It is the one format all three
   platforms publish natively, and it is the fallback whenever the two peers
   share nothing better.

3. **The sender converts; the receiver never does.** The sender is
   operating on content its own operating system just handed it — local,
   trusted, and already decoded by the platform. The receiver is operating
   on bytes from the network. Putting every conversion on the sending side
   means untrusted image data is never parsed by Crossover: the receiver
   hands the blob to the OS clipboard verbatim and the *consuming
   application* decodes it, exactly as it would for any other application's
   clipboard content.

4. **Windows to Windows stays exactly as it is.** Both peers advertise
   `Dib`, both prefer it, and the bytes cross verbatim with no conversion —
   today's behaviour and today's cost, unchanged.

5. **JPEG is carried verbatim or not at all.** It is never a conversion
   target and never synthesized. A peer that advertises it may receive it
   from a source clipboard that genuinely holds JPEG bytes; a peer that does
   not advertise it never sees it.

6. **No common format is a decline, not a guess.** If the sender cannot
   produce anything the receiver advertises, the transfer is declined
   observably (FR-3.6) rather than sent in a format the far side will drop
   silently.

## Alternatives Considered

**Keep "verbatim, always" and let cross-platform images fail.** Honest and
free, and it preserves ADR 0014 exactly. Rejected because it makes image
sync a Windows-only feature in a product whose next phase is cross-platform,
and the failure would be silent-ish: the receiver would install bytes no
application can render.

**Convert on the receiver.** The obvious implementation — take whatever
arrives, decode it, publish the platform's native form — and the reason it
is rejected is the whole of the "property worth protecting" section above.
It puts an image decoder on the path that handles hostile input, in the
process that injects input and holds the identity key. The threat model
([SECURITY.md](../SECURITY.md)) treats peer-supplied bytes as untrusted; a
decoder there would be the largest new attack surface since pairing.

**Always convert to PNG on the sender, for every peer.** Simpler than
negotiation: one interchange format, no bits, no preference list. Rejected
because it makes the common case pay for the rare one — two Windows machines
would encode and decode PNG for no reason, spending CPU on every screenshot
to solve a problem neither of them has. It would also *lose* the verbatim
guarantee for the pairing that has it today.

**Negotiate a format per transfer, in the offer.** More flexible than a
handshake bit, and it would let a sender offer what it actually holds.
Rejected as more protocol surface for a decision that does not change during
a session — the peers' capabilities are fixed at connect, so the handshake
is the right place.

## Consequences

**Cross-platform images become possible at all**, which is the point.

**Transfers get smaller, and that is worth more than it looks.** A 4K
screenshot is 31.6 MiB as a DIB and a few MiB as PNG. The input-latency
measurement of 2026-08-16 found the worst input delay is one in-flight bulk
chunk's write time, so *less bulk on the wire is directly less head-of-line
blocking* — a cross-platform transfer will interfere with input less than a
Windows-to-Windows one, which is a pleasant inversion.

**Each platform gains encoders, on local content only.** Windows needs
DIB → PNG (WIC is already available; no new Rust dependency). macOS and
Linux need PNG → DIB if they want to send to a Windows peer, which means
decoding their own local clipboard image — trusted input, using the
platform's own imaging APIs. Preferring platform APIs over a Rust image
crate keeps the dependency graph and the audit surface unchanged.

**ADR 0014's "verbatim" becomes "verbatim where the peers agree".** That is
a real weakening of a stated guarantee and should be read as one. What
survives is the part that mattered: Crossover never re-encodes *received*
content, and never decodes it at all.

**A PNG installed on Windows is invisible to `CF_DIB`-only applications.**
Windows synthesizes nothing from the registered `"PNG"` format, as the
platform module already documents. Under this ADR that case mostly
disappears — a Windows receiver advertises `Dib` and a macOS sender produces
it — but it remains reachable if a sender can only produce PNG. It stays a
documented limitation rather than a reason to decode on receipt.

**The feature bits must be honest.** A build advertising `Png` that cannot
install PNG breaks a peer that trusted it, exactly as a build advertising
`CHUNKED_CLIPBOARD` prematurely would. The existing rule — advertise only
what the code genuinely does — carries over, and the same
advertisement-level test should cover the new bits.

## Amendment (2026-09-28): accepted, with four conditions

Accepted by the maintainer as a Phase 9.0 precondition. The core decision
stands unchanged — the receiver names what it can install, PNG is the
baseline, the sender converts its own local content, and **a receiver never
decodes what a peer sent**. Reviewing it against what was built after it
was drafted found four things it did not say, and each is now part of the
decision.

### 1. A sender produces the receiver's canonical form, or it starts a loop

Loop prevention (FR-3.3) rests on a round trip: a receiver installs an item,
remembers its hash, and when its own write raises a change notification the
read that follows finds the same bytes and suppresses them. On Windows that
only holds for a DIB already in the form the Windows reader returns —
`canonical_dib` trims a blob to its header's own arithmetic precisely so
that a read-back is a fixed point. A DIB produced elsewhere, with a V5
header or trailing bytes, would install, read back under a different hash,
look like a new local copy, and travel back: a clipboard sync loop, which is
release-blocking.

So: **every image format a platform advertises has a canonical form, defined
by that platform's own reader, and a sender producing the format must
produce exactly that form.** Each platform's port carries a conformance
test — produce, install, read back, compare hashes — and a round trip that
is not a fixed point fails the build, not a soak. The receiver still does
nothing but install bytes; the obligation sits with the sender, where the
conversion already is.

### 2. The format bits move the protocol version

The format bits are appended to `FeatureFlags`, and a v6 build advertises
none of them. Rule 6 — no common format is a decline — would then stop
images between a new build and a v6 one, silently from the user's side.
[ADR 0017](0017-protocol-version-3.md)'s rule applies unchanged: peers are
upgraded in lockstep, the floor moves with the ceiling, and a mixed pair is
refused cleanly at `Hello` rather than half-working. The bump lands with the
first build that advertises a format bit, not before.

### 3. Local encoders: platform APIs where there are any, a fenced codec where there are not

"Prefer platform imaging APIs" is right for Windows (WIC) and macOS
(ImageIO / `NSBitmapImageRep`), and has no good answer on Linux: the nearest
thing is GTK's gdk-pixbuf, a heavy dependency for a background service. A
vetted pure-Rust PNG codec is permitted on Linux **for local content only**.
It is fenced from the receive path the way the zip reader is fenced from
production code today (`clippy.toml`'s `disallowed-types`, docs/SECURITY.md
F9): its decode entry points are disallowed everywhere except the one module
that reads this machine's own clipboard, so no path from the network can
reach them without failing the build.

A Windows sender needs no encoder at all in the common case: applications
that publish `CF_DIB` usually publish a registered `"PNG"` rendering beside
it, and when one is present it is sent verbatim instead of re-encoding the
pixels.

### 4. Conversion is off the driver loop, bounded before it starts, and refusable

Encoding a 4K screenshot is hundreds of milliseconds of CPU. Conversion runs
off the clipboard driver's loop, exactly as the file blob builder does
([ADR 0015](0015-spooled-virtual-file-paste.md)), so a slow encode never
stalls inbound verdicts. The source item's size is checked against its
ceiling *before* conversion begins, and the converted result against the
target type's ceiling after. A conversion that fails, or produces nothing
the receiver advertises, is an observable refusal (FR-3.6) — logged,
counted, and never a silent drop.

### What this makes explicit about "never decodes"

The receive path installs bytes and checks their length; it parses nothing.
One piece of image arithmetic *is* reachable by peer-originated bytes, and
has been since images first travelled: after a peer's DIB is installed, the
read-back runs `canonical_dib`'s header arithmetic over it. That is bounded
header arithmetic, conservative by construction (anything it cannot describe
confidently is kept whole), and property-tested — not a decoder. Each port's
canonical-form reader inherits the same standing: bounded, no pixel or
compressed-stream decoding, and tested as reachable input. A fuzz target for
it is owed where the fuzz harness can build the platform crate.
