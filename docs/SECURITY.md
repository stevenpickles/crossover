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
8. **Pairing is not permission.** A paired peer holds exactly the capabilities
   its trust-store record grants, and nothing more. Any capability that writes
   to the local filesystem is **off by default**, requires an explicit per-peer
   grant, and is never implicitly conferred by an existing pairing or by a
   version upgrade (§4, §7).
9. **Received bytes never choose their own destination.** Nothing arriving over
   the wire may determine *where* the local filesystem is written. Peer-supplied
   names are data to be validated, never paths to be followed; received content
   is created inside the configured destination and nowhere else (§7).

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
- `crossover peers` lists the store; `crossover peers remove <device-id>`
  revokes (a `show` subcommand can come later). Removal revokes
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
| T12 | Peer-supplied filename escapes the destination (separator, `..`, drive/UNC prefix, reserved device name) | F4 bare-name validation on the receiver, reject-not-repair; F3 confines every create to the drop folder (§7) |
| T13 | Received file overwrites, truncates, or replaces an existing user file | F5: exclusive create only, collision resolved by receiver-chosen name, never overwrite (§7) |
| T14 | Disk exhaustion via oversized, over-numerous, or under-declared files | F6 size/count/budget caps validated before allocation (NFR-1), F7 aborts and cleans up on breach mid-transfer (§7) |
| T15 | Interrupted or torn transfer leaves a partial file that looks complete | F8: temp file, hash verification, atomic rename into the final name only when whole (§7) |
| T16 | Paired peer pushes files the user never authorized | F1: `file_receive` off by default and enforced before any allocation or create; pairing is not permission (invariant 8, §4) |
| T17 | Decompression bomb or archive path traversal ("zip slip") via a sent folder | F9: archives are written as opaque archive files and never enumerated, decompressed, or extracted (§7) |
| T18 | Received file is launched or handled automatically on arrival | F10: no execution, no handler invocation, no executable bit, no post-write action beyond the diagnostic (§7) |

Out of scope (documented, not defended): a fully compromised trusted peer
machine — a peer you paired with and that is now malicious can do whatever
its granted permissions allow; per-peer permissions are the containment
mechanism. A peer granted `file_receive` can place content of its choosing in
the drop folder within the caps: containment is the folder boundary, the caps,
and the absence of any automatic action on what lands there — Crossover does
not inspect or scan received content, and anti-malware on the receiving machine
remains the user's. Physical attackers with local admin on either machine are
likewise out of scope.

## 7. Received files (the filesystem-write surface)

Phase 7 file transfer ([ADR 0015](adr/0015-drop-folder-file-transfer.md),
Proposed) is the first capability that lets a remote peer cause a **write to
the local filesystem**. Every other subsystem consumes network input into
memory and the OS clipboard; this one creates named objects on disk that
outlive the session, so it carries its own invariants (F1–F11) on top of §1.
They apply to the receiving side, which is the only side that decides anything.

1. **F1 — Consent before bytes.** No file transfer is accepted from a peer
   whose trust-store record does not carry `file_receive` (§4). The check runs
   on the offer, **before** any buffer is allocated, any name is used, and any
   file is created; refusal is fail-closed and logged. Pairing alone never
   grants it, and a grant is revocable like any other permission (§4).
2. **F2 — Transfer is push-only; there is no peer-initiated read.** The
   protocol carries no message that names a local path to read, enumerate, or
   fetch. A peer can only offer content the *sending* user explicitly copied;
   it can never cause the local machine to disclose a file of the peer's
   choosing. Adding any peer-driven read is a new surface requiring a new ADR.
3. **F3 — One destination, resolved locally.** Every received file is created
   inside the user-configured drop folder, whose absolute path is resolved and
   validated once — before the transfer, never from wire data. Received
   transfers never create subdirectories, never write through a symlink,
   junction, or other reparse point that leaves the folder, and never fall back
   to another location: if the configured folder is absent, is not a directory,
   or is not writable, file transfer is disabled for the session and reported.
4. **F4 — Names are data, not paths.** The peer-supplied name is accepted only
   as a **bare filename**: no `/` or `\`, no drive or UNC prefix, no `.` or
   `..` component, no NUL or control characters, no trailing dot or space, no
   Windows reserved device name (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`,
   `LPT1`–`LPT9`, with or without an extension), and a bounded length in both
   characters and encoded bytes. Validation is **reject, not repair** — a name
   that fails is refused and the transfer fails observably; the receiver never
   rewrites a hostile name into a "safe" one it then trusts. The receiver
   enforces its own rules regardless of what the sender claims to have checked.
5. **F5 — No overwrite, ever.** Files are created with an exclusive create
   (`CREATE_NEW` / `O_EXCL`), so an existing name can never be replaced,
   truncated, appended to, or renamed over — including when the existing entry
   is a symlink or reparse point. A collision is resolved by the *receiver*
   choosing a distinct new name within a bounded number of attempts; if it
   cannot, the transfer fails observably. Silent overwrite is a defect, not a
   fallback.
6. **F6 — Caps are validated before allocation.** Declared per-file size,
   per-transfer file count, and cumulative per-transfer bytes are each checked
   against local maxima before any allocation or file creation (NFR-1, FR-3.6),
   and a transfer that declares more than the local limits is declined up front
   rather than started. Buffers are sized from validated declarations, never
   from an unbounded stream.
7. **F7 — A cap breached mid-transfer aborts and cleans up.** Bytes actually
   written are counted against the same caps, so a sender that under-declares
   is cut off at the limit rather than trusted: the receiver stops writing,
   deletes its partial temp artifact, reports the reason (NFR-3), and — because
   exceeding a declared length is a protocol violation — terminates the session
   fail-closed (invariant 1). No partial file survives in the drop folder.
8. **F8 — Nothing incomplete is ever visible under its final name.** Payload
   bytes are written to a temporary name inside the same drop folder, flushed,
   and verified against the offered length and hash; only then is the file
   renamed atomically to its final name. Interruption at any point (crash,
   disconnect, cap breach, revocation) leaves either nothing or a temp artifact
   that is never mistaken for a delivered file. Stale temp artifacts are
   removed on abort and at session start.
9. **F9 — Archives are written, never expanded.** A folder arrives as a
   sender-produced archive and is written as a single archive **file**. The
   receiver never enumerates, decompresses, or extracts it, so decompression
   bombs, archive path traversal ("zip slip"), and hostile symlink or hardlink
   entries are unreachable — the archive is opaque bytes bounded only by F6.
   Any future auto-extraction is a new filesystem-write surface and requires a
   new ADR plus its own threat entries here.
10. **F10 — No execution, no interpretation, no side effects.** The receiver
    never opens, launches, previews, shell-associates, indexes, or marks
    executable what it wrote, and performs no post-write action beyond the
    diagnostic. Received content becomes active only when the user chooses to
    act on it.
11. **F11 — Every rejection is observable; content never is.** Permission
    denials, name rejections, cap breaches, and collision failures each produce
    a diagnostic naming the reason (NFR-3, FR-3.6) — never a silent drop.
    Diagnostics may record the rejected name in escaped form; the **contents**
    of a received file are never logged, on the same footing as clipboard
    contents (invariant 6).

## 8. Security-sensitive code areas

Changes in these areas require heightened review and, where architectural,
an ADR: identity generation, key storage, credential verification, pairing,
trust store persistence/revocation, TLS configuration, protocol parsing and
length validation, input/clipboard authorization gates, received-file name
validation and destination-path construction, drop-folder configuration and
archive handling, and logging/diagnostics (for leak risk).

Security review is a standing item in each phase's exit criteria and a
dedicated activity in Phase 6 ([ROADMAP.md](ROADMAP.md)).
