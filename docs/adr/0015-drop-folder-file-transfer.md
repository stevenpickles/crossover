# 0015. Files and folders transfer to a drop folder, never to a paste target

Status: Proposed (Phase 7 — design captured 2026-08-11, not yet scheduled)
Date: 2026-08-11

## Context

[ADR 0014](0014-chunked-rich-clipboard-transfer.md) settled the images half of
rich clipboard and deliberately left files unsettled, with a sketched direction
and an explicit gate: files add a **filesystem-write surface**, so they need
their own ADR *and* threat-model additions in [SECURITY.md](../SECURITY.md)
before implementation. `adr/README.md` carries that as the one open "Known
decision awaiting an ADR"; [ROADMAP.md](../ROADMAP.md) carries it as the third
and last piece of Phase 7. This is that ADR.

The constraints that shape it:

- **Files are rare, folders rarer** (the maintainer's stated use case in ADR
  0014). Images are the value; files are a convenience. A file-transfer
  subsystem is not worth building, and every unit of complexity here is spent
  guarding a write surface rather than delivering the feature.
- **Security is priority #1** ([SPECIFICATION.md](../SPECIFICATION.md) §2).
  Everything else in the system moves bytes into memory, a clipboard register,
  or an input queue. This is the first feature that moves peer-controlled bytes
  onto **disk, under a peer-influenced name**, from a process that per
  [ADR 0012](0012-elevated-worker-integrity.md) runs at **high integrity**. The
  design must be judged as an attack surface first and a feature second.
- **The clipboard model does not fit files.** Windows offers files on the
  clipboard as `CF_HDROP` — a list of *paths on the source machine*, which are
  meaningless on the destination. Making Ctrl+V in Explorer produce real files
  requires delayed rendering plus a virtual file list
  (`FILEGROUPDESCRIPTOR`/`FILECONTENTS`) served through an `IDataObject` we
  own, with render callbacks driven by whichever application is pasting. That
  is the file-transfer subsystem we already declined to build, and it hands
  choice of the write target to the pasting application.
- The transport pieces already exist in design: bulk rides the Background
  class with chunks as the preemption unit
  ([ADR 0013](0013-interactive-over-bulk-prioritization.md)), and the chunked
  transfer path plus the `Offer`/`Accept`/`Data`/`Applied` transaction
  ([ADR 0005](0005-clipboard-transaction-flow.md),
  [ADR 0006](0006-clipboard-transmission-triggers.md)) is ADR 0014's. Nothing
  here invents new transport.

## Decision (proposed)

**Copying files sends them; it does not make them pasteable.** A file transfer
delivers bytes into a **configured drop folder** on the receiving machine and
tells the user where they landed. Crossover does not emulate a paste target, does
not write `CF_HDROP` on the destination, and does not attempt Explorer
fidelity — on either side of the wire. Receiving is **off by default** and
requires an explicit per-peer grant *and* an explicitly configured destination.

### Sender side

- **Trigger and transaction are unchanged.** A local `CF_HDROP` observation is
  staged like any other clipboard item and transmitted on the existing triggers
  (ADR 0006: settled-change debounce, control transfer). The transaction is the
  ADR 0005 offered flow; the bytes are chunked per ADR 0014 and ride the
  Background class per ADR 0013.
- **One clipboard item is one blob**, whatever the selection was:
  - a **single file** → its bytes, verbatim, no compression (consistent with
    ADR 0014's verbatim principle);
  - a **single folder**, or **any multi-entry selection** → **one zip archive**,
    built on the sender. A folder therefore arrives as a `.zip`, not as a
    folder tree. Recursive directory walking, per-entry transactions, and
    per-entry name validation all collapse into one bounded blob with one name.
- **The archive is built before the offer**, to a temporary file in the
  sender's own temp directory. The offer must carry an exact `content_length`
  and `content_hash`, and NFR-1 requires the receiver to bound its commitment
  *before* any bytes arrive — both need the finished size, so streaming an
  archive as it is generated is not an option.
- **Only the bare name travels.** The wire carries a sanitized file name, never
  a path: the sender's directory layout is not disclosed to the peer. Naming:
  a single file keeps its own name; a single folder becomes `<folder>.zip`; a
  multi-entry selection becomes `<parent-folder>.zip`, falling back to
  `files.zip` when the parent name is unusable.
- **The sender refuses, observably, before any bytes leave** (FR-3.6 — never
  truncated, never silently dropped), when:
  - the selection exceeds `MAX_CLIPBOARD_FILE_ENTRIES`, or nests deeper than
    `MAX_ARCHIVE_DEPTH`;
  - the total content, or the finished archive, exceeds
    `MAX_CLIPBOARD_FILE_BYTES`;
  - any entry is a **symlink, junction, or other reparse point** — these are
    not followed and their presence refuses the whole transfer rather than
    silently omitting entries. Following them would let a copied shortcut pack
    arbitrary out-of-tree content, or cycle;
  - any entry cannot be read (locked, permission denied, disappeared
    mid-walk) — a partial archive is never sent as if it were the selection;
  - the peer's session did not negotiate file support, or `clipboard_send` is
    not granted for that peer.
- **Zip writing only, never reading.** The sender uses a write-only archive
  path. No component of Crossover parses an archive, on either machine.

### Receiver side

- **Permission: a new per-peer flag `file_receive`, default `false`.** It joins
  the existing `PeerPermissions` model in the trust store
  (`keyboard`, `mouse`, `clipboard_send`, `clipboard_receive` — SECURITY.md's
  permissions section) and is **deliberately excluded from
  `PeerPermissions::FULL`**, so pairing does not grant it. Enabling it is an
  explicit user action on the receiving machine. This is the first flag in that
  model that is enforced *and* not granted by default; it is also the reason
  the model was built granular from day one.
- **Destination: an explicitly configured drop folder**, a new optional key in
  `~/.crossover/config.toml` (ARCHITECTURE.md §8), e.g.
  `[clipboard] drop_folder = "D:\\Inbox\\Crossover"`. There is **no implicit
  default** — not Downloads, not a folder we create. Unset means file receive
  is refused even when the per-peer grant exists. Fail closed (SECURITY.md
  invariant 1): a filesystem write target is a decision the user makes, not one
  we infer.
- **The drop folder is validated once at startup and canonicalized.** It must
  exist, be a directory, be writable, not be the root of a volume, and not be a
  system-sensitive location (`%WINDIR%`, `%ProgramFiles%`, a Startup folder, or
  any directory on `PATH`) — a high-integrity process must not be droppable
  into somewhere that grants execution. The canonical path is cached as **the
  write root**; a config that fails validation disables file receive with an
  actionable diagnostic rather than falling back to anything.
- **Name sanitization: bare names only, enforced at decode.** The name is
  network input, so it is validated in `crossover-protocol` and is
  unrepresentable past the parser, exactly as `ClipboardData`'s hash and UTF-8
  checks already are. A conforming name is: valid UTF-8, 1..=`MAX_FILE_NAME_BYTES`;
  no NUL or control characters; none of `/ \ : * ? " < > |`; not `.` or `..`
  and containing no `..` component; no drive-letter or UNC prefix; not a
  Windows reserved device name (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`,
  `LPT1`–`LPT9`, case-insensitive, with or without an extension); no trailing
  space or dot (Windows silently strips them, so they are a name-confusion
  vector). Rejection is a typed decline, never a rename-into-something-safe:
  we do not guess what the sender meant.
- **Defence in depth on the path.** The final path is the write root joined to
  the sanitized bare name, and the resolved parent directory is verified to be
  the write root before the file is created. Sanitization and containment are
  independent checks; either one alone would be sufficient, and the second one
  also catches a symlink planted *inside* the drop folder.
- **Collisions never overwrite.** Files are created with `create_new`
  semantics — the atomic create *is* the existence check, so there is no
  stat-then-create race. On collision the receiver appends ` (1)`, ` (2)`, …
  up to `MAX_NAME_COLLISION_ATTEMPTS`, then fails the transaction with a typed
  result. No existing file in the drop folder is ever truncated, replaced, or
  appended to.
- **No partial file ever wears the final name.** Chunks stream to a temp file
  in the drop folder named `.crossover-partial-<uuid>.part`, created
  `create_new`. The receiver keeps a running byte count and hash; on completion
  it verifies the total equals the declared `content_length` and the hash
  equals the declared `content_hash`, then renames to the final name (again
  non-overwriting, re-entering the collision loop if needed). **Any** other
  outcome — hash or length mismatch, a chunk that would exceed the declared
  length, out-of-order or repeated chunk index, peer disconnect, session
  teardown, write error, cap exceeded — deletes the partial. A bounded sweep of
  stale `.part` files runs at startup for the case where the process died
  between write and cleanup.
- **Memory is O(chunk), not O(file).** Unlike images (ADR 0014), which
  reassemble into a buffer because they must be handed to the OS clipboard, a
  file is written straight through to disk as chunks arrive. The receiver never
  holds the whole payload.
- **Space is checked before accepting**, not discovered mid-write: the offer is
  declined when free space on the destination volume is below
  `content_length + MIN_FREE_SPACE_MARGIN_BYTES`.
- **One file transfer at a time per session** (`MAX_CONCURRENT_FILE_TRANSFERS`
  = 1); a second offer while one is in flight is declined `NotReady`. This
  bounds disk commitment, partial-file count, and reasoning about cleanup.
- **The received file is never opened, executed, extracted, or placed on the
  clipboard.** On Windows it is marked with a `Zone.Identifier` alternate data
  stream (Internet zone), so SmartScreen, Office Protected View, and the
  execution-warning machinery treat it as the untrusted content it is. That
  marking is platform-specific and therefore lives behind a
  `crossover-platform` trait method, no-op where the concept does not exist.
- Delivery is reported as an actionable diagnostic naming the written path.
  **Names are logged at debug only; contents never** (SECURITY.md invariant 6 —
  a file name is user data, and a file body is clipboard content).

### Auto-extraction is out, permanently in this design

**A received archive is written to disk as a `.zip` file and Crossover never
extracts it.** This is a security decision, not an ergonomic oversight, and it
is the single highest-value control in this ADR:

- **Zip-slip is unreachable.** Entry names inside an archive are fully
  attacker-controlled paths — the classic traversal write. We never read an
  entry name, so there is nothing to sanitize and nothing to get wrong.
- **The zip-bomb class dies on the receive side.** There is no decompression,
  so there is no expansion ratio, no nested-archive amplification, and no
  quadratic-blowup case. Bytes on disk equal bytes on the wire, and those are
  already bounded by `MAX_CLIPBOARD_FILE_BYTES` before a single one is
  accepted.
- **No archive parser touches untrusted input**, in a high-integrity process,
  in any Crossover component. The sender writes archives; nothing reads them.
- Symlink and hardlink entries, permission bits, and alternate stream tricks
  inside the archive are inert — they are just bytes in a file.

The cost is one double-click by the user, in their own shell, with their own
tooling and its own protections. That is the correct place for extraction to
happen.

### Wire protocol

- **`ContentType` gains `File`.** Variants are appended, never renumbered
  (postcard varint discriminants, ADR 0001); the golden wire snapshots and the
  protocol version rules apply as they do for ADR 0014's image type.
- **File metadata rides a descriptor, not `ClipboardMeta`.** `ClipboardMeta`
  stays `Copy` and fixed-size — it is the engine's working currency and the
  tests assert that. A variable-length name cannot live there. Instead
  `ClipboardOffer` (and the chunked transfer's begin message) carries
  `Option<FileDescriptor>`:

  ```text
  FileDescriptor {
      file_name:     String,   // bare name, sanitized, ≤ MAX_FILE_NAME_BYTES
      archived:      bool,     // true = the blob is a zip built by the sender
      entry_count:   u32,      // entries packed, ≤ MAX_CLIPBOARD_FILE_ENTRIES
      original_bytes: u64,     // uncompressed total, for the user-facing report
  }
  ```

  `ClipboardOffer` consequently loses `Copy`. A `File` offer without a
  descriptor, a descriptor on a non-`File` offer, or a descriptor whose
  `archived`/`entry_count` disagree (`entry_count > 1` with `archived == false`)
  is malformed.
- **Files always use the offered flow, at any size.** ADR 0005 routes items
  under `CLIPBOARD_INLINE_MAX_BYTES` inline, and `ClipboardOffer::validate`
  currently rejects an offer below that threshold as malformed. That rule
  becomes type-aware: it applies to non-file types, while `ContentType::File`
  is **always** offered and **never** inline, even for ten bytes. The offer
  round is the only place the permission, name, space, and cap checks can run
  *before* the bytes travel, so a file that skipped it would be a file that
  skipped every guard. A `ClipboardData` of type `File` arriving without a
  preceding accepted offer is rejected and counts as a protocol violation.
- **New typed decline reasons** so the sender learns exactly why, and the user
  gets an actionable diagnostic rather than a silent nothing (NFR-3, FR-7.1):
  `NotPermitted` (no `file_receive` grant, or no drop folder configured),
  `InvalidName`, `InsufficientSpace`; `TooLarge` and `NotReady` already exist
  and carry their existing meanings.
- **New `ApplyResult` variants:** `Stored` (the file is durably in the drop
  folder under the reported name — the file type's definition of success) and
  `StorageFailed`. FR-3.2 defines success as "the destination OS clipboard was
  updated", which is exactly right for text and images and inapplicable to a
  type that deliberately never touches the clipboard; the specification needs
  one sentence recording that adaptation, so that the divergence is deliberate
  rather than silent.
- **Feature negotiation.** File support is advertised in the `Hello` feature
  flags, so a sender does not spend time building an archive for a peer that
  cannot accept it. The advertisement is an optimization only: **the receiver's
  own permission and configuration checks are authoritative**, re-evaluated per
  transaction, and a peer that lies about its flags gains nothing.
- **No hash dedup for files.** `AlreadyHave` is not used for `ContentType::File`.
  Dedup is correct for a clipboard *register*, whose state we own; a drop
  folder is user-mutable state we do not own — the user may have moved,
  renamed, or deleted the earlier delivery. Re-copying a file re-delivers it.

### Bounds

Every quantity below is network-influenced, is a named constant beside the
existing `MAX_*` family, and is validated **before allocation or write**
(NFR-1); every violation returns a typed value, never a panic. Proposed
defaults, to be ratified when scheduled:

| Constant | Proposed | Bounds |
|---|---|---|
| `MAX_CLIPBOARD_FILE_BYTES` | 256 MiB | Blob size on the wire and on disk. Enforced by both peers; the **receiver's** cap governs, and it may be configured downward, never upward |
| `MAX_CLIPBOARD_FILE_ENTRIES` | 256 | Entries packed into one archive |
| `MAX_ARCHIVE_DEPTH` | 32 | Directory recursion depth on the sender |
| `MAX_FILE_NAME_BYTES` | 255 | Name field, validated at decode |
| `MAX_NAME_COLLISION_ATTEMPTS` | 100 | Suffix attempts before failing the transaction |
| `MAX_CONCURRENT_FILE_TRANSFERS` | 1 | In-flight file transactions per session |
| `MIN_FREE_SPACE_MARGIN_BYTES` | 64 MiB | Headroom required beyond `content_length` |

Additional per-chunk invariants: the running byte total is compared against the
declared length on **every** chunk and aborts the moment it would exceed it —
the receiver never trusts the sender to stop; chunk indices must be strictly
sequential, with a repeat or a gap treated as a protocol violation
(fail closed). The frame ceiling is unchanged: files are chunked, so
`MAX_FRAME_BODY_BYTES` does not grow for this feature.

## Alternatives Considered

- **Explorer-paste fidelity via delayed rendering and virtual files**
  (`FILEGROUPDESCRIPTOR`/`FILECONTENTS` served from an `IDataObject`, which
  ADR 0014 floated as the files-only use of delayed rendering). Rejected: it is
  the file-transfer subsystem this project decided not to build — COM object
  lifetime, render callbacks on a foreign thread, owning the far clipboard for
  an unbounded period against FR-3.1a — and, decisively, it hands the *write
  target* to whichever application is pasting. The drop folder is what makes
  the write surface a single, configured, auditable location.
- **Auto-extracting received archives into the drop folder.** Rejected on
  security grounds, as argued above: it reintroduces zip-slip, zip bombs, and
  an archive parser over untrusted input inside a high-integrity process, in
  exchange for saving a double-click.
- **One transaction per selected file** instead of one archive. Rejected: N
  concurrent transactions to bound, partially-delivered-selection semantics to
  define, and N attacker-controlled names to sanitize instead of one. It also
  multiplies the cleanup surface for partial writes.
- **Streaming a tar-like archive generated on the fly**, avoiding the sender's
  temp file. Rejected: the offer round needs an exact length and hash up front,
  which is also what lets the receiver bound its commitment before accepting.
  A temp file on the sender is cheap; an unbounded, unhashed stream is not.
- **A sensible default drop folder** (Downloads, or a folder we create at
  install). Rejected: an implicit filesystem write target is exactly the kind
  of default that turns a granted permission into a surprise. Fail closed.
- **Writing `CF_HDROP` on the receiver** pointing at the delivered file, so
  Ctrl+V in Explorer copies it onward. Rejected for now: it re-enters the
  clipboard sync path we just left (a clipboard change we caused, on a type we
  refuse to echo — FR-3.3 risk for no clear gain) and implies the fidelity this
  ADR declines to promise. Placing the delivered *path as text* on the
  clipboard is a smaller, safer convenience and is left as an open question.
- **Compressing single files too**, for symmetry. Rejected: ADR 0014's verbatim
  principle — a single file arrives byte-identical, with its own name, and the
  LAN does not need the saving.
- **Reusing `AlreadyHave` hash dedup for files.** Rejected, as argued above:
  the receiver cannot claim to "already have" a file it does not control.
- **Granting `file_receive` as part of `PeerPermissions::FULL`.** Rejected: the
  whole point of a filesystem write surface being opt-in is that pairing is not
  consent to it. Pairing consents to input and clipboard, which is what the
  ceremony's text describes.

## Consequences

- **What becomes easier:** files work at all, with a single small
  transaction type on top of machinery ADRs 0013 and 0014 already require. The
  receive path's security argument is unusually short — bare names, one
  canonical write root, atomic non-overwriting creates, no parser, no
  extraction, no execution — which is the whole reason for the shape.
- **What becomes harder / worse for the user:** copy-paste of files into
  Explorer does not work; a folder arrives as a `.zip` that must be extracted
  by hand; nothing arrives at all until the user both grants `file_receive` and
  configures a drop folder. All three are deliberate, and all three are the
  price of a write surface small enough to reason about.
- **Schema additions in two persisted stores:** `PeerPermissions` gains
  `file_receive` (trust store) and the config gains `[clipboard] drop_folder`.
  Both are additive and optional, so existing files keep loading; an older
  binary reading a newer config would reject the unknown key, which is the
  documented behaviour.
- **A new sender-only dependency** (a zip *writer*). Nothing in the workspace
  gains an archive *reader*, so the supply-chain and parsing surface is
  write-path only.
- **Platform split holds:** file writing, sanitization, containment, collision
  handling, and cleanup are `std::fs` and therefore live in core and test on
  all three OSes; only the untrusted-file marking (`Zone.Identifier`) is
  platform-specific and goes behind a `crossover-platform` trait.
- **`ClipboardOffer` loses `Copy`** (a descriptor is variable-length);
  `ClipboardMeta` keeps it. A small mechanical ripple through the engine.
- **New tests are load-bearing, not incidental:** a name-sanitization corpus
  (traversal, absolute and UNC paths, device names, trailing dot/space,
  control characters, over-length, non-UTF-8) as part of the malformed-input
  suite; a containment test proving no write escapes the canonical root; a
  collision test proving no existing file is ever replaced; a fault-injection
  test proving a truncated or aborted transfer leaves no file under the final
  name and no orphaned `.part`; and a permissions test proving a peer without
  `file_receive`, or a receiver without a drop folder, moves zero bytes.
- **SECURITY.md must gain the corresponding threat-model entries** before
  implementation (drafted alongside this ADR): peer-controlled filesystem write
  and path traversal, overwrite of existing user files, disk exhaustion,
  delivery of malicious content into a location the user trusts, and the
  archive-parsing class that this design removes by not parsing archives. This
  ADR is the design; that document remains the authority on the threats.
- **SPECIFICATION.md needs one sentence** adapting FR-3.2's definition of
  success for the file type (durably written to the drop folder, acknowledged
  end to end), so the divergence is recorded rather than silent.

## Open questions (to settle when scheduled)

- The cap values in the bounds table, `MAX_CLIPBOARD_FILE_BYTES` (256 MiB)
  above all — the maintainer's call on what a rare, convenience-grade transfer
  should be allowed to cost.
- Whether the drop folder truly stays mandatory, or whether a default the user
  must confirm once is a better trade than a feature that appears broken until
  configured.
- Whether the receiver should place the delivered file's **path as text** on
  its own clipboard as a convenience (and if so, how that interacts with
  loop prevention and the trigger model).
- How delivery is surfaced to the user before Phase 9 brings a tray: log line
  only, or a platform notification.
- Whether `file_receive` gets a CLI verb of its own (`crossover peers
  allow-files <id>`) or a general per-peer permission editor — the latter is
  the first enforcement of the granular model, so it may be worth doing
  properly once.
- Whether an oversized selection should offer a graceful fallback (e.g. refuse
  with a message naming the cap and the actual size) beyond the plain typed
  refusal — a diagnostics question, not a design one.
- Cross-platform follow-through in Phase 8: the untrusted-file marking
  equivalent on macOS (quarantine attribute) and Linux (nothing), and whether
  a folder-as-`.zip` remains the right shape there.
