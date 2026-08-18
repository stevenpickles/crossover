# 0015. Files and folders spool internally and paste as virtual files

Status: Accepted (design captured 2026-08-11, model revised 2026-08-12,
open forks settled and accepted 2026-08-17)
Date: 2026-08-12

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
  onto **disk**, from a process that per
  [ADR 0012](0012-elevated-worker-integrity.md) runs at **high integrity**. The
  design must be judged as an attack surface first and a feature second.
- **The clipboard model does not fit files directly.** Windows offers files on
  the clipboard as `CF_HDROP` — a list of *paths on the source machine*, which
  are meaningless on the destination. Making Ctrl+V in Explorer produce real
  files requires a virtual file list
  (`CFSTR_FILEDESCRIPTORW` + `CFSTR_FILECONTENTS`) served through an
  `IDataObject` we own, with render callbacks driven by whichever application is
  pasting.
- The transport pieces already exist, in design and now in code: bulk rides the
  Background class with chunks as the preemption unit
  ([ADR 0013](0013-interactive-over-bulk-prioritization.md)), and the chunked
  transfer path (`ClipboardChunk`, `ChunkPlan`/`ChunkReassembly`, feature-bit
  negotiation, typed `ClipboardProvider`) plus the
  `Offer`/`Accept`/`Data`/`Applied` transaction
  ([ADR 0005](0005-clipboard-transaction-flow.md),
  [ADR 0006](0006-clipboard-transmission-triggers.md)) is ADR 0014's,
  implemented. **Nothing here invents new transport.**

**Why this ADR changed model.** The first draft of 0015 (2026-08-11) specified
a *drop folder*: a user-configured directory on the receiver, into which
Crossover wrote the delivered file, telling the user where it landed. A
structured decision review by the project owner rejected that on **user
experience**, not on security: the destination of a paste is the user's *intent
at paste time*, expressed by where they press Ctrl+V, and a configuration key
cannot express it. The drop folder also made the feature appear broken until
configured, and left the user to move every delivery by hand. The review
retained everything the drop-folder draft got right — eager verified transfer,
sender-side zipping, never extracting, reject-not-repair name validation,
default-off per-peer permission — and changed only **what the receiver does
with a completed blob**: it spools internally and offers the result to the OS
paste mechanism, RDP-style. The result is described below; the drop folder is
kept as a documented alternative and as a candidate fallback for platforms
without a virtual-file-paste equivalent.

## Decision (proposed)

**Files transfer eagerly and paste virtually.** A copied file is transferred
in full, verified, and written to a **bounded internal spool** the user does
not configure and Crossover owns; the receiver then places a **virtual file
list** on its own clipboard, served from that spool. The user pastes where they
intend, and **the OS shell — not Crossover — performs the write into the paste
target.** Crossover never writes to a user-visible location. Receiving is
**off by default** and requires an explicit per-peer grant.

The wire protocol and the engine model are **unchanged from the previous
draft**: Offer (id, type, length, hash) → Accept → chunks → hash-verified
completion, over ADR 0014's machinery, gated by `file_receive`. What follows the
verified completion is what this ADR revises.

### Sender side (unchanged from the previous draft)

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
- **A virtual file list is never a send source.** A clipboard whose file list is
  one we placed is not staged for transmission — see *Loop prevention*, below.

### Receiver side: spool, then advertise

- **Permission: a new per-peer flag `file_receive`, default `false`.** It joins
  the existing `PeerPermissions` model in the trust store
  (`keyboard`, `mouse`, `clipboard_send`, `clipboard_receive` — SECURITY.md's
  permissions section) and is **deliberately excluded from
  `PeerPermissions::FULL`**, so pairing does not grant it. Enabling it is an
  explicit user action on the receiving machine: **`crossover peers allow-files
  <device-id>`** (with the matching `deny-files`), a dedicated verb rather than
  a general per-peer permission editor, which stays deferred. This is the first
  flag in that model that is enforced *and* not granted by default; it is also
  the reason the model was built granular from day one.
- **The destination is not configurable, because there is no destination.**
  Bytes land in the **spool**: a private, Crossover-owned directory under the
  existing local app data root (`%LOCALAPPDATA%\Crossover\spool` on Windows;
  ARCHITECTURE.md §8 already puts machine-local state there). The path is fixed
  by the build and the platform's app-data resolution — **never wire-influenced,
  never assembled from peer input**, and not a user setting. There is nothing
  for the user to configure and nothing to fail closed on, which is precisely
  what the drop folder cost.
- **The spool is a protected directory, opened once and used by handle.** Its
  location is inside the *user's* profile, which per ADR 0012 means a
  **medium-integrity process running as the same user can reach a directory the
  high-integrity worker deletes from and reads** — an ordinary unprivileged
  local process, not the compromised-machine attacker §6 puts out of scope.
  Three properties close that, and SECURITY.md carries them as **F15**:
  1. **An explicit security descriptor, asserted on every open — not merely at
     creation:** a DACL granting only the worker's user and the local
     administrators group, **plus a mandatory integrity label with
     no-write-up**, so a medium-integrity process running as the same user can
     neither replace the directory nor modify an entry. Two corrections the
     platform slice (feature/126) forced, both recorded here rather than left
     in the code:
     - *At creation* was not enough. Property 2's check — a real directory,
       not a reparse point — **passes for a root a lower-integrity same-user
       process pre-created** with a permissive DACL and no label, which is the
       cheapest attack available and would satisfy the check while providing
       none of the protection F14's "protected since written" rests on. The
       descriptor is therefore re-asserted on the verified handle at every
       open, and a root whose descriptor cannot be asserted is refused rather
       than used unprotected.
     - The label is stamped at **the worker's own integrity level, capped at
       High**, not hard-coded High. Windows refuses a label above the caller's
       own without `SeRelabelPrivilege`, so a fixed High would make the spool
       unusable for a non-elevated worker — the case this ADR already
       describes as the label being inert while the DACL still applies. Now
       that case is expressed by the level rather than by a failed call.
  2. **Open once, verify, then never re-resolve by path:** the root is opened
     with `FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT` and
     rejected unless it is a real directory and **not** a reparse point. If it
     is missing it is created; if it exists and fails the check, file receive is
     disabled for the run with an actionable diagnostic — never "delete it and
     start over", which is the very operation being defended.
  3. **Every delete is relative to that handle.** The startup purge, GC
     eviction, and abort cleanup all operate on the opened root, enumerating and
     unlinking by handle-relative name, never by re-resolving
     `%LOCALAPPDATA%\...` and never with a recursive delete that would follow a
     directory junction. This is the invariant the previous draft did not need:
     it wrote files but never deleted a *tree*, and a high-integrity recursive
     delete through an attacker-planted junction (which needs no privilege to
     create, unlike a symlink) is an arbitrary-file-delete elevation of
     privilege — precisely the confused-deputy abuse of the worker that T11
     claims is contained.
- **Entries are protected at rest, and the render does not re-hash.** The same
  security descriptor is what keeps a completed `.bin` entry the bytes we
  verified: without it, a same-user process could swap an entry's contents after
  completion-verification and before a render, and the shell would write
  attacker bytes under a name the user trusts. Re-hashing at render time was
  considered and **rejected**: it doubles the I/O of every paste, and it does not
  actually close the window — the shell streams the entry through our `IStream`
  *after* any such check, so hash-then-stream is itself a time-of-check /
  time-of-use race. The access control is the control; the hash is the *wire*
  integrity check, and SECURITY.md F14 is worded to claim exactly that and no
  more.
- **Spool entries are named by us, not by the peer.** A completed transfer is
  stored as `<spool>/<entry-id>.bin`, where `<entry-id>` is a locally generated
  UUID. **The peer-supplied name never becomes a filesystem name on this
  machine** — it is carried alongside the entry as metadata and used in exactly
  one place, the clipboard descriptor. This is strictly stronger than the
  drop-folder draft, where the peer's name (sanitized) named a real file.
- **Name sanitization is still required, and still reject-not-repair.** A
  hostile name reaches the shell through `FILEDESCRIPTORW.cFileName`, which is
  what Explorer uses to name the file it creates in the paste target — so the
  name is validated as network input in `crossover-protocol`, unrepresentable
  past the parser, exactly as `ClipboardData`'s hash and UTF-8 checks already
  are, and re-checked before the descriptor is built. A conforming name is:
  valid UTF-8, 1..=`MAX_FILE_NAME_BYTES` and 1..=`MAX_FILE_NAME_UTF16_UNITS`
  once encoded; no NUL and **no character in Unicode general category `Cc`
  (control) or `Cf` (format)**; none of `/ \ : * ? " < > |`; not `.` or `..` and
  containing no `..` component; no drive-letter or UNC prefix; not a Windows
  reserved device name (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`,
  `LPT1`–`LPT9`, case-insensitive, with or without an extension); no trailing
  space or dot (Windows silently strips them, so they are a name-confusion
  vector). Rejection is a typed decline, never a rename-into-something-safe: we
  do not guess what the sender meant. **Nothing is advertised to the shell until
  the name has passed** — validation precedes `IDataObject` construction, not
  just precedes a write, and the identical check runs at decode and again there.
- **Category `Cf` is called out because `Cc` alone is the classic miss.**
  `U+202E RIGHT-TO-LEFT OVERRIDE` is category `Cf`, not `Cc`, so a
  control-characters-only rule — and Rust's `char::is_control()`, which tests
  `Cc` — passes `invoice\u{202E}gnp.exe`, which Explorer renders as
  `invoiceexe.png`. Rejecting all of `Cf` covers the bidi overrides and
  isolates (`U+202A`–`U+202E`, `U+2066`–`U+2069`, `U+200E`/`U+200F`) plus every
  future addition to the category, at the cost of refusing the occasional
  legitimate name containing a joiner — an acceptable trade under
  reject-not-repair for a rare, convenience-grade transfer.
- **Extension hiding by other means is accepted knowingly, and the list above is
  not a claim of completeness.** `report.pdf.exe`, a Cyrillic homoglyph, or a
  double extension under a shell configured to hide known types are all
  *valid* filenames that no validator can reject without rejecting real ones.
  They are contained downstream, not upstream: the pasted file carries
  Mark-of-the-Web (`CFSTR_ZONEIDENTIFIER`, below), so SmartScreen and the
  execution-warning machinery treat it as untrusted content, and F10 guarantees
  Crossover never launches anything itself. A reader should take the reject-list
  as "the names that would break a *path*, plus the ones that lie about their
  own rendering", not as an anti-malware filter.
- **No partial data is ever spooled under a usable identity.** Chunks stream to
  `<spool>/<entry-id>.part`, created with exclusive-create (`create_new`)
  semantics. The receiver keeps a running byte count and hash; on completion it
  verifies the total equals the declared `content_length` and the hash equals
  the declared `content_hash`, then renames to `<entry-id>.bin` and only then
  registers the entry as advertisable. **Any** other outcome — hash or length
  mismatch, a chunk that would exceed the declared length, out-of-order or
  repeated chunk index, peer disconnect, session teardown, write error, cap
  exceeded — deletes the partial and registers nothing. This is the previous
  draft's F8 discipline, unchanged in substance and moved inside the spool.
- **Memory is O(chunk), not O(file).** Unlike images (ADR 0014), which
  reassemble into a buffer because they must be handed to the OS clipboard, a
  file is written straight through to the spool as chunks arrive. The receiver
  never holds the whole payload.
- **Space and spool budget are settled before accepting**, not discovered
  mid-write. The offer is declined when free space on the spool volume is below
  `content_length + MIN_FREE_SPACE_MARGIN_BYTES`, or when `content_length`
  alone exceeds `MAX_SPOOL_BYTES`. Otherwise the receiver **evicts at admission
  — actually, not hypothetically** — oldest-first until
  `used + content_length ≤ MAX_SPOOL_BYTES`, and counts the in-flight `.part`
  against `used` from the moment it is created, releasing the reservation on
  abort. Testing feasibility at admission but only evicting after completion
  would let a fifth 256 MiB item write its partial alongside a legitimately
  full 1 GiB spool, so the honest peak would have been
  `MAX_SPOOL_BYTES + MAX_CLIPBOARD_FILE_BYTES`. Reserving at admission makes
  `MAX_SPOOL_BYTES` the true ceiling, which is what a bound is for.
- **One file transfer at a time per session** (`MAX_CONCURRENT_FILE_TRANSFERS`
  = 1); a second offer while one is in flight is declined `NotReady`. This
  bounds disk commitment, partial-entry count, and reasoning about cleanup.
- **Then, and only then, the clipboard.** On a verified entry the receiver
  places an `IDataObject` on its own clipboard advertising:
  - `CFSTR_FILEDESCRIPTORW` — one descriptor, carrying the validated name and
    the exact byte length;
  - `CFSTR_FILECONTENTS`, `lindex` 0 — **delayed rendering**: the bytes are
    produced only when something asks, and they are produced by reading the
    spool entry;
  - `CFSTR_ZONEIDENTIFIER` — **Local intranet zone** (see the 2026-08-17
    decision below; this originally read *Internet*), so the file the shell
    creates records where it came from without the execution-warning
    machinery treating it as internet-sourced. This is Windows-specific and
    lives behind a `crossover-platform` trait, no-op where the concept does
    not exist.
- **The accepted `FORMATETC` set is exact, and everything else is refused.**
  `CFSTR_FILECONTENTS` is served **only** as `TYMED_ISTREAM`, and only for
  `lindex == 0`; the descriptor and the zone identifier are served only as
  `TYMED_HGLOBAL`, where they are tiny and fixed-size. A request for
  `CFSTR_FILECONTENTS` as `TYMED_HGLOBAL` returns **`DV_E_TYMED`**, and a
  request for any other `lindex` returns **`DV_E_LINDEX`**. Saying
  "`IStream` in preference to `HGLOBAL`" would have been a guarantee we do not
  hold: a consumer that asks only for `HGLOBAL` would then force the entire
  item — up to 256 MiB — into one global allocation on demand, which is the
  exact cost delayed rendering exists to avoid, and it would be reachable by any
  local process. A bounded `HGLOBAL` fallback under some smaller ceiling was
  considered and rejected as a second code path serving the same bytes for the
  benefit of consumers we have not identified. **The accepted consequence:** a
  paste target that cannot consume `TYMED_ISTREAM` cannot paste the item, and
  says so through the shell's own failure. Explorer and the common Office
  targets take `IStream` for file contents; confirming the set of targets that
  do not is a verify-at-implementation item.
- **Delayed rendering here is a disposal mechanism, not a transfer mechanism.**
  The bytes are already local and already verified when the object is placed. A
  render never touches the network, never needs the session to be alive, never
  waits on the peer, and can never fail for a reason the peer controls. It
  resolves an **opaque entry id** to a spool path — it never takes a path or a
  name from the caller, and the only caller-supplied index it honours is
  `lindex == 0`.
- **A render is bounded in concurrency, because `GetData` is callable by anyone
  local.** Any process on the machine — including a medium-integrity one
  reaching into the high-integrity worker — can call `GetData` on the clipboard
  object in a loop. Unbounded, that is unlimited 256 MiB disk reads and an
  occupied STA thread, driven by an unprivileged local process. So: **one render
  at a time**, and a request arriving while one is in flight is **refused with a
  failure HRESULT, never queued** — queueing would convert a refusal into
  unbounded pending work, which is the same denial with extra steps. Refusing is
  safe because a genuine paste is a single user gesture; a caller that collides
  with itself is not one.
- **Explorer performs the write.** The user presses Ctrl+V (or drags) in
  whatever destination they intend; the shell creates the file there, under the
  descriptor's name, applying its own collision handling, its own permissions,
  and its own overwrite prompt. Crossover contributes bytes and a name and
  nothing else. Where the drop-folder draft's security argument was *"we write,
  but only here"*, this one is *"we do not write anywhere the user can see"*.
- **Delivery is surfaced by the paste affordance itself, plus a log line.** The
  clipboard now holds a pasteable file: that *is* the notification. No toast in
  v1 (Phase 9's tray can revisit it). **Names are logged at debug only;
  contents never** (SECURITY.md invariant 6 — a file name is user data, and a
  file body is clipboard content).

### Clipboard integration: ownership, lifetime, and loop prevention

This is the part the model change actually adds, and it interacts with
machinery that already exists.

- **Placing the object replaces prior clipboard content**, exactly as any
  clipboard write does. A file arriving therefore costs the user whatever was on
  their clipboard — the same cost text and images already impose, and the same
  one-write-per-item citizenship ADR 0014 settled for FR-3.1a. The write is a
  single `OleSetClipboard` at completion; the transfer itself is invisible to
  other applications while it runs.
- **A subsequent local copy revokes our object.** When the user copies anything
  locally, the OS makes that application the clipboard owner and our
  `IDataObject` stops being the clipboard's content; the shell will not ask us
  to render again. This is normal, expected, and not an error.
- **Item lifetime and spool lifetime are deliberately different.** Losing the
  clipboard does **not** delete the spool entry, and pasting does **not**
  consume it: a render is idempotent, so the same item can be pasted into three
  places. The entry lives until the spool GC evicts it (below) or the process
  exits. *Proposed answer to the open question:* **a paste does not consume the
  entry.** Consuming it would make the second Ctrl+V fail for a reason no user
  model predicts, and a "one paste" rule buys no security — the bytes are
  already local and already the user's. **Revoking `file_receive` does not
  delete already-spooled entries** either; it stops the next transfer. That is
  a deliberate call with a real cost, argued in SECURITY.md F1 and recorded
  against the purge-on-revocation alternative in T20.
- **Loop prevention is required and is not automatic.** The receiver's own
  clipboard write raises its own change notification — the `ClipboardProvider`
  contract explicitly guarantees that our own writes notify — and if that
  notification were read back and staged, the receiver would offer the file
  straight back to the sender: FR-3.3's loop, on the largest payload type we
  have. ADR 0014's guard is a **hash memory**: the engine keeps the last
  `APPLIED_HASH_MEMORY` (8) applied content hashes and suppresses an outbound
  offer whose content hash matches one of them
  (`crossover-core/src/clipboard.rs`, `applied_hashes`; counted as
  `clipboard_loop_suppressed`). **That guard does not transfer to this case**,
  and it is important to say so plainly rather than to list it first and imply
  it does: a virtual file list is never read back *as bytes*, so no
  `content_hash` is ever computed for it and no comparison can match. Computing
  one would mean invoking our own delayed render — pulling up to 256 MiB through
  the OS to derive a hash we already know. Three layers, ranked by what actually
  holds:
  1. **Ownership identity (primary, platform layer).** The Windows
     `ClipboardProvider` remembers the `IDataObject` it placed and, on each
     change notification, asks OLE whether the current clipboard object is still
     that one (`OleIsCurrentClipboard`). If it is, the change is our own
     placement: the provider reports it as *owned*, the driver does not stage
     it, and nothing is read or hashed. This is the cheap, exact check, and the
     one that fires in practice. It is also **correctly self-limiting**: once
     any other application copies, the object is no longer the clipboard's, the
     check stops matching, and ordinary local copies resume synchronizing
     exactly as before — this layer suppresses *our* item, not the clipboard.
  2. **No `CF_HDROP`, no send; spool paths never (core layer).** This is the
     layer that actually holds if the ownership check ever misses — a
     replacement object placed by a shell extension, a provider that loses track
     across a restart. An outbound file offer may only be staged from a real
     `CF_HDROP` of on-disk paths; a virtual file list carries no `CF_HDROP` and
     is therefore never staged at all, and a `CF_HDROP` path resolving inside
     the spool root is never staged either.
  3. **Hash memory (residual, engine layer).** The completed entry's
     `content_hash` is still inserted into `applied_hashes` at registration —
     the same call site an applied image uses — but on Windows it is close to
     inert for the reason above. It earns its place for the *next* platform: a
     drop-folder fallback (Linux) or any future path where delivered content is
     re-read as ordinary bytes would put this guard back in the firing line, and
     it costs one insert.
- **What the user copies after pasting is genuinely theirs.** Once Explorer has
  written the file into the user's chosen folder, copying *that* file is an
  ordinary local copy of an ordinary local file, and syncing it back is correct
  behaviour, not a loop. Here the hash memory *does* apply — the copy is read as
  bytes — so the first such copy is usually suppressed as a duplicate of what we
  just applied, which is the same dedup ADR 0014 already provides for a
  re-copied image, and is the desired outcome.
- **Windows Clipboard History and Cloud Clipboard are unresolved, and both touch
  invariants.** Win+V history and cross-device Cloud Clipboard observe clipboard
  content, and neither was contemplated when the threat model was written:
  - *Does history render our formats at copy time?* If the history service
    enumerated and rendered `CFSTR_FILECONTENTS` when the item is placed,
    delayed rendering would be defeated — up to 256 MiB pulled with no paste
    and no user gesture. The likely answer is no (file-promise formats are
    generally not captured), but **likely is not a design input**: this is a
    verify-at-implementation item, and the answer changes whether the "no bytes
    move until a paste" property is real.
  - *Cloud Clipboard versus invariant 7 (local first, no cloud).* Cloud
    Clipboard synchronizes clipboard content to the user's Microsoft account.
    Peer-delivered file content leaving the machine that way is a
    confidentiality event nothing in SECURITY.md contemplates. The design's
    position is that Crossover **sets the opt-out formats** — the
    `CanIncludeInClipboardHistory` and
    `ExcludeClipboardContentFromMonitorProcessing` clipboard formats — on the
    data object, so neither history nor cloud sync retains the item. This is
    stated as intent here and needs confirming on the Windows builds we support.
  - *What the user sees if history snapshots the descriptor but not the
    contents:* a Win+V entry that fails when pasted, with no diagnostic from us,
    because the failure happens in the shell. If the opt-out formats work, the
    entry never appears and the question is moot; if they do not, this is the
    user-visible symptom to expect.

### The spool: bounds, eviction, and lifetime

The spool is a **new bounds surface** — disk the user did not configure, holding
peer-controlled bytes at rest — so it is bounded on three axes, all enforced
before acceptance and again after each completion:

| Constant | Proposed | Bounds |
|---|---|---|
| `MAX_SPOOL_BYTES` | 1 GiB | Total bytes of all spool entries **including any in-flight `.part`**. Exactly four maximum-size (256 MiB) items, and no headroom beyond that by construction — the round number is the point: enough that a normal working session never evicts something the user still wanted, small enough to be an unremarkable footprint in app data. Since files are rare and typically far below the cap, four *maximum-size* items is a floor on how many real ones fit, not a typical count |
| `MAX_SPOOL_ENTRIES` | 16 | Entries retained. Bounds directory scan, GC cost, and index size independently of item size, so a thousand tiny files cannot accumulate |
| *(entry lifetime)* | while it is the clipboard | An entry lives exactly as long as the clipboard still offers it. See "Entry lifetime" below — this replaces the age-based TTL the draft proposed |
| `SPOOL_SWEEP_TTL` | 24 h | **Backstop only.** The age at which an entry is swept even though no clipboard change was ever observed for it — a lost listener, an ownership change we missed. Not the user-visible rule, and not a bound anything should normally reach |

- **Eviction is oldest-completion-first**, applied until all three bounds hold,
  and it is **observable**: every eviction logs the entry id, its age, and the
  bound that forced it (NFR-3). Silent disappearance of something the user could
  previously paste is a diagnostic failure, not a tidy-up.
- **Eviction is not defeated by the clipboard.** If the entry currently
  advertised on the clipboard is the one that must go, it goes; a later render
  then fails observably (the shell reports the copy failed) and we log why.
  Bounds outrank convenience. In practice the advertised entry is the newest and
  eviction is oldest-first, so this is a corner, not the common path.
- **Admission evicts, it does not merely test.** Before Accept, the receiver
  evicts oldest-first until `used + content_length ≤ MAX_SPOOL_BYTES` and then
  reserves `content_length` for the in-flight `.part`; free volume space must
  also exceed `content_length + MIN_FREE_SPACE_MARGIN_BYTES`. A transfer is
  never started that could not be retained, and the reservation is what makes
  `MAX_SPOOL_BYTES` the real on-disk ceiling rather than a post-hoc target.
- **The spool is purged at startup, wholesale.** A virtual file list does not
  survive the process that published it (see the flushing note in
  *Consequences*), so no entry from a previous run is reachable by any paste.
  Rather than reconcile an on-disk index against orphans, the worker deletes
  every file under the spool root at startup — orphaned `.part` files,
  completed `.bin` entries, and anything else that has appeared there. This
  removes the entire orphan-reconciliation surface and makes the spool's
  contents exactly "what this process received".
  **Two things the sweep deliberately does not do** (feature/126): it does not
  recurse, so a *directory* found in the root — a junction included — is
  reported and left, because a recursive delete from the high-integrity worker
  through a planted junction is the exact arbitrary-file-delete this design
  forbids; and it unlinks only names that pass the spool's own strict name
  rule, so a foreign file planted under a name we would never generate is
  reported rather than removed. Both cases mean something else is writing to a
  directory whose descriptor excludes it, which is a warning worth raising and
  not a tidy-up worth performing silently.
- **The spool index is in memory only.** Entry id → validated name, length,
  hash, completion time. Nothing about an entry is persisted or trusted across
  restarts, so there is no on-disk metadata format for a peer to influence.

### Wire protocol: no change beyond the previous draft

**Explicitly: the model change costs nothing on the wire.** Every item below is
carried over verbatim from the drop-folder draft, because the transfer half of
that design was not what the review rejected.

- **`ContentType` gains a file variant.** Variants are appended, never
  renumbered (postcard varint discriminants, ADR 0001); the golden wire
  snapshots and the protocol version rules apply as they do for ADR 0014's image
  type, and the sender-side feature gate of PROTOCOL.md §3.1 applies (an
  unknown enum discriminant is fatal to an older peer, so the gate is
  mandatory).
- **File metadata rides a descriptor, not `ClipboardMeta`.** `ClipboardMeta`
  stays `Copy` and fixed-size — it is the engine's working currency and the
  tests assert that. A variable-length name cannot live there. Instead
  `ClipboardOffer` carries `Option<FileDescriptor>`:

  ```text
  FileDescriptor {
      file_name:     String,   // bare name, sanitized, ≤ MAX_FILE_NAME_BYTES
      archived:      bool,     // true = the blob is a zip built by the sender
      entry_count:   u32,      // entries packed, ≤ MAX_CLIPBOARD_FILE_ENTRIES
      original_bytes: u64,     // uncompressed total, for the user-facing report
  }
  ```

  `ClipboardOffer` consequently loses `Copy`. A file offer without a
  descriptor, a descriptor on a non-file offer, or a descriptor whose
  `archived`/`entry_count` disagree (`entry_count > 1` with `archived == false`)
  is malformed. The name in this descriptor is the one that later becomes
  `FILEDESCRIPTORW.cFileName`; validating it at decode is what keeps a hostile
  name from ever reaching the shell.
- **Files always use the offered flow, at any size**, as chunked types already
  do (ADR 0014 scoped the 64 KiB inline threshold to non-chunked types). The
  offer round is the only place the permission, name, space, and spool-budget
  checks can run *before* the bytes travel, so a file that skipped it would be a
  file that skipped every guard. File content arriving without a preceding
  accepted offer is rejected and counts as a protocol violation.
- **New typed decline reasons** so the sender learns exactly why, and the user
  gets an actionable diagnostic rather than a silent nothing (NFR-3, FR-7.1):
  `NotPermitted` (no `file_receive` grant), `InvalidName`, `InsufficientSpace`
  (volume headroom or spool budget); `TooLarge` and `NotReady` already exist and
  carry their existing meanings.
- **New `ApplyResult` variants:** `Stored` and `StorageFailed`. `Stored` now
  means *the blob is durably spooled, verified, and offered on the receiver's
  clipboard* — the file type's definition of success. FR-3.2 defines success as
  "the destination OS clipboard was updated", which under this model is once
  again literally true for files, so the SPECIFICATION.md adaptation the
  previous draft required shrinks to a clarifying sentence: for files, the
  clipboard is updated with a promise of bytes that are already local.
- **Feature negotiation.** File support is advertised in the `Hello` feature
  flags, so a sender does not spend time building an archive for a peer that
  cannot accept it. The advertisement is an optimization only: **the receiver's
  own permission and budget checks are authoritative**, re-evaluated per
  transaction, and a peer that lies about its flags gains nothing.
- **No hash dedup for files.** `AlreadyHave` is not used for the file type. A
  spool entry may have been evicted, and the user may have already pasted and
  then deleted the result; the receiver cannot honestly claim to "already have"
  it. Re-copying a file re-delivers it.

### Auto-extraction is out, permanently in this design

**A received archive is spooled and advertised as a `.zip` file and Crossover
never extracts it.** This is a security decision, not an ergonomic oversight,
and it remains the single highest-value control in this ADR:

- **Zip-slip is unreachable.** Entry names inside an archive are fully
  attacker-controlled paths — the classic traversal write. We never read an
  entry name, so there is nothing to sanitize and nothing to get wrong.
- **The zip-bomb class dies on the receive side.** There is no decompression,
  so there is no expansion ratio, no nested-archive amplification, and no
  quadratic-blowup case. Bytes in the spool equal bytes on the wire, and those
  are already bounded by `MAX_CLIPBOARD_FILE_BYTES` before a single one is
  accepted.
- **No archive parser touches untrusted input**, in a high-integrity process,
  in any Crossover component. The sender writes archives; nothing reads them.
- Symlink and hardlink entries, permission bits, and alternate stream tricks
  inside the archive are inert — they are just bytes in a file. Explorer copies
  the `.zip`; it does not expand it either.

The cost is one double-click by the user, in their own shell, with their own
tooling and its own protections. That is the correct place for extraction to
happen.

### Bounds

Every quantity below is network-influenced, is a named constant beside the
existing `MAX_*` family, and is validated **before allocation or write**
(NFR-1); every violation returns a typed value, never a panic. **Ratified
as proposed** when the receiving half was implemented (feature/128); the
column is now what the code holds, not what it hoped to.

| Constant | Proposed | Bounds |
|---|---|---|
| `MAX_CLIPBOARD_FILE_BYTES` | 256 MiB | Blob size on the wire and in the spool. Enforced by both peers; the **receiver's** cap governs, and it may be configured downward, never upward |
| `MAX_CLIPBOARD_FILE_ENTRIES` | 256 | Entries packed into one archive |
| `MAX_ARCHIVE_DEPTH` | 32 | Directory recursion depth on the sender |
| `MAX_FILE_NAME_BYTES` | 255 | Encoded length of the name field, validated at decode and again before the descriptor is built. 255 is NTFS's per-component limit, so a name that cannot be a filename anywhere never enters the system |
| `MAX_FILE_NAME_UTF16_UNITS` | 259 | The *character* bound F4 requires, in the units that actually matter downstream: `FILEDESCRIPTORW.cFileName` is `WCHAR[260]`, so 259 units plus the NUL is the exact capacity. Both bounds are checked — 255 UTF-8 bytes can encode at most 255 UTF-16 units (all-ASCII worst case), so the byte bound already implies this one today; it is stated and tested separately so that raising either cap cannot silently overrun a fixed-size Win32 buffer |
| `MAX_CONCURRENT_FILE_TRANSFERS` | 1 | In-flight file transactions per session — structural: the engine holds one `Option`, so a second cannot exist to be counted |
| `MIN_FREE_SPACE_MARGIN_BYTES` | 64 MiB | Headroom required on the spool volume beyond `content_length` |
| `MAX_SPOOL_BYTES` | 1 GiB | Total spool footprint (above) |
| `MAX_SPOOL_ENTRIES` | 16 | Retained spool entries (above) |
| `SPOOL_SWEEP_TTL` | 24 h | Backstop age for entries whose clipboard state was never observed (above) |

`MAX_NAME_COLLISION_ATTEMPTS` from the previous draft is **deleted**: nothing
collides any more. Spool entries are named by locally generated UUID, and
collisions in the *paste target* are Explorer's dialog to show and the user's to
answer.

Additional per-chunk invariants (unchanged): the running byte total is compared
against the declared length on **every** chunk and aborts the moment it would
exceed it — the receiver never trusts the sender to stop; chunk indices must be
strictly sequential, with a repeat or a gap treated as a protocol violation
(fail closed). The frame ceiling is unchanged: files are chunked, so
`MAX_FRAME_BODY_BYTES` does not grow for this feature.

### Threading: an STA is required (flagged, not designed)

An OLE clipboard `IDataObject` must live on a **single-threaded apartment**
thread with a message pump, and its `GetData` render callbacks arrive on that
thread, driven by whichever application is pasting. The Windows platform crate
therefore needs a dedicated STA thread owning the object's lifetime, with the
async engine communicating with it by channel — the object must not be created
on a tokio worker thread. Serving `TYMED_ISTREAM` from the spool file keeps a
large render from blocking the pump for the duration of the copy.

**That thread is separate from the clipboard-listener pump, and the separation
is a requirement, not an implementation preference.** The existing
`AddClipboardFormatListener` window pump is how *all* clipboard synchronization
— text and images included — learns that anything changed. If renders were
serviced on that same thread, any local process could call `GetData` in a loop
and starve clipboard change notifications, so ordinary text and image sync
would stop machine-wide: a remotely visible denial of service driven by an
unprivileged local process. Two threads, plus the one-render-at-a-time bound
above, keep a hostile or merely enthusiastic consumer contained to the thread it
is abusing. This is an
**implementation consequence to be designed in the platform slice**, called out
here because it is the one genuinely new mechanism the model change introduces,
it is `unsafe` COM code, and it is confined to `crossover-platform-windows` by
NFR-4.

## Alternatives Considered

- **A configured drop folder** (the previous draft of this ADR, 2026-08-11):
  the receiver writes the delivered file into a user-configured directory and
  reports the path. Rejected on **user experience**: the destination of a paste
  is the user's intent at the moment they press Ctrl+V, and a configuration key
  cannot express it — every delivery then needs a manual move, and the feature
  looks broken until the key is set. Its security argument was sound and most of
  it survives here in stronger form (we now write nowhere the user can see), so
  it is kept as a **documented alternative** and as the **candidate fallback for
  platforms with no virtual-file-paste equivalent**, Linux in particular.
- **True lazy, RDP-style pull: advertise the file and fetch it from the peer
  only when the render callback fires.** Rejected on three counts, any one
  sufficient: it requires the *session to be alive at paste time*, so a
  clipboard item silently rots when the peer sleeps; it requires the *sender to
  retain and pin* the source file (and to notice if the user deletes or edits
  it) — state we would have to invent and bound; and it needs **new wire
  semantics**, a peer-initiated content pull, which is exactly the
  "no peer-initiated read" property SECURITY.md F2 currently guarantees for
  free. Eager transfer costs bandwidth on a 2.5 GbE LAN that we have already
  decided is not scarce (ADR 0014), and buys a render that cannot fail for a
  reason the peer controls.
- **Auto-extracting received archives.** Rejected on security grounds, as
  argued above: it reintroduces zip-slip, zip bombs, and an archive parser over
  untrusted input inside a high-integrity process, in exchange for saving a
  double-click. Permanently out of this design; any future change is a new
  filesystem-write surface and needs its own ADR.
- **One transaction per selected file** instead of one archive. Rejected: N
  concurrent transactions to bound, partially-delivered-selection semantics to
  define, N attacker-controlled names to sanitize instead of one, and N
  descriptors to keep consistent in one data object.
- **Streaming a tar-like archive generated on the fly**, avoiding the sender's
  temp file. Rejected: the offer round needs an exact length and hash up front,
  which is also what lets the receiver bound its commitment before accepting.
- **Multiple descriptors — advertising a real multi-file list** so a folder
  pastes as a folder. Rejected for v1: it re-introduces per-entry names to
  validate, per-entry contents to serve by index, and a partial-render failure
  mode, which is the file-transfer subsystem this project declined to build. The
  single-`.zip` shape is accepted knowingly.
- **Compressing single files too**, for symmetry. Rejected: ADR 0014's verbatim
  principle — a single file arrives byte-identical, with its own name.
- **Reusing `AlreadyHave` hash dedup for files.** Rejected: a spool entry may
  have been evicted, so "already have" cannot be asserted honestly.
- **Granting `file_receive` as part of `PeerPermissions::FULL`.** Rejected: the
  whole point of a filesystem write surface being opt-in is that pairing is not
  consent to it. Pairing consents to input and clipboard, which is what the
  ceremony's text describes.
- **Rendering the whole item into `HGLOBAL` eagerly** instead of delayed
  rendering with `IStream`. Rejected: it would put up to
  `MAX_CLIPBOARD_FILE_BYTES` into a single global allocation at *placement*
  time, for an item that may never be pasted.

## Consequences

- **What becomes easier:** files behave the way users already expect from RDP —
  copy there, paste here, into whatever folder the paste lands in. Crossover's
  write surface *shrinks* relative to the drop-folder draft: no user-visible
  path is ever constructed, no peer-supplied name ever names a real file, and
  overwrite of a user's file is structurally impossible because we do not
  create files where the user's files live.
- **What becomes harder / worse:** the receive path now owns COM object
  lifetime, an STA thread, and a disk spool with its own GC — real complexity
  the drop folder did not have. A folder still arrives as a `.zip`. And the
  feature's shape is now **Windows-first in a way the drop folder was not**: the
  drop folder was portable `std::fs`, while a virtual file list needs a
  per-platform mechanism.
- **A pasteable item does not survive the process.** An OLE data object's
  delayed-rendered formats are lost when the owning process exits unless it
  flushes them into the clipboard first, and flushing would materialize up to
  256 MiB. So Crossover relinquishes the clipboard on shutdown rather than
  flushing: after a restart, a previously pasted-but-not-yet-consumed item is
  gone. This is why the spool is purged at startup, and it is the one place
  where "no network at paste time" does not extend to "no Crossover at paste
  time".
- **Phase 8 platform questions, noted not designed:** macOS has
  `NSFilePromiseProvider`, which is the direct analog of this model (a promised
  file the receiving app materializes); Linux has no equivalent in the X11 or
  Wayland clipboard models, and the drop folder is the standing fallback
  candidate there. Neither is designed in this ADR.
- **Schema addition in one persisted store:** `PeerPermissions` gains
  `file_receive`, and reading an older store yields `false`. **Implementation
  correction (2026-08-17):** this ADR originally called that addition "additive
  and optional, so existing files keep loading". It is not. The trust store is
  postcard-encoded, so its fields are positional with no names and no defaults;
  appending the flag shifts every field of an existing record after the fourth
  permission byte, and the byte that lands where the new flag is read is the
  length prefix of `remembered_addresses` — `1`, i.e. *granted*, for any peer
  with a remembered address. The realistic outcome is a store that fails to
  load; the unacceptable one is a store that loads with a filesystem-write
  permission the user never gave. So the at-rest **format version moves to 2**,
  the version byte selects the decoder before any decoding happens, and version
  1 keeps a frozen decoder whose upgrade writes `file_receive: false` as a
  literal. The user-visible consequence is unchanged — old stores load, with the
  permission off — but it is a versioned migration, not a free field. **The
  config file gains nothing** — the drop-folder key is not introduced, which is
  one fewer persisted surface than the previous draft.
- **A new sender-only dependency** (a zip *writer*). Nothing in the workspace
  gains an archive *reader*, so the supply-chain and parsing surface is
  write-path only.
- **Platform split holds, but shifts — and F15 moves part of the spool across
  it.** Name validation, retention accounting, bounds, GC policy, and
  torn-transfer cleanup logic stay in core and test on all three OSes. But the
  spool's *protection* is not portable `std::fs`: creating the root with an
  explicit DACL and integrity label, opening it with reparse-point semantics,
  and unlinking relative to that handle are Win32, so the spool grows a small
  platform trait (open-or-create-protected-root, enumerate, unlink-at) whose
  Windows implementation carries the security properties and whose portable
  implementation is honest about providing less. The `IDataObject`, its STA
  thread, the zone marking, and the ownership check likewise live behind
  `crossover-platform` traits in `crossover-platform-windows`.
  `ClipboardProvider` gains a way to place a *promised file list* distinct from
  placing bytes, and a way to report that the current clipboard is our own
  object.
- **`ClipboardOffer` loses `Copy`** (a descriptor is variable-length);
  `ClipboardMeta` keeps it. A small mechanical ripple through the engine.
- **New tests are load-bearing, not incidental:** a name-sanitization corpus
  (traversal, absolute and UNC paths, device names, trailing dot/space, `Cc`
  control *and* `Cf` format characters — `U+202E` explicitly — over-length in
  both bytes and UTF-16 units, non-UTF-8) as part of the malformed-input suite,
  asserting that a failing name produces no descriptor at all; a spool-bounds
  test proving bytes, entries, and the backstop age are all enforced, that
  admission reserves before accepting, and that every eviction is logged; a
  **lifetime test** proving an entry is collected when the clipboard moves on,
  is *not* collected while it is still offered however many times it is
  pasted, and is never collected out from under a render in flight; a fault-injection test
  proving a truncated or aborted transfer leaves no advertisable entry and no
  orphaned `.part`; a **loop test** proving that placing a virtual file list
  produces zero outbound offers; a startup-purge test; a **junction test**
  (Windows) proving that a spool root replaced by a directory junction is
  rejected rather than deleted through; a `FORMATETC` test proving
  `TYMED_HGLOBAL` file contents and `lindex != 0` are refused with the right
  HRESULTs; a concurrency test proving a second in-flight render is refused, not
  queued; and a permissions test proving a peer without `file_receive` moves
  zero bytes.
- **SECURITY.md's §7 invariants and §6 threat rows are updated with this ADR**
  (F1–F15, T12–T21) — the precondition ADR 0014 set for any implementation.
  That document remains the authority on the threats. **F15 is new to the
  F-set's shape, not just its content:** every prior invariant governed
  *creating* and *not overwriting*, because the drop-folder design never deleted
  a tree. This model deletes on three paths — startup purge, GC eviction, abort
  cleanup — from a high-integrity process, so deletion needed an invariant of
  its own.

## Decisions taken (2026-08-17)

The design forks below were settled by the maintainer when files was
scheduled as the next work. The remaining items are verify-at-implementation,
not forks.

### Entry lifetime: while the clipboard still offers it

An entry lives as long as the clipboard holds the item it backs, and is
collected once the clipboard moves on. **Not** an age-based TTL.

This is a better rule than the 24 h the draft proposed, for the reason the
draft itself gave away: a TTL is "the only bound that can delete something
the user was still planning to paste". Tying the lifetime to the clipboard
removes that failure entirely — an entry can only disappear once the thing
it backs is no longer on offer, at which point it could not have been pasted
anyway.

It is also the smaller exposure window. Peer-controlled bytes rest on disk
for exactly as long as they are useful rather than for a fixed period, which
is what the TTL was reaching for and misses in both directions: too long for
an item replaced a minute later, too short for one still wanted tomorrow.

And it composes with the repeatable-paste decision below: within the
clipboard's lifetime an item may be pasted any number of times, and after it
there is nothing to paste.

Three things follow, and are requirements rather than notes:

1. **A sweep at startup.** Entries from a previous run cannot correspond to
   the current clipboard, so they are collected unconditionally on start.
   This is also what makes "an unpasted item does not survive a worker
   restart" true by construction rather than by intention.
2. **A backstop age (`SPOOL_SWEEP_TTL`).** The rule depends on *observing*
   that the clipboard moved on. A lost listener or a missed ownership change
   would otherwise strand an entry forever, so an unobserved entry is swept
   on age as a floor-sweeper — a safety net, not the policy.
3. **A dependency on Clipboard History exclusion.** If Windows retained our
   item in history (Win+V), the clipboard "moving on" would not make it
   unreachable, and collecting the entry would break a history paste. The
   ADR already requires exclusion from history and cloud sync for
   invariant-7 reasons; this decision now *also* depends on it, which raises
   that verify-at-implementation item from ergonomic to load-bearing.

Collection is by handle on the opened spool root, and must not race a render
already in flight — an entry being read is not collected out from under the
reader.

### A paste does not consume the entry

As proposed. A render is idempotent, so an item can be pasted into several
places, which is how a clipboard behaves everywhere else; a
consume-on-paste rule would make the second paste of the same thing fail,
which no user expects. With the lifetime rule above, the entry is collected
when the clipboard moves on rather than lingering.

### `MAX_CLIPBOARD_FILE_BYTES` stays at 256 MiB

As proposed. It covers documents, archives and photo sets — a convenience
feature, not a file-sync product — and refusals are observable (FR-3.6)
rather than silent truncation. The measured cost of a saturated Background
lane (docs/ROADMAP.md, 2026-08-16) argues against raising it: a larger
ceiling buys reach and spends responsiveness.

## Open questions (to settle when scheduled)

- ~~**Spool retention values.**~~ Settled above: entry lifetime follows the
  clipboard, with a 24 h backstop for unobserved entries. `MAX_SPOOL_BYTES`
  (1 GiB) and `MAX_SPOOL_ENTRIES` (16) stand as written — with the lifetime
  rule there is normally one live entry, so both are backstops rather than
  working limits.
- ~~**Whether a paste consumes the spool entry.**~~ Settled above: no.
- **Whether Linux's fallback is the drop folder.** The X11/Wayland clipboard has
  no promised-file mechanism, so the Phase 8 Linux port either revives the drop
  folder for that platform (a per-platform UX divergence, but the design is
  already written and its threat entries mostly still apply) or ships without
  file receive. Not decided here.
- ~~Whether `MAX_CLIPBOARD_FILE_BYTES` (256 MiB) is the right ceiling.~~
  Settled above: it stands.
- Whether `CFSTR_ZONEIDENTIFIER` on the data object actually causes the shell to
  stamp the zone onto the pasted file across the Explorer versions we care
  about, or whether the marking is only reliable on the spool copy. A
  verify-at-implementation item, not a design fork — and one the manual
  paste probe now checks by reading the file's `Zone.Identifier` stream.
- ~~**Clipboard History (Win+V) and Cloud Clipboard.**~~ Settled below: the
  file item is excluded (F16), text and images are disclosed rather than
  suppressed. The behavioural half remains verify-before-ship, per the
  clipboard-integration section: whether the history service renders `CFSTR_FILECONTENTS`
  at copy time (which would defeat delayed rendering), whether
  `CanIncludeInClipboardHistory` and
  `ExcludeClipboardContentFromMonitorProcessing` reliably keep the item out of
  both history and account sync on supported builds, and what the user sees if
  a descriptor is snapshotted without its contents. The cloud-sync half is an
  **invariant-7 question** (local first, no cloud), not merely an ergonomic one,
  so it must be answered before the platform slice ships rather than observed
  afterwards.
- Which paste targets cannot consume `TYMED_ISTREAM` file contents, given that
  the design refuses `TYMED_HGLOBAL` for `CFSTR_FILECONTENTS` outright. If the
  set turns out to include something the maintainer uses, the bounded-fallback
  alternative comes back with a stated ceiling.
- Whether an oversized selection should offer a graceful fallback (e.g. refuse
  with a message naming the cap and the actual size) beyond the plain typed
  refusal — a diagnostics question, not a design one.

## Decisions taken while implementing the receiving half (2026-08-17)

Three things this ADR specified turned out differently in contact with the
code. They are recorded here rather than left as drift.

### A second offer supersedes the transfer in flight; it is not declined

The ADR said a second file offer arriving mid-transfer is declined
`NotReady`. The engine supersedes it instead — the partial is deleted and
the new item admitted — and the reason is a property of the transaction
model this ADR inherited rather than a preference.

A peer holds **one** outbound transaction. A second offer therefore means
it has already abandoned the first at its origin and stopped sending
chunks for it. Declining the new offer would refuse the user's newest copy
in order to keep writing a partial nobody is feeding, until the transfer
deadline expires a minute later. Superseding preserves everything
`MAX_CONCURRENT_FILE_TRANSFERS` was for — one transfer, one partial, a
bounded disk commitment — and is the rule every other inbound item already
follows, so files stop being a special case in the state machine.

### Free space is asked of the volume behind the open handle

Admission needs the volume's free space, and the obvious call
(`GetDiskFreeSpaceExW`) takes a **path**. The spool deliberately has none
to give: the root is a handle so that nothing re-resolves
`%LOCALAPPDATA%\...` (F15). The check therefore goes through
`NtQueryVolumeInformationFile` on the root handle, and reports the
*caller-available* figure so a volume quota is answered honestly. This is
the second place the no-path rule forced the NT layer rather than the Win32
one, after the relative opens.

### The engine decides, the driver touches the disk

The receiving path is split the way clipboard reads and writes already are:
the engine (sans-io) owns permission, budget, sequence and abort
discipline and emits `AdmitFile` / `WriteFileChunk` / `CommitFile` /
`AbortFile` / `EvictSpoolEntry`; the driver performs them and reports back.

That split is what makes this ADR's guarantees unit-testable without a
filesystem — "nothing is answered before the spool has taken the transfer",
"a chunk is judged before it is written", "every outcome but a verified
completion deletes the partial" are all assertions over an action list.
It also produced one rule the ADR did not state: the commit waits for the
*write* of the final chunk to be confirmed, not merely for the hash to
verify, since an entry promoted ahead of its own last bytes would be
registered before it was whole.

### Clipboard History and Cloud Clipboard: exclude the item, disclose the rest

The ADR listed this as one open question. It is two, and they have different
answers (maintainer, 2026-08-17).

**The file item is excluded from both**, and the deciding reason is not the
cloud one the ADR leads with — it is that a history entry for a virtual file
list is *a promise that cannot be kept*. The render callback can only be
answered by this process holding this spool entry, and the object dies with
the worker while the entry is collected when the clipboard moves on. A
retained Win+V entry would therefore fail on paste, later, with no diagnostic
from us. "No entry" beats "an entry that breaks", and that argument holds
whatever the cloud service does. Recorded as SECURITY.md **F16**. The two
behavioural questions the ADR raised are unchanged and still verify-before-ship:
whether history renders `CFSTR_FILECONTENTS` at copy time, and whether the
opt-out formats are honoured on the supported builds.

**Text and images are left alone.** Crossover sets no exclusion formats on any
write today, so a user with Cloud Clipboard enabled has already been syncing
peer-delivered *text* to their Microsoft account — a larger surface than files
and one that predates this ADR. Suppressing it would silently break Win+V for
ordinary synchronized text, which is a real usability loss and a surprising
one, and invariant 7 is a claim about what Crossover introduces rather than
about the user's own OS features. So the gap is closed by naming it — in
invariant 7, in the threat table as T22, and in the README — rather than by
overriding a setting the user chose.

### Two findings from the data object (2026-08-17)

**`OleIsCurrentClipboard` is not sufficient on its own.** It answered
"still ours" after a same-process Win32 `SetClipboardData` had replaced
the clipboard, which would have made the loop guard suppress a copy the
user really made. Ownership is therefore the conjunction of that call and
an unchanged `GetClipboardSequenceNumber` since the placement. Recorded in
SECURITY.md F13, because the guard's wording claimed more than the API
delivers.

**The OLE clipboard hands consumers a mediating object.** Asking the
clipboard's data object for file contents as `TYMED_HGLOBAL` returns
`DV_E_FORMATETC` without our `GetData` running at all: the intermediary
answers out-of-enumeration requests itself. The refusal still happens,
which is what production needs, but the specific codes this ADR
specifies — `DV_E_TYMED`, `DV_E_LINDEX` — are only observable by calling
our object directly, which is how they are tested.

### The zone is intranet, not internet (2026-08-17)

The design stamped `ZoneId=3` and leaned on it: the reject-not-repair
name rules explicitly *do not* try to catch `report.pdf.exe`, a homoglyph,
or a double extension, and argue instead that such names are "contained
downstream" because the pasted file carries Mark-of-the-Web. Zone 3 is
what makes SmartScreen challenge an executable and Office open a document
in Protected View.

Changed to `ZoneId=1`, Local intranet, on a maintainer decision, for two
reasons that point the same way.

**It is the accurate description.** The file came from a paired machine on
the local network. Zone 3 says it came from the internet, which is simply
untrue, and a marking that misdescribes its own provenance is a poor
foundation for anything downstream to reason about.

**The friction is certain and the protection is against an attacker
already out of scope.** Every ordinary document pasted between the user's
two machines pays the Protected View banner and the blocked-file warning.
What zone 3 buys in return is a challenge on content sent by a *paired
peer that has itself been compromised* — which SECURITY.md §6 documents as
out of scope and does not defend against.

**What is lost, stated plainly.** The downstream containment for
extension-hiding names is weaker: the `Zone.Identifier` stream is still
written and still readable, so provenance survives and anything that
inspects zones can act on it, but SmartScreen and Protected View will not
treat a pasted file as untrusted content. F10 is unaffected — Crossover
still never opens, launches, previews, or hands anything to a shell
association — and the containment that remains is the one §6 names:
nothing leaves the spool without the user's own paste gesture, and
anti-malware on the receiving machine is the user's.

Making the zone configurable was considered and rejected for now: it is a
security-relevant knob whose two settings are hard to explain, on a
feature whose whole design brief is "deliberately minimal".

### What is deliberately not here yet

Entry lifetime — collection when the clipboard moves on, and the
`SPOOL_SWEEP_TTL` backstop — needs the clipboard object that makes "the
clipboard still offers it" observable. That object now exists
(`is_current`), so the rule is the next slice's work rather than a
dependency waiting on one. The startup sweep, which needs no such
observation, is already implemented: the spool is purged in full when it
is opened.

The data object is also **not yet wired to the engine**: a completed
transfer spools and registers, and nothing offers it. That is deliberate
— the object is worth landing and testing on its own, and `FILE_CLIPBOARD`
stays unadvertised until the whole path exists, so no conforming peer can
produce an entry that would sit unoffered.

## Decisions taken while implementing the sending half (2026-08-18)

The blob builder — the piece that turns a `CF_HDROP` selection into one
offerable blob — settled five things this ADR either left open or stated
only in passing.

### Archive entries are Stored, never deflated

The ADR says a single file travels verbatim and rejects compressing it
"for symmetry", but never says what happens to entries *inside* an
archive. They are stored uncompressed, for three reasons that agree.

ADR 0014's rule is the LAN is faster than any codec would save, and this
is the same LAN. Compression is the whole reason a zip crate has a
supply-chain footprint at all — the default feature set pulls aes, bzip2,
zopfli, lzma, ppmd, xz and zstd — and none of it is built. And it is what
keeps the byte bound honest: with Stored entries the finished archive is
the walked content plus a small fixed header per entry, so the cumulative
byte check performed **during** the walk genuinely bounds the artifact.
Deflating would make the mid-walk figure an input to a compressor whose
output size is not known until it is written, which turns "refuse before
building something oversized" back into "build it and measure".

The cost is stated plainly: a folder of compressible documents travels at
its full size, and a selection that would have fitted under 256 MiB
compressed can be refused. That is the same trade images already make.

### The blob is a delete-on-close handle, not a path

The ADR says the archive is built "to a temporary file in the sender's own
temp directory". The boundary carries an **open handle and no path**, and
the file is created `FILE_FLAG_DELETE_ON_CLOSE` with `FILE_SHARE_DELETE`.

Cleanup on every refusal path was a requirement, and a `Drop` is the
obvious way to meet it — but a `Drop` does not run when the process dies
mid-build, and this build can be a 256 MiB copy. Delete-on-close makes the
cleanup the operating system's, covering the crash case the ADR did not
have to think about on the receiving side (the spool has a startup purge
for exactly this reason; the sender now needs no equivalent). Not exposing
the path follows the spool's rule for the same reason F15 gives: a caller
that can name the artifact can keep it, re-resolve it, or hand it
somewhere else.

### A single file is copied, not streamed from where it sits

The offer's length and hash are fixed before any byte travels, so the
bytes that travel must be a snapshot. Reading them from the user's own
file instead would leave a window in which editing or deleting it during
a transfer changes what the receiver hash-verifies — a failure the user
could not connect to what they did. One local copy of an item already
bounded at 256 MiB, on a path this ADR describes as rare, is the cheaper
half of that trade.

### Depth is counted in archive path components

`MAX_ARCHIVE_DEPTH` had no counting convention. A selected file or folder
sits at depth 1 and its children at depth 2, so the cap admits a selected
folder plus 31 levels beneath it. The constant now lives beside its
sibling caps in `crossover-protocol` — it has no wire field, because
nothing on the receiving side ever looks inside an archive (F9), but it is
the same family of bound on what one clipboard item may become.

`MAX_CLIPBOARD_FILE_ENTRIES` counts **directories as well as files**,
because they are archive entries: an empty folder is written as one so a
folder's shape survives the round trip, and the count is what bounds the
archive's central directory.

### The name is derived at the platform boundary and judged above it

`validate_file_name` lives in `crossover-protocol` and a platform crate
carries no dependencies, so the builder cannot ask whether the name it
derived from the filesystem conforms. Mirroring the validator would have
meant two rules for the one string of a file transfer that reaches a
shell, which is precisely the shape of bug the reject-not-repair design
exists to avoid.

So the builder reports the name *and where it came from*, and the layer
that can name both crates applies this ADR's two answers: a name the
selection gave itself — a file's own name, or `<folder>.zip` — is refused
when it does not conform, and a name derived from a multi-entry
selection's parent folder falls back to `files.zip`. The fallback is
asserted to be a conforming name in its own test, since it is the one
name with nothing left to fall back to.

Two smaller rules fell out of the walk. A name that is not valid Unicode
is refused rather than lossily converted, for the same reject-not-repair
reason. And two entries that would pack under one name refuse the item
rather than one of them being suffixed: ordinary shell selections come
from a single folder where the filesystem has already made names unique,
so this is a pathological clipboard rather than a case worth
accommodating.

## Decisions taken while implementing the sending transaction (2026-08-18)

The engine half — the piece that takes an observed selection, has it
packed, and runs the ADR 0005 transaction over the result — settled six
more things.

### The engine is told the feature bit, because the send gate is too late

Everywhere else in the system the negotiated feature set is enforced at
`gate_outbound`, the one place every frame passes on its way to the wire
(PROTOCOL.md §3.1), and the engine deliberately knows nothing about it —
`TRANSFER_TIMEOUT`'s doc even records the cost of that: an offer the gate
refuses locally waits out the full deadline, because the engine cannot
know a capability was missing.

Files cannot pay that cost, and not because a minute is long. By the time
a frame reaches the send gate the archive has already been built: a walk
over the selection and a write of up to 256 MiB, spent to learn something
a single bit answered before any of it started. So the engine takes a
`FileSend` policy the way it already takes `FileReceive` — supplied by the
application, refreshed as the negotiated features or the trust store
change, closed by default — and judges it *before* emitting a build. The
send gate still stands behind it; this is a second check in front of an
expensive step, not a replacement for the authoritative one.

The four states are the ADR's own refusal list made legible: no sending
half in this build, the peer never advertised `FILE_CLIPBOARD`, the peer
holds no `clipboard_send` grant, and allowed. A user acts on each
differently (NFR-3), and collapsing them to a boolean would report "not
sent" for all three.

### The spool root travels as a string, for one comparison

Loop-prevention layer 2 — "a `CF_HDROP` path resolving inside the spool
root is never staged" — needs the root's *name*, and F15 built the spool
so that it has none to give: it is a handle precisely so nothing
re-resolves `%LOCALAPPDATA%\...`.

The resolution is that `SpoolStorage` now answers where it is, for
comparison and diagnostics only, and the engine holds that string and
compares path components against it. Nothing opens it, resolves it, or
hands it to an API; every spool operation still goes through the handle,
so the property F15 rests on is untouched. Where there is no spool the
answer is `None`, which is not a hole but the truthful statement that
nothing of ours is on disk for a selection to point at.

Two properties of the comparison are deliberate. It matches components
case-insensitively, because the only filesystem this rule currently
guards does, and a case-sensitive check would miss `...\SPOOL\`. And it
answers *"treat as ours"* for anything it cannot judge without resolving
— a relative path, or one carrying a `..` component — because the safe
direction of a wrong answer here is a copy that does not synchronize,
not a loop on the largest payload type in the system. A shell `CF_HDROP`
produces absolute, normalized paths, so the concession costs nothing
real.

One path inside the spool refuses the **whole** selection rather than
being dropped from it: one clipboard item is one blob, so a partial
selection would send something the user did not select.

### The engine names the chunk; the driver reads it

The receiving side's split — "the engine decides, the driver touches the
disk" — inverts cleanly. An outbound item now carries a *body* that is
either bytes the engine retains (text, images, as before) or a blob the
driver holds open, and `Action::SendFileChunk` names an offset and a
length rather than carrying a payload. The driver reads exactly that
slice when it sends the frame.

That is what makes the sender O(chunk) rather than O(file), mirroring the
receiver's write-through, and it is why a 256 MiB file costs the engine
the same memory a 4 KiB one does. The chunk *plan* is shared with images
unchanged: same `ChunkPlan`, same one-chunk-per-command pacing, so a file
is preempted by live input exactly as ADR 0013 requires without a second
implementation of anything.

Dropping the blob deletes the artifact (delete-on-close, above), so every
exit path emits `ReleaseFileBlob`: delivered, declined, superseded by a
newer local copy, lost to the conflict race, timed out, session lost,
unreadable. The driver's slot is one deep by construction as well, so a
second selection replaces the first even if a release were ever missed.

### A build gets its own deadline scope

`TransferScope` gains `Build`. Two scopes existed because an outbound
offer and an inbound reassembly can be in flight at once and a shared
deadline would let one keep resetting the other's clock; a build is a
third such thing, and a sharper case: it runs *while an unrelated
outbound transfer is still going*, since it does not supersede anything
until it has something to supersede with. Arming it on the outbound clock
would leave that transfer unbounded.

A build that never answers is abandoned on that deadline, and its answer,
if it arrives later, is released rather than offered — the same
supersession rule the transaction slot has always had, one step earlier
in the pipeline.

### `AlreadyHave` is handled on the sending side, though we never send it

This ADR rules out hash dedup for files: a spool entry may have been
evicted, so *our* receiver cannot honestly claim to already have one, and
a test asserts it never does. The sending side still handles the decline,
because a peer's may — the decline path is typed and shared with images,
and `AlreadyHave` is success-shaped there. The outcome is the right one
either way: the transaction closes, the blob is released, and **zero
payload bytes** follow, which is what the offered flow at any size buys.

### The name is judged once more, at the last point before the wire

`wire_file_name` is applied here rather than at the encoder, and the
length is re-checked against `MAX_CLIPBOARD_FILE_BYTES` even though the
builder already bounded it. Both are NFR-1's rule about validating
*before* the wire rather than at it: a name or a length that first failed
inside `encode_payload` would be a refusal with no diagnostic and no
counter, which is exactly the silent drop FR-3.6 forbids.

### What is deliberately not here yet

`FILE_CLIPBOARD` is still unadvertised and the application supplies no
send policy, so the gate is closed and nothing is packed in production.
That is the next slice, and it is a small one: advertise the bit, and
compute `FileSend` from the session's negotiated features and the peer's
`clipboard_send` grant the way `file_receive_policy` already computes its
twin.
