# Crossover Security Model

Threat model, trust model, and security design. Vulnerability reporting is in
the root [SECURITY.md](../SECURITY.md). Security is priority #1
([SPECIFICATION.md](SPECIFICATION.md) §2); nothing in any other document
overrides this one.

---

## 1. Security invariants

These hold at all times, in every phase:

1. **Fail closed.** Authentication failures, malformed protocol input,
   unexpected state transitions, and validation failures reject or terminate
   the affected session. There is no partially-trusted or "probably fine"
   state (FR-2.3).
2. **Reachability is not authorization.** Possession of a peer's IP address,
   hostname, and port grants nothing. Unknown identities are rejected before
   any application data is exchanged (FR-2.2).
3. **Mutual authentication.** Both sides cryptographically prove their
   identity on every connection. Encryption without peer authentication is
   insufficient (FR-2.1).
4. **No silent downgrade.** TLS version, protocol version, and feature
   negotiation never fall below either side's configured minimum without
   terminating.
5. **Network input is never trusted**, even post-authentication: all lengths
   validated before allocation, all invariants checked (NFR-1).
6. **Secrets stay put.** Private keys never leave the machine in normal
   operation, never appear in logs, and are stored under OS protection where
   available. Clipboard contents never appear in logs by default (FR-7.4).
7. **Local first.** No cloud dependency, no external telemetry (FR-7.5).
   Cloud infrastructure is never introduced to solve a local problem without
   explicit ADR-level approval.
   **What this does not claim, stated plainly:** once content is installed on
   the destination clipboard, it is subject to that machine's own clipboard
   settings like any other copy. Windows **Clipboard History** (Win+V) and
   **Cloud Clipboard** ("sync across your devices", which sends clipboard
   content to the signed-in Microsoft account) both observe the clipboard, and
   a user who has enabled them has enabled them for everything — including
   items Crossover received from a peer. Crossover introduces neither and does
   not suppress either: they are the user's own OS features acting on the
   user's own clipboard, and silently overriding that choice would break Win+V
   for ordinary synchronized text with no explanation. A user who does not
   want peer content leaving the machine that way turns Cloud Clipboard off in
   Windows Settings → System → Clipboard. **The one exception is the virtual
   file list** (§7): it is excluded from both, because an entry that outlives
   the spool entry or the process behind it is a promise that cannot be kept —
   see F16.
8. **Pairing is not permission.** A paired peer holds exactly the capabilities
   its trust-store record grants, and nothing more. Any capability that writes
   to the local filesystem is **off by default**, requires an explicit per-peer
   grant, and is never implicitly conferred by an existing pairing or by a
   version upgrade (§4, §7).
9. **Received bytes never choose their own destination.** Nothing arriving over
   the wire may determine *where* the local filesystem is written. Peer-supplied
   names are data to be validated, never paths to be followed; received content
   is created inside Crossover's own internal spool and nowhere else, and it
   reaches a user-visible location only when the operating system's paste
   mechanism carries out the user's own gesture (§7).

## 2. Device identity

Each installation generates a persistent identity at first run (FR-1.1):

- device UUID (random)
- human-readable device name
- asymmetric keypair; credential form (raw public key vs. self-signed
  certificate) and its binding into TLS 1.3 mutual authentication are fixed
  by ADR (deferred decision 3)
- creation timestamp

Storage: private key material goes through the `SecureStorage` platform
trait — DPAPI or equivalent on Windows, Keychain/secret-service later.
Identity survives restarts; regeneration is an explicit user action that
invalidates existing pairings.

## 3. Pairing (trust establishment)

Trust is established only by an explicit ceremony on **both** machines
(FR-1.2). Conceptual flow:

```
Computer A:   crossover pair --listen
Computer B:   crossover pair <address-of-A>

Both display a short human-verifiable code, e.g.:

    Pairing fingerprint:  RAVEN-BLUE-47-MANGO

The user verifies the codes match and confirms on both machines.
```

On confirmation, each peer durably records the other's identity; future
connections authenticate automatically; identities not in the trust store
are rejected (FR-1.3).

**MITM requirement:** the ceremony must defeat an active man-in-the-middle
on first contact. The verification mechanism — short authentication string
(SAS) derived from the key exchange vs. a PAKE (e.g., SPAKE2 over a short
code) — is deferred decision 2. Whichever is chosen, the security argument
(what the human comparison/entry actually proves, and the probability an
active attacker survives it) must be written into the ADR.

## 4. Trusted peer store

Each record contains: peer UUID, device name, credential fingerprint,
first-paired and last-connected timestamps, permissions, optional remembered
addresses.

- Permissions model: per-peer flags (`keyboard`, `mouse`, `clipboard_send`,
  `clipboard_receive`, and — with Phase 7 file transfer — `file_receive`).
  Initial implementation may default paired peers to full capability for the
  *input and clipboard* flags, but the data model supports granular permissions
  so they can be enforced later without migration. That default-on latitude
  **stops at the filesystem**: `file_receive` defaults to **off** for every
  peer, existing records included, and only an explicit user grant turns it on
  (invariant 8, §7). A trust store written before file transfer existed reads
  back as `file_receive: false`; no migration may enable it.
- The at-rest blob is versioned, and the version selects the decoder *before*
  anything is decoded. `file_receive` arrived with format version 2; version 1
  keeps a frozen decoder of its own, whose upgrade sets the flag to a literal
  `false`. This is not optional carefulness: the store is postcard-encoded, so
  fields are positional with no names and no defaults, and decoding a version-1
  record against the current record shifts every field after the fourth
  permission byte — the byte that would be read as `file_receive` is the
  remembered-address count. An appended field would therefore either lose the
  store or grant a filesystem write nobody consented to.
- `crossover peers` lists the store, including each peer's file permission;
  `crossover peers remove <device-id>` revokes trust entirely, and
  `crossover peers allow-files <device-id>` / `deny-files <device-id>` grant and
  withdraw `file_receive` (a `show` subcommand can come later). Removal revokes
  authorization immediately (FR-1.4): future connections are rejected (the
  store is re-read on every accept/attempt), and active sessions from that
  identity are terminated within the running process's trust-store poll
  ([ADR 0010](adr/0010-active-session-revocation.md)).
- The store contains **no private keys** — only the local device's public
  metadata and peers' public credentials. Theft of the trust store without
  the private key must not enable impersonation.

## 5. Transport security

- TLS 1.3 via rustls/tokio-rustls ([ARCHITECTURE.md](ARCHITECTURE.md) §7).
- Both peers present credentials; each side verifies the presented
  credential against the *expected* pinned identity from the trust store —
  not against a public CA hierarchy. There are no CAs in this system.
- Unsupported TLS versions, unknown identities, invalid credentials, and
  unsupported protocol versions all terminate with a distinguishable,
  logged reason (actionable diagnostics per FR-7.3, without leaking secret
  material).
- A packet capture must reveal no application payload in plaintext (FR-2.4);
  this is verified by test in Phase 1 ([ROADMAP.md](ROADMAP.md)).

## 6. Threat scenarios

The design must address each of these; tests and reviews trace back to this
table:

| # | Threat | Primary defense |
|---|--------|-----------------|
| T1 | Unknown LAN device connects to Crossover's port | Invariant 2: rejected before application data; bounded resource use pre-auth |
| T2 | Active MITM during first pairing | Pairing ceremony human verification (§3, ADR) |
| T3 | Replay of captured application messages | TLS 1.3 anti-replay; session-scoped message ids |
| T4 | Malformed protocol frames / parser exploitation | NFR-1 bounds + fail-closed parsing + fuzzing ([TESTING.md](TESTING.md)) |
| T5 | Oversized payloads (memory exhaustion) | Length validation before allocation; negotiated maxima |
| T6 | Stale/revoked peer credential reconnecting | Trust store is the authority; revocation is immediate (§4) |
| T7 | Stolen trust store (without private key) | Store contains no secrets usable for impersonation (§4) |
| T8 | Input injection before authorization completes | Input/clipboard handlers unreachable until session `ESTABLISHED` (state machine, [ARCHITECTURE.md](ARCHITECTURE.md) §5.3) |
| T9 | Clipboard exfiltration by unauthorized peer | Same as T8, plus per-peer clipboard permissions (§4) |
| T10 | Secrets leaking via logs/diagnostics | Invariant 6; log-content tests in CI |
| T11 | Compromise of the high-integrity worker yields local admin | Escalation gated behind install (admin) + a trusted-peer session; bounded/validated parsing (NFR-1) and the T8/T9 authorization gates still contain untrusted input; SYSTEM stays unreachable (the service links no network code, ADR 0011). The worker runs high-integrity for admin users so it can drive elevated windows ([ADR 0012](adr/0012-elevated-worker-integrity.md)) |
| T12 | Peer-supplied filename escapes into a path (separator, `..`, drive/UNC prefix, reserved device name) and reaches the shell through the file descriptor | F4 bare-name validation on the receiver, reject-not-repair, **before** the data object is constructed; F3 keeps every create inside the spool, where names are ours and not the peer's (§7) |
| T13 | Received file overwrites, truncates, or replaces an existing user file | F5: exclusive create inside the spool under a locally generated name; Crossover creates nothing where the user's files live, and the paste target's collision handling is the shell's own, answered by the user (§7) |
| T14 | Disk exhaustion via oversized, over-numerous, or under-declared files | F6 size/count/budget caps validated before allocation (NFR-1), F7 aborts and cleans up on breach mid-transfer, F12 bounds the spool in bytes, entries, and age (§7) |
| T15 | Interrupted or torn transfer leaves a partial file that looks complete | F8: temp entry, hash verification, atomic rename, and registration only when whole — and nothing unregistered can ever be pasted (§7) |
| T16 | Paired peer pushes files the user never authorized | F1: `file_receive` off by default and enforced before any allocation or create; pairing is not permission (invariant 8, §4) |
| T17 | Decompression bomb or archive path traversal ("zip slip") via a sent folder | F9: archives are spooled as opaque archive files and never enumerated, decompressed, or extracted — by us or on our behalf (§7) |
| T18 | Received file is launched or handled automatically on arrival | F10: no execution, no handler invocation, no preview, no post-delivery action beyond advertising the item and the diagnostic (§7) |
| T19 | A virtual file list we placed is read back and offered to the peer, looping the largest payload type in the system | F13: own-object ownership check, applied-hash memory, and the rule that spool paths are never a send source (§7, FR-3.3) |
| T20 | Clipboard-object lifetime abuse: rendering after the peer's grant is revoked, or a render callback used to reach content of the caller's choosing, or driven in a loop as a local denial of service | F14: rendering resolves an opaque entry id to a registered, complete spool entry only — no path or name from the caller, index 0 only, one render at a time, no network read. Revocation gates *acceptance*, not already-delivered items (§7 F1, and the rejected alternative recorded below) |
| T21 | Same-user, medium-integrity local process abuses the high-integrity worker's spool: replaces the root with a directory junction to obtain an arbitrary-file delete, or swaps a completed entry's bytes between verification and render | F15: explicit DACL plus High mandatory label (no write-up), root opened and verified as a non-reparse-point directory, every delete performed relative to that handle and never as a path-resolved recursive delete (§7) |
| T22 | Peer-delivered content leaves the machine through the *destination's own* clipboard features — Windows Clipboard History (Win+V) or Cloud Clipboard sync to a Microsoft account | For the file item, F16: excluded from both, so a promise we cannot keep is never retained and file content never reaches an account. For text and images, **not defended and deliberately so** — they are the user's own OS features acting on their own clipboard, disclosed in invariant 7 and the README rather than silently overridden |

Out of scope (documented, not defended): a fully compromised trusted peer
machine — a peer you paired with and that is now malicious can do whatever
its granted permissions allow; per-peer permissions are the containment
mechanism. A peer granted `file_receive` can place content of its choosing in
the spool, and therefore on the user's clipboard, within the caps: containment
is the spool boundary, the caps, and the fact that nothing leaves the spool
without the user's own paste gesture — Crossover does not inspect or scan
received content, and anti-malware on the receiving machine remains the user's.
Physical attackers with local admin on either machine are likewise out of scope.

## 7. Received files (the filesystem-write surface)

Phase 7 file transfer ([ADR 0015](adr/0015-spooled-virtual-file-paste.md),
Accepted) is the first capability that lets a remote peer cause a **write to
the local filesystem**. Every other subsystem consumes network input into
memory and the OS clipboard; this one creates objects on disk that outlive the
session, so it carries its own invariants (F1–F15) on top of §1. They apply to
the receiving side, which is the only side that decides anything.

Two structural facts shape the rest of this section. Bytes land in a **spool**:
a private, Crossover-owned directory the user does not configure and no wire
field influences. They reach a user-visible location only when the operating
system's paste mechanism, executing the user's own gesture, copies them out of
a completed spool entry — Crossover advertises a virtual file list and the
shell performs that write.

**Implemented so far** (feature/126–132): the protected spool and its
handle-only boundary (F15), the `file_receive` grant (F1), the receiving
path from offer to verified entry (F3, F5, F6, F7, F8), the virtual file
list and its render bound (F4's descriptor construction, F10, F13, F14,
F16), and the whole of F12 — byte and entry budgets, the clipboard
lifetime rule, and the age backstop behind it. **Not yet**: the sending
half (`CF_HDROP` observation and folder zipping). `FILE_CLIPBOARD` is
still unadvertised until that lands, so no conforming peer sends a file
yet.

1. **F1 — Consent before bytes.** No file transfer is accepted from a peer
   whose trust-store record does not carry `file_receive` (§4). The check runs
   on the offer, **before** any buffer is allocated, any name is used, and any
   file is created; refusal is fail-closed and logged. Pairing alone never
   grants it, and a grant is revocable like any other permission (§4).
   Revocation gates **acceptance**, not delivery already completed: an item
   already spooled and advertised stays pasteable. Revocation stops the next
   transfer; it does not claw back a finished one. Spool bounds (F12) and the
   startup purge are what bound that window.

   **The alternative was considered and deliberately not taken**, and the reason
   is recorded here rather than left to be inferred (T20). A spool entry is
   *not* equivalent to a file the user chose to receive: it is UUID-named inside
   `%LOCALAPPDATA%`, invisible in any file manager the user would think to open,
   and it was spooled automatically against a standing grant rather than
   accepted item by item. So a peer revoked **for cause** can leave up to
   `MAX_SPOOL_BYTES` of its content pasteable for up to the entry TTL, one
   Ctrl+V away, with nothing the user can practically delete by hand. The
   alternative — *revocation purges that peer's spool entries and relinquishes
   the clipboard if the advertised entry is theirs*, which the in-memory index
   can support by recording the originating peer — is **deferred, not
   rejected**. It is not taken in v1 because revocation would then destroy
   content the user may be mid-paste into a document, making a security action
   silently eat a user's data, and because the same argument would extend to
   every already-applied clipboard item and every file any model already wrote:
   this invariant would be the only place in the system where revocation
   reaches backwards. If the entry TTL is later judged too long a window, this
   is the first control to add.
2. **F2 — Transfer is push-only; there is no peer-initiated read.** The
   protocol carries no message that names a local path to read, enumerate, or
   fetch. A peer can only offer content the *sending* user explicitly copied;
   it can never cause the local machine to disclose a file of the peer's
   choosing. Adding any peer-driven read is a new surface requiring a new ADR.
3. **F3 — Crossover writes only to its own spool.** Every received byte is
   created inside the internal spool directory, whose absolute path is fixed at
   build/configuration level and resolved from the platform's app-data root —
   **never from wire data, and not a user setting**. Received transfers never
   create subdirectories, never write through a symlink, junction, or other
   reparse point that leaves the spool, and never fall back to another location.
   **No received content is placed in user-visible space by Crossover at all:**
   that happens only through the OS paste mechanism carrying out the user's
   gesture, into the target the user chose at that moment.
4. **F4 — Names are data, not paths, and are validated before anything is
   advertised.** The peer-supplied name is accepted only as a **bare filename**:
   no `/` or `\`, no drive or UNC prefix, no `.` or `..` component, no NUL, **no
   character in Unicode general category `Cc` (control) or `Cf` (format)**, no
   trailing dot or space, no Windows reserved device name (`CON`, `PRN`, `AUX`,
   `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`, with or without an extension), and a
   bounded length in **both** encoded bytes and UTF-16 code units — the latter
   because the name is copied into a fixed-size `WCHAR` field in the descriptor.
   `Cf` is named explicitly because a control-characters-only rule misses it:
   `U+202E RIGHT-TO-LEFT OVERRIDE` is `Cf`, not `Cc`, so `invoice<U+202E>gnp.exe`
   would pass and display to the user as `invoiceexe.png`. The category test
   covers the bidi overrides and isolates (`U+202A`–`U+202E`, `U+2066`–`U+2069`,
   `U+200E`/`U+200F`) and anything later added to `Cf`.
   Validation is **reject, not repair** — a name that fails is refused and the
   transfer fails observably; the receiver never rewrites a hostile name into a
   "safe" one it then trusts. Validation completes **before the clipboard data
   object is constructed**, because the name travels onward in the file
   descriptor and is what the shell uses to name the file it creates: a hostile
   name reaches the shell if it is not stopped here. The peer's name never names
   a file on this machine — spool entries are named by a locally generated
   identifier. The receiver enforces its own rules regardless of what the sender
   claims to have checked. **This list is not a completeness claim.** Names that
   mislead without malforming — `report.pdf.exe`, homoglyph spellings, a double
   extension under a shell that hides known types — are valid filenames and are
   accepted knowingly; they are contained downstream by the Internet-zone
   marking (F10's no-execution rule plus the zone stamped on the pasted file,
   which since ADR 0015's 2026-08-17 decision is Local intranet rather than
   Internet — provenance without the execution-warning machinery),
   not by this validator.
5. **F5 — No overwrite, ever, of anything we create.** Spool entries are
   created with an exclusive create (`CREATE_NEW` / `O_EXCL`) under a locally
   generated name, so an existing entry can never be replaced, truncated,
   appended to, or renamed over — including when the existing entry is a symlink
   or reparse point. Because the name is ours and unique, collision is a defect
   condition rather than a routine one, and there is no rename-to-avoid loop.
   Collisions in the **paste target** are not ours to resolve: the shell's own
   collision dialog governs there, answered by the user, on the user's own
   files — user-mediated and deliberately out of our hands. Silent overwrite by
   Crossover is a defect, not a fallback.
6. **F6 — Caps are validated before allocation.** Declared per-file size,
   per-transfer file count, and cumulative per-transfer bytes are each checked
   against local maxima before any allocation or file creation (NFR-1, FR-3.6),
   together with free volume space and the spool budget (F12); a transfer that
   declares more than the local limits, or that could not be retained if it
   arrived, is declined up front rather than started. Buffers are sized from
   validated declarations, never from an unbounded stream.
7. **F7 — A cap breached mid-transfer aborts and cleans up.** Bytes actually
   written are counted against the same caps, so a sender that under-declares
   is cut off at the limit rather than trusted: the receiver stops writing,
   deletes its partial spool artifact, and reports the reason (NFR-3). The
   breach is a protocol violation and is charged as one — which ends the
   session once the peer makes a habit of it, on
   [PROTOCOL.md](PROTOCOL.md) §7's graduated rule, rather than on the
   first frame. That is the same handling every malformed chunk already
   receives, and the reason it is graduated is written there: a single
   violation must not be able to kill a healthy session over an in-flight
   tail. What is *not* graduated is the effect on the transfer itself — it
   is over at the first bad chunk. No partial entry survives, and none is
   ever registered as advertisable.
8. **F8 — Nothing incomplete is ever advertisable.** Payload bytes are written
   to a temporary name inside the spool, flushed, and verified against the
   offered length and hash; only then is the entry renamed atomically and
   registered. Interruption at any point (crash, disconnect, cap breach,
   revocation) leaves either nothing or a temp artifact that is never mistaken
   for a delivered item. Stale temp artifacts are removed on abort and the
   entire spool is purged at startup. This is strictly stronger than a
   final-name-on-disk discipline: incomplete data can never appear in
   user-visible space at all, because the shell only ever copies from an entry
   that is already complete, hash-verified, and registered.
9. **F9 — Archives are stored, never expanded.** A folder arrives as a
   sender-produced archive and is spooled as a single archive **file**. The
   receiver never enumerates, decompresses, or extracts it — nor does it ask
   the shell to — so decompression bombs, archive path traversal ("zip slip"),
   and hostile symlink or hardlink entries are unreachable: the archive is
   opaque bytes bounded only by F6. What the user pastes is the `.zip`. Any
   future auto-extraction is a new filesystem-write surface and requires a new
   ADR plus its own threat entries here.
10. **F10 — No execution, no interpretation, no side effects.** The receiver
    never opens, launches, previews, shell-associates, indexes, or marks
    executable what it spooled, and performs no post-delivery action beyond
    advertising the item on the clipboard and emitting the diagnostic.
    Advertising is not acting on the content: no bytes are rendered until
    something the user drove asks for them. Received content becomes active
    only when the user chooses to act on it.
11. **F11 — Every rejection is observable; content never is.** Permission
    denials, name rejections, cap breaches, spool evictions, and failed renders
    each produce a diagnostic naming the reason (NFR-3, FR-3.6) — never a silent
    drop. Diagnostics may record the rejected name in escaped form; the
    **contents** of a received file are never logged, on the same footing as
    clipboard contents (invariant 6).
12. **F12 — The spool is bounded, and shrinking it is observable.** Retention is
    capped on three axes — total bytes (`MAX_SPOOL_BYTES`), entry count
    (`MAX_SPOOL_ENTRIES`), and entry age (`SPOOL_SWEEP_TTL`, the backstop
    behind the clipboard-lifetime rule) — each a named constant. The byte cap is enforced **by evicting at admission, not by
    testing feasibility at admission and evicting later**: before an offer is
    accepted the receiver evicts oldest-first until the incoming length fits and
    then reserves that length against the in-flight partial, so the cap is the
    true peak on disk rather than a figure exceeded by whatever is still
    arriving. All three caps are re-checked after every completion. Eviction is
    oldest-first and every eviction is logged with the entry and the bound that
    forced it, so an item the user could previously paste never disappears
    silently. The bound is never suspended to keep an advertised item alive. The
    spool is purged in full at startup, so no entry from a previous run
    persists.
13. **F13 — Our own virtual file list never becomes an outbound transfer.**
    Placing the data object raises the local clipboard-change notification like
    any other write, and staging that notification would offer the item straight
    back to its sender — FR-3.3's loop, on the largest payload type in the
    system. Three independent guards prevent it: the platform layer recognizes
    the clipboard's current object as one we placed and reports it as owned
    rather than readable; the engine's applied-hash memory holds the delivered
    item's content hash and suppresses a matching outbound offer; and no path
    inside the spool root may ever be staged as a send source. The first fires
    without reading or rendering anything.
    **Ownership is two conditions, not one**, and the second was added
    because the first was observed answering wrongly: `OleIsCurrentClipboard`
    compares against the object OLE last placed, and it still reported "ours"
    after a plain Win32 `SetClipboardData` in the same process had replaced
    the clipboard contents — OLE learns of ownership changes through its own
    window, and a non-OLE write next door does not always tell it. A stale
    "yes" here is the harmful direction: it would suppress staging a copy the
    user genuinely made. So the check also requires the clipboard sequence
    number to be unchanged since the placement, which any update by anyone
    moves. A race can then only produce a stale *"no"*, which costs one
    redundant read.
14. **F14 — Rendering serves registered spool content and nothing else, one at
    a time.** A delayed-render callback resolves an **opaque entry id** to a
    registered, complete spool entry whose bytes were hash-verified against the
    offer when it was written and have been protected at rest by F15 ever since.
    It takes no path or name from its caller, and the only caller-supplied index
    it honours is file-contents index 0 — any other index, and any medium other
    than the single accepted one per format, is refused with the corresponding
    typed failure rather than served. It never reads outside the spool root; it
    never performs a network read, needs a live session, or consults the peer. A
    render for an evicted entry fails observably rather than falling back to
    anything.
    **Exactly one render runs at a time**, and a request arriving
    while one is in flight is refused, never queued: the callback is reachable
    by any local process, so unbounded concurrency would be an unprivileged
    local denial of service — unbounded large reads and an occupied apartment
    thread. That thread is separate from the clipboard-change listener, so a
    render cannot starve clipboard synchronization for the rest of the machine.
    The wording here is deliberately "verified when written, protected since",
    not "verified at render": re-hashing before a render was considered and
    rejected because the consumer streams the entry *after* any such check, so
    it would be a time-of-check/time-of-use race rather than a guarantee. F15's
    access control is what makes the claim true.
15. **F15 — Deleting is as guarded as creating: the spool root is protected,
    verified, and only ever operated on by handle.** This model deletes on three
    paths — the startup purge, GC eviction, and abort cleanup — and it deletes
    from a process that runs at **high integrity** for administrator users
    ([ADR 0012](adr/0012-elevated-worker-integrity.md)), inside a directory that
    lives in the user's own profile. A same-user, medium-integrity process is
    therefore an in-scope attacker here: it is not the compromised-machine or
    local-admin attacker §6 excludes. Three properties, each independently
    load-bearing:
    - **Explicit security descriptor, asserted on every open and never
      inherited.** The spool root is created with a DACL granting only the
      worker's user and local administrators, **and a mandatory integrity label
      with no-write-up**, so a same-user medium-integrity process can neither
      replace the directory nor alter an entry inside it. This is also what
      makes F14's "protected since written" true: without it, an entry's bytes
      could be swapped after verification and before a render, and the shell
      would write attacker bytes under a name the user trusts. Asserting the
      descriptor **at creation alone would not hold**: the reparse-point check
      below passes for a root a lower-integrity same-user process pre-created
      with a permissive DACL, so the descriptor is re-applied to the verified
      handle on every open, and a root that will not take it is refused rather
      than used unprotected. The label is stamped at the worker's own integrity
      level capped at High, because Windows refuses a label above the caller's
      own: where the worker is not elevated there is no integrity boundary to
      cross and the label is inert, and the DACL still applies.
    - **Opened once and verified, never re-resolved.** The root is opened with
      reparse-point-opening semantics and rejected unless it is a real directory
      and **not** a reparse point. A root that exists and fails the check
      disables file receive for the run with a diagnostic; it is never
      "cleaned up" and recreated, because deleting it is the operation being
      defended.
    - **Every delete is handle-relative.** Purge, eviction, and cleanup
      enumerate and unlink relative to that verified handle — never by
      re-resolving the configured path, and never as a recursive tree delete
      that could follow a directory junction. A junction (a mount point, which
      an unprivileged process can create, unlike a symlink) planted where the
      spool was, deleted through by a high-integrity process, is an
      arbitrary-file-delete elevation of privilege: exactly the confused-deputy
      abuse of the worker that T11 asserts is contained. Nothing outside the
      verified root is ever unlinked, and a directory found *inside* it is
      reported rather than descended into — the sweep has no recursive form to
      misuse.
16. **F16 — The virtual file list is excluded from Clipboard History and
    Cloud Clipboard.** The data object carries the
    `CanIncludeInClipboardHistory` and
    `ExcludeClipboardContentFromMonitorProcessing` formats, and the reason is
    a property of the item rather than a policy about clipboards. A file item
    is a *promise* served by a render callback that only this process, holding
    this spool entry, can answer: the object dies with the worker, and the
    entry is collected when the clipboard moves on. A retained history entry
    would therefore fail on paste, later, with no diagnostic from us because
    the failure happens inside the shell — "no entry" is a better outcome
    than "an entry that breaks". Two consequences follow and are checked
    rather than assumed (ADR 0015): the history service must not render
    `CFSTR_FILECONTENTS` at copy time, which would pull up to
    `MAX_CLIPBOARD_FILE_BYTES` with no paste and no user gesture; and cloud
    sync must not retain the item, which would take peer-delivered file
    content off the machine — the invariant-7 half of the question, and the
    reason this is verified on the supported builds before the paste path
    ships rather than observed afterwards. This exclusion covers the file item
    only; text and images are left to the user's own settings, as invariant 7
    records.

## 8. Security-sensitive code areas

Changes in these areas require heightened review and, where architectural,
an ADR: identity generation, key storage, credential verification, pairing,
trust store persistence/revocation, TLS configuration, protocol parsing and
length validation, input/clipboard authorization gates, received-file name
validation and file-descriptor construction, spool path resolution, its security
descriptor and integrity label, spool retention, eviction, and deletion, the
clipboard data object and its delayed-render callbacks, archive handling, and
logging/diagnostics (for leak risk).

Security review is a standing item in each phase's exit criteria and a
dedicated activity in Phase 6 ([ROADMAP.md](ROADMAP.md)).
