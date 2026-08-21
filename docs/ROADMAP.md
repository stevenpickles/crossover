# Crossover Roadmap

> **Current phase: 8 — Dynamic Display Topology** (not yet started.)
>
> Phase 7 (Rich Clipboard — images and files) closed 2026-08-20: the last
> open exit criterion — **input latency bounded under a saturating
> background transfer** — was measured on the wired link the decision had
> been waiting for, and it passes. Images and files were already
> delivery-proven on hardware (2026-08-16 and 2026-08-19, recorded below);
> what remained was the number, and the number the 64 KiB hold was really
> about is the socket write.
>
> **Measured on wire, 2026-08-21T00:19:07Z** (evening of 08-20 local).
> Direct wired link, machine A (Intel I225-LMvP 2.5 GbE, dock-attached,
> listener) ↔ machine B (10 GbE, dialer), negotiated 2.5 Gbps full duplex,
> both machines on `dev` at `f69afc8`. B drove the contention ADR 0013 is
> actually about — **one writer carrying both**: interactive input (B
> controlling A, continuous mouse and keyboard) and bulk file data (B → A)
> over the same connection. Ten distinct 200 MiB random-content files
> (distinct so hash-dedup could not shortcut them) were pasted back-to-back
> between 00:04:34 and 00:05:13 UTC — 39 s, ~1 s per delivery (file
> delivery p50 906 ms / p95 1080 ms) — with input running the whole time,
> then hands-off until the interim metrics line. B's counters over the
> window: 4,558 input samples (4,561 input events), 36,732 frames sent,
> 2,098,396,126 bytes sent, `clipboard_files_sent=10`
> (`clipboard_file_sent_bytes=2,097,152,000`).
>
> | | avg | max |
> |---|---|---|
> | socket accepting the bytes (`input_write`) | **0.019 ms** | **0.147 ms** |
> | waiting for the writer (`input_lane`) | 0.41 ms | 72.2 ms |
> | queue-to-wire, total (`input_queue`) | 0.43 ms | 72.2 ms |
>
> **This is a pass, and it settles the 64 KiB question.** The socket-write
> path — the thing the hold questioned — never exceeded **0.147 ms** under
> full saturation, which is below even the **0.21 ms** ADR 0013 costs a
> *single* 64 KiB chunk at 2.5 GbE, and against the WiFi run's mean 1.94 ms
> / max 309.8 ms. The 2026-08-16 failure is therefore attributable to the
> slow physical link, not to the chunking design. **Resolution
> (maintainer, 2026-08-20): the 64 KiB chunk size stands** — recorded as
> [ADR 0013](adr/0013-interactive-over-bulk-prioritization.md)'s 2026-08-20
> addendum, with [ARCHITECTURE.md](ARCHITECTURE.md) §5.4's "held pending a
> wired measurement" closed to match. The writer-task redesign §5.4 said
> this measurement would price is **not warranted by this evidence**: §5.4
> already records that a writer task still writes serially into one TLS
> stream, and the socket figures leave it nothing to win.
>
> **The window is clean, which is what makes the maxima mean anything.**
> B's service was restarted at 00:04:06 UTC for fresh counters (the
> measurement start, T), and across T → T+15 A's log shows one unbroken
> session while B reports `reconnect_attempts=0` — so every input sample
> coexisted with genuine transfer traffic and nothing else. An earlier
> attempt the same day *was* polluted, by an environmental NIC link drop,
> and was discarded rather than reported.
>
> **One honest observation, recorded rather than buried.** A single tail
> event of **~72 ms** appeared in the interactive lane while socket writes
> stayed ≤ 0.147 ms — so it is a pre-writer scheduling/queueing stall, not
> bulk head-of-line blocking at the socket, which is the failure mode this
> criterion exists to catch. The averages (0.41 ms lane, 0.43 ms total)
> place it as one outlier among 4,558 samples, and the operator perceived
> nothing. Worth a future investigation, not a blocker; it is listed with
> the phase's carried follow-ups under Phase 7's exit criteria below.
>
> **Images landed 2026-08-16.** The Windows CF_DIB backend carries real
> images between two machines verbatim in both directions, and the feature
> bit is advertised now that the promise behind it is real (ADR 0014). It was
> validated on hardware against build `0.1.0-dev.351.g15f28c283` — the first
> build able to name its own commit — over a 7h45m two-machine session:
> ~144 MiB received, 11 items sent and 9 applied, one conflict resolved,
> **zero retries and zero contention**, clipboard latency max 10 ms, and a
> deferred-queue peak of 0 (the Background lane never backed up hard enough
> to park the driver, the first real evidence on how `MAX_DEFERRED_EVENTS`
> is sized).
>
> **Hash-dedup is demonstrated** (feature/116): over a real session, an
> image the receiver already holds costs one offer and one decline —
> `AlreadyHave`, then silence on the wire — instead of megabytes. The engine
> already decided this correctly; what was missing was proof that nothing
> followed the decline onto the wire, which is the half the criterion is
> about.
>
> **Input latency was measured, and it does not meet the criterion.**
> Instrumented in feature/117 — every input frame timed from the moment it
> is handed to the send path to the moment it reaches the wire — and read on
> hardware 2026-08-16 during a saturating image transfer (~127 MiB sent,
> 7,571 input frames timed):
>
> | | measured | expected |
> |---|---|---|
> | mean | **1.94 ms** | tens of µs |
> | max | **309.8 ms** | single-digit ms |
>
> ADR 0013 costed a 64 KiB chunk at "0.21 ms of 2.5 GbE, so one chunk of
> worst-case input delay stays sub-millisecond". The measured mean already
> breaks that, and the maximum is three orders of magnitude past it.
>
> **Attributed on the second run** (feature/118 split the wait): of a
> 124.3 ms worst case, **124.3 ms was the frame waiting for the writer and
> 0.18 ms was the socket** accepting its own bytes. The input frame's bytes
> leave quickly; it waits because the session loop is mid-write on a 64 KiB
> bulk chunk. Head-of-line blocking behind one in-flight frame — the "a
> frame in flight is unpreemptable" limit ADR 0013 names — not the lane
> split failing and not the link refusing small writes.
>
> **Held at 64 KiB** (maintainer, 2026-08-16), to be revisited on a wired
> link. The measurement was taken over **WiFi**, which ADR 0013's arithmetic
> never contemplated: the same chunk is 0.21 ms of 2.5 GbE and ~124 ms of a
> bad wireless moment. Chunk size is the only lever that acts on this
> directly, and it is cheap to move — the receiver takes its plan from the
> first chunk, so a smaller sender-side chunk needs no protocol change. Note
> that **moving the writer to its own task would not help**: it still writes
> serially into one stream (see ARCHITECTURE.md §5.4, corrected).
> **Resolved 2026-08-20**: the wired re-run happened, the socket write
> measured 0.147 ms worst case, and 64 KiB stands unchanged — see the
> closure summary at the top of this marker.
>
> The mechanism is already documented rather than newly discovered:
> [ARCHITECTURE.md](ARCHITECTURE.md) §5.4 records that the session loop
> polls nothing while a write is pending, names "moving the writer to its
> own task" as the fix that "would remove the freeze entirely", and defers
> it as needing an ADR of its own. This measurement is the evidence that the
> deferral now has a cost worth pricing. `clipboard_deferred_peak` also went
> non-zero (1) for the first time, so the Background lane genuinely backed
> up.
>
> Nothing here is unsafe or stuck; it is responsiveness under a saturating
> bulk transfer, on a link that was never the design target. Files/folders
> ([ADR 0015](adr/0015-spooled-virtual-file-paste.md), **Accepted**
> 2026-08-17) is the second sub-milestone and is where the work is now.
>
> **The receiving half is built** (2026-08-17). A peer file is admitted
> against permission, free space and the spool budget *before* the offer is
> answered, streamed through to the spool a chunk at a time — memory stays
> O(chunk), which is why a file may be 256 MiB where an image is capped at
> 64 — verified against the offered hash and length, and promoted to an
> entry only then. Every other outcome deletes the partial and registers
> nothing. The engine stayed sans-io: it decides, and the driver performs
> the four spool operations and reports back, so the guarantees are unit
> tests over an action list rather than filesystem fixtures. The
> `file_receive` grant reaches a running worker through the poll that
> already re-reads the trust store for revocation.
>
> **The data object exists** (2026-08-17). A spooled entry can be offered
> to Explorer as a virtual file list on an apartment thread of its own:
> the descriptor carries the validated name and size, the contents are
> served only as a read-only stream at index zero, and the file the shell
> writes records where it came from (Local intranet — the internet zone
> was built first and changed on a maintainer decision, ADR 0015).
> Automated tests drive it through the real
> clipboard as a consumer does and read the entry back byte for byte. Two
> findings came out of building it, both recorded in ADR 0015:
> `OleIsCurrentClipboard` alone reported "still ours" after a same-process
> Win32 write, so ownership now also requires an unchanged clipboard
> sequence number (SECURITY.md F13); and the OLE clipboard's own mediating
> object answers out-of-enumeration requests before ours does, so the
> typed refusal codes are asserted against the object directly.
>
> **The receiving half is complete** (2026-08-18). A completed transfer is
> now offered: the engine holds the origin's verdict until the entry
> reaches the clipboard, so `Stored` means the user can paste it rather
> than that bytes exist somewhere, and an offer that never lands deletes
> the entry instead of leaving peer bytes nothing advertises. With
> something on the clipboard to observe, the entry-lifetime rule is real —
> an entry lives while the clipboard still offers what it backs and is
> collected the moment it moves on, with the 24-hour age backstop only for
> the case the rule cannot see. The same observation is what stops our own
> offer being staged back to the peer that sent it, which no content hash
> could do: a virtual file list has no bytes to hash.
>
> **The sender's two lower halves exist** (2026-08-18). A local `CF_HDROP`
> copy is observed and handed up as paths rather than bytes, and a
> selection is packed into one blob — a single file verbatim, a folder or
> a multi-entry selection as one Stored-entry archive — with the exact
> length and SHA-256 the offer must carry. Every refusal ADR 0015 names is
> typed and happens before anything could travel: entry count, depth and
> cumulative bytes are judged *during* the walk, a reparse point or an
> unreadable entry refuses the whole item rather than quietly shrinking
> it, and the temporary artifact is delete-on-close so no refusal path can
> leave one behind. The five decisions this forced are recorded in ADR
> 0015 rather than left in the code.
>
> **The send transaction is built** (2026-08-18). A local file selection
> now becomes an offer and a chunk stream: the peer's negotiated file
> support and its `clipboard_send` grant are judged *before* anything is
> walked, because the build is the expensive step and an un-negotiated
> file offer is fatal to an older peer's session rather than merely
> unanswered. The bytes never enter the engine — an outbound item carries
> either bytes it retains or a blob the driver holds open, and for a blob
> the engine names each chunk's offset and length and the driver reads
> exactly that slice, so the sender is O(chunk) exactly as the receiver's
> write-through is. Every path that ends the transaction hands the blob
> back, which is what deletes the artifact: delivered, declined,
> superseded, timed out, session lost, unreadable. The sender's half of
> loop prevention is in too — a `CF_HDROP` resolving inside the spool is
> never staged.
>
> **The sender side is built** (2026-08-18): local `CF_HDROP` observation
> (feature/133, PR #38), the blob builder (feature/134, PR #39), and the
> engine's send transaction over it (feature/135, PR #40) — the three
> paragraphs above.
>
> **`FILE_CLIPBOARD` is advertised** (2026-08-18, feature/136). This was the
> deliberate final act: `FeatureFlags::ADVERTISED` is now `ALL`, so a
> conforming peer can actually reach either half of the file path instead of
> negotiating it away. The send policy is computed the way
> `file_receive_policy` already computes its twin — `SessionRoute` now
> carries each live session's negotiated features, `file_send_policy` folds
> them with the trust store's `clipboard_send` grant into
> `FileSend::{Unsupported, NotNegotiated, Denied, Allowed}`, and it is
> published at the same two points `file_receive_policy` is: session
> establishment/loss, and the trust-store revocation poll — so revoking
> `clipboard_send` reaches a running worker within one poll, exactly as
> revoking `file_receive` already did.
>
> Two things worth being plain about, so this entry does not overstate what
> landed. **`clipboard_send` is, as of this slice, the first permission this
> codebase enforces anywhere** — text and images still travel without
> checking it, unchanged from before; this slice was not scoped to fix that.
> And **this closes construction, not validation**: the whole file path —
> offer, blob build, chunked send, spool, verify, virtual-file paste — has
> been exercised by the test suites only. No two-machine hardware run has
> moved a real file between the two workstations yet, which is what the
> phase needs next before files can be called done. One question is still
> answered by a human rather than by CI before that: whether Windows honours
> the clipboard-history/cloud exclusions the virtual-file object declares
> (docs/TESTING.md §1.6) — open, carried forward.
>
> **The files slice is hardware-validated** (2026-08-19), closing the item
> above. A two-machine session over a direct 2.5/10 GbE wired link —
> machines A (listener) and B (dialer), initial build `e689a3b`, final
> retests on the build containing PRs #43–#46 — exercised the whole path
> construction had previously only test suites for
> ([ADR 0015](adr/0015-spooled-virtual-file-paste.md)'s 2026-08-19 addendum
> and [SOAK.md](SOAK.md)'s Phase 7 files section carry the full session
> record). Single-file and folder/multi-selection transfers arrived
> byte-identical in both directions and opened without SmartScreen or
> Protected View, carrying `ZoneId=1`; `AlreadyHave` dedup, the >256 MiB
> `TooLarge` refusal (observed live, 268,500,992 bytes vs. the 268,435,456
> max), and loop prevention (a deliberate spool-path copy probe suppressed
> silently, `clipboard_loop_suppressed` incrementing exactly once) all held.
> **The question left open above is now answered**: docs/TESTING.md §1.6's
> third exception (F16) is confirmed on wire-crossed hardware, not just
> locally — a received offer does not appear in Win+V history and does not
> cloud-sync, and the pasted file opens unprompted with `ZoneId=1`.
>
> The session found and fixed four real defects, all merged to `dev`: an
> edge-transfer bounce under a hand tremor at the seam, ADR 0009's
> deliberately deferred push-through risk materializing (PR #44, feature/137,
> re-arm hysteresis); a control-request lockout from a ~4.7 s-late answer
> racing its own retry into a self-healing ~7 s lockout (PR #45, feature/139);
> an inbound head-of-line block — control frames were gated on the clipboard
> driver's queue, with no inbound interactive/bulk separation, ADR 0013
> having been outbound-only until now (PR #46, feature/140); and a silent
> worker death on B at 02:05:38 UTC that self-healed in ~1.3 s with no stuck
> input, but whose root cause the service recorded nowhere — fixed forward,
> not diagnosed, by durable supervision logging (PR #43, feature/138, ADR
> 0011 addendum). Retests on the post-#46 build confirm the seam no longer
> bounces and ~20 rapid crossings ran clean.
>
> **This closes delivery-proven for files, not Phase 7.** The phase's input
> latency exit criterion is still open: it remains held at 64 KiB
> (2026-08-16, above), and this session's hardware read — input responsive
> throughout large file transfers, p50 input-path latency ~3 ms during sends
> — is an informal wired-link data point, not the rigorous max-latency
> instrumentation (feature/117/118) redone over wired. The wired link that
> just proved out files is exactly the link that decision was waiting to be
> revisited on; a formal wired re-run of that measurement is a candidate next
> step, not undertaken here. (It was undertaken on 2026-08-20 and closed the
> phase — the closure summary at the top of this marker.) Also skipped, deliberately: the mid-transfer
> network-disconnect case — at 2.5 Gbps a 200 MiB transfer completes
> sub-second, leaving no practical "mid" — with the 02:05:38 abrupt-disconnect
> recovery standing as live evidence for that class of fault ([SOAK.md](SOAK.md)).
>
> Phase 6 (Windows Prototype Hardening) closed 2026-08-14: the multi-day
> unattended soak — the last exit criterion — ran 2026-08-11 → 2026-08-14
> between the two workstations under the background service, with no manual
> intervention and no re-pairing (outcome recorded in docs/SOAK.md §Phase 6
> soak). The hardening was exercised for real: machine A was off from midday
> 08-12 to early 08-14 — with B itself down for stretches of that window —
> and B's supervisor retried at the capped 30 s backoff whenever it was up,
> re-establishing on its own within seconds of both machines returning; an
> early-soak stretch of immediate worker exits on B had the
> service relaunch on the ADR 0011 backoff until a launch came up cleanly,
> ~19 minutes later, unattended. Clipboard and input reliability held
> throughout — the few clipboard failures were bounded and observable
> (retries, then a logged reason), and no input was ever left stuck. Two follow-ups came out of the soak, **both now
> closed**: the display topology was captured once at startup, so unplugging
> or powering off a monitor left a stale seamless edge (fixed in
> feature/107), and a worker that exited before its run loop recorded nothing
> about why — panics went to a `NUL` stderr and the file sink could go
> missing silently, both fixed in feature/115. Earlier Phase 6 deliverables — hardened
> reconnect, sectioned/versioned startup configuration, active-session
> revocation ([ADR 0010](adr/0010-active-session-revocation.md)), the
> dedicated security review against [SECURITY.md](SECURITY.md) §6-§7
> (docs/security-review-phase6.md), reconnect-recovery metrics, unattended
> background operation ([ADR 0011](adr/0011-background-service-launcher.md),
> [ADR 0012](adr/0012-elevated-worker-integrity.md)), and Windows packaging —
> were validated on hardware as they landed.
>
> **Sequencing (2026-08-16):** after Phase 7's files half, the next phase is
> **dynamic display topology with a drag-and-drop editor** — maintainer
> decision. Cross-platform validation moves to Phase 9 and productization to
> Phase 10. The reasoning is that two Windows machines sharing a keyboard and
> mouse *well* are worth more than three platforms sharing one adequately,
> and that the topology model is far easier to change before three platform
> crates depend on it than after. The macOS and Linux risk catalogues written
> for the old Phase 8 keep their value and simply wait
> ([platform-risks-macos.md](platform-risks-macos.md),
> [platform-risks-linux.md](platform-risks-linux.md)).
>
> Accepted ADRs that name "Phase 8" meaning cross-platform are left as
> written — they are immutable, and ADR 0014 already annotates the same
> drift from the previous re-sequencing.
>
> **Sequencing (2026-08-11):** after Phase 6, **rich clipboard (images and
> files) is scheduled before cross-platform validation** — maintainer decision.
> The hard part of rich clipboard is platform-neutral (prioritization, chunked
> protocol, transaction engine), so it is hardened once on Windows, where the
> real issues surface, then carried to macOS and Linux. The phases below are
> renumbered: 7 Rich Clipboard, 8 Cross-Platform Validation, 9 Productization.
>
> Phase 5 (Seamless Crossover) closed 2026-08-09: the two-machine seamless
> soak ran on real hardware — a two-monitor, mixed-DPI machine paired with a
> single-monitor one. The cursor crosses a screen edge and control *and*
> keyboard follow on their own, control returns at the reverse edge with no
> console command, the clipboard stays synced throughout, and exactly one
> cursor is visible — on the active machine. ADR 0009 records the design:
> the edge crossing is a new trigger on the existing control engine, the
> crossing position travels as a fraction of the *edge monitor* (so
> mismatched resolution and DPI land the cursor at the matching height), and
> the return is an instant controlled-side revoke.
>
> The soak drove several fixes: Right Shift arrives E0-extended on real
> hardware and was being dropped, un-shifting right-hand symbols (fixed in
> capture); the crossing fraction now maps against the specific edge
> monitor, not the mismatched bounding box; and "one visible cursor" went
> through the wringer — a transparent overlay could not span mismatched
> monitors, so masking is done with `SetSystemCursor`, applied off the
> control loop, restored synchronously on quit / lost connection / next
> launch, and — the safety net — shown again the instant local input is
> seen on a hidden-but-not-driving machine. One handshake race is parked as
> a known latent item: a brief cross-machine state disagreement can leave a
> machine controlling with no visible cursor; the local-input fail-safe
> recovers it within ~200 ms, so it is a tidy-up, not a blocker
> (docs/SOAK.md Phase 5 limitations).
>
> Phase 4 (Remote Keyboard) closed 2026-08-09: the two-machine keyboard
> soak (docs/SOAK.md) ran on real hardware — normal typing and shortcuts
> forwarded cleanly, repeated control cycles left no stuck keys or
> modifiers, and the both-Control escape returned control every time. One
> app-specific finding: `Shift+Home`/`Shift+End` did not extend a
> selection in a native editor with its own key handling, while
> `Shift+Arrow` did. The input pipeline was proven correct end to end —
> capture, coalescing, injection, and injection→selection into a standard
> control all verified in `crossover-platform-windows` probes — so the
> behavior is the editor's own, not a forwarding defect (docs/SOAK.md
> Phase 4 limitations).
>
> Phase 3 (Remote Mouse) closed 2026-08-08: the two-machine remote-mouse
> soak ran on real hardware — clean takeover, smooth motion, and
> disconnect mid-drag left no stuck buttons. One cosmetic follow-up is
> tracked separately (a deliberate quit logs a spurious TLS-close warning
> on the peer; ReleaseAllInput still fires, so it is diagnostics-only).
>
> Update this marker when a phase's exit criteria are verified. Do not begin
> a later phase because time remains — complete and validate exit criteria
> first. Later-phase functionality is implemented early only when required
> to keep the architecture clean.

Phases build the three hardest foundations first — security, networking,
reliable clipboard sync — before any input forwarding. The first meaningful
user-facing milestone is the **Secure Clipboard Prototype** (end of
Phase 2), not remote input.

---

## Phase 0 — Repository Foundation (completed 2026-08-07)

**Goal:** a professional Rust workspace that builds and tests on all three
desktop OSes, with the documentation and decision framework in place.

Deliverables:

- Cargo workspace with the initial crates from
  [ARCHITECTURE.md](ARCHITECTURE.md) §3 (skeletons with placeholder tests)
- `rust-toolchain.toml` (pinned toolchain + MSRV), rustfmt config, clippy
  policy (warnings denied)
- GitHub Actions: fmt/clippy/build/test on Windows, Linux, macOS
- structured logging foundation (tracing) wired into the app skeleton
- error-handling conventions established (thiserror/app-boundary pattern)
- ADR framework in place ([adr/README.md](adr/README.md)); the decisions
  Phase 1 depends on are recorded as its prerequisite ADRs at Phase 1 entry
- README kept current

Exit criteria:

- `cargo build/test/fmt --check/clippy` pass on all three OSes in CI
- No clipboard/input/network functionality exists yet — that is correct

Verified 2026-08-07: CI green on ubuntu/windows/macos for `dev` at the
Phase 0 tip (runs 31185855135, 31186123879).

## Phase 1 — Secure Peer Connection (completed 2026-08-07)

**Goal:** two Windows computers establish and retain a secure trusted
relationship.

Deliverables: TCP listener/connector; TLS 1.3 with mutual authentication;
persistent device identity in `SecureStorage`; pairing ceremony
([SECURITY.md](SECURITY.md) §3); trusted peer store with revocation; Hello
exchange and version negotiation ([PROTOCOL.md](PROTOCOL.md) §3); automatic
reconnect with bounded backoff; `crossover pair/peers/status`; structured
connection logging; test-peer tool covering connect/auth/negotiate paths.

Prerequisite ADRs: serialization format, pairing mechanism, identity
credential form, default port (deferred decisions 1, 2, 3, 6).

Exit criteria — two fresh Windows machines can:

1. explicitly pair, 2. establish an encrypted connection, 3. restart
Crossover and reconnect **without re-pairing**, 4. reject a third untrusted
machine.

Plus: packet capture reveals no plaintext application payload (automated
where practical); every authentication failure mode produces an actionable
diagnostic; fuzz targets exist for all Phase-1 parse paths.

Verified 2026-08-07:

- CI green on all three OSes plus the fuzz smoke at the Phase 1 tip of
  `dev` (run 31197398890).
- Exit criteria demonstrated live via two isolated-storage instances on
  one Windows machine (accepted in lieu of two physical machines, by
  explicit decision): explicit pairing with a typed code; encrypted
  session establishment both roles; listener killed and restarted with
  automatic reconnection and no re-pairing; a third machine paired
  elsewhere refused by mutual authentication.
- Plaintext check via a byte-capturing TCP relay around a live session:
  canary device names absent from every wire byte, the capture parses as
  contiguous TLS records (application data only inside TLS), and the only
  plaintext is the semantics-free SNI placeholder. TLS 1.3 keeps
  certificates (and thus fingerprints) encrypted as well.

## Phase 2 — Reliable Text Clipboard (completed 2026-08-07)

**Goal:** extremely reliable bidirectional UTF-8 text clipboard sync —
before any input forwarding.

Deliverables: Windows clipboard monitoring behind `ClipboardProvider`
(sequence-number change detection, contention handling — risks R-4/R-5);
clipboard engine with ids, hashing, acknowledgement, loop prevention,
bounded retry, latest-wins conflict policy; transaction flow from
[PROTOCOL.md](PROTOCOL.md) §5 (prerequisite: ADR for deferred decision 4);
reconnect-safe behavior; metadata-only transaction logging; clipboard
integration + fault-injection suites.

Exit criteria:

- Automated stress: **≥10,000 bidirectional clipboard updates** with zero
  content corruption, zero sync loops, zero unexplained ordering failures,
  zero silent failures, zero crashes ([TESTING.md](TESTING.md) §3)
- Every ultimately-failed update produces an observable diagnostic
- **Milestone: Secure Clipboard Prototype** — pair, trust persistence,
  encrypted transport, reliable bidirectional clipboard, auto-reconnect

Verified 2026-08-07 — **milestone achieved**:

- **Stress gate**: 10,000 bidirectional updates, `applied=10000`,
  `corrupt=0`, `failed=0`, exactly 20,000 frames (no synchronization
  loop). Hermetic, so it gates every merge
  (`tools/test-peer/tests/stress.rs`).
- **Two physical Windows machines** on one LAN (192.168.50.100 /
  .101) ran the [SOAK.md](SOAK.md) runbook: one-directional sync,
  simultaneous bidirectional copying, items above the offered-flow
  threshold, Ethernet unplugged 30 s and replugged, and process
  restart. Trust persisted; reconnection needed no re-pairing;
  conflict resolution agreed on winners from both sides' logs; no
  content mismatch in any run.
- **Latency** (originating machine's own clock): p50 4–6 ms once a
  transaction starts.
- **Clipboard citizenship**: zero `read_busy` events and zero
  `Set-Clipboard` failures in other applications during bidirectional
  load — see below.

The soak found two real defects that no hermetic test could, both fixed
and re-verified rather than merely recorded:

1. Crossover held the machine-global clipboard often enough to make
   *other* applications' copy operations fail outright. Three rounds of
   measurement narrowed it — critical-section length, then write
   volume, then finally the reads, which took the same lock on every
   change notification. Reported failures went from hundreds per run to
   zero.
2. Sustained read contention starved inbound work: an acknowledgement
   waited 27 seconds behind a self-re-enqueueing retry loop sharing one
   serial queue.

That work produced [ADR 0006](adr/0006-clipboard-transmission-triggers.md):
clipboard transmission is trigger-driven rather than change-driven.
Control transfer becomes the primary trigger in Phase 5 (recorded in
that phase's deliverables); a settled-change debounce carries Phase 2
and remains the fallback.

## Phase 3 — Remote Mouse

**Goal:** control the second computer's pointer.

Deliverables: mouse capture and injection behind platform traits
(ADR 0007: hooks suppress, Raw Input supplies motion, `SendInput`
injects with tagged events; respect the hook-timeout budget R-2);
pointer transport with coalescing; buttons and wheel; coordinate
abstraction (DPI-aware, R-3); button state tracking with
`ReleaseAllInput`; **explicit** control switching via CLI command or
hotkey — no edge detection yet.

Exit criteria: second machine controllable with first machine's mouse;
repeated activate/deactivate cycles never leave stuck buttons (fault-
injection verified); pointer response subjectively suitable for desktop use
on LAN; injection failures against elevated windows are detected and
diagnosed (R-1).

## Phase 4 — Remote Keyboard

**Goal:** forward keyboard input safely.

Deliverables: keyboard capture/injection; key representation per FR-4.1;
modifier handling; ordered key transitions; key-state tracking;
`ReleaseAllInput` on every disconnect path.

Exit criteria: normal typing and common shortcuts work remotely; repeated
activation/deactivation correct; disconnect at **arbitrary** moments
(fault-injected) never leaves keys or modifiers logically pressed.

Verified 2026-08-09:

- **Hermetic**: the control-transfer property tests
  (`granted_keys_are_injected_and_released_on_disconnect`,
  `hand_back_releases_everything_the_peer_left_held`, and the proptest
  `any_interleaving_ends_clean_on_disconnect`) prove the state machine
  ends with nothing held on every interleaving; the HID↔scancode table
  round-trips every key with its extended flag; capture, injection, and
  injection→selection are exercised through the real Win32 pipeline
  (`crossover-platform-windows`).
- **Two physical Windows machines** on one LAN ran the [SOAK.md](SOAK.md)
  Phase 4 runbook: normal typing, modifier chords (including the
  `Ctrl+C`/`Ctrl+V` clipboard round trip), auto-repeat, the both-Control
  escape, repeated control cycles, and a disconnect with a modifier held.
  No key or modifier ever stuck; the escape returned control every time
  and never leaked to the peer; the shutdown metrics reported clean
  hand-backs and no reconnects.
- **One app-specific finding, investigated to ground truth**:
  `Shift+Home`/`Shift+End` failed to extend a selection in a native
  editor with its own key handling while `Shift+Arrow` worked.
  Single-machine probes proved the forwarding path correct at every stage
  — Home/End are captured identically to the arrows (right extended flag,
  right HID, Shift held, no phantom shift), Windows synthesizes no phantom
  shift on injection, and the exact injected batch drives a selection in a
  standard edit control. The divergence is the editor's own Home/End
  behavior, verifiable by comparing local and remote input in that app
  (docs/SOAK.md Phase 4 limitations); it is not a forwarding defect and
  does not block the exit criteria.

## Phase 5 — Seamless Crossover

**Goal:** two computers behave like neighboring monitors.

Deliverables: monitor enumeration; topology configuration; edge detection;
negotiated control-transfer state machine ([ARCHITECTURE.md](ARCHITECTURE.md)
§5.1); cursor mapping across DPI differences; automatic keyboard ownership
transfer; automatic return; transition diagnostics.

Exit criteria: with `A | B`, the user repeatedly moves A → B → A through
screen edges with no manual switching; keyboard follows the active machine;
clipboard stays synchronized throughout; transfer under induced packet
delay/loss converges to exactly one owner.

## Phase 6 — Windows Prototype Hardening

**Goal:** suitable for continuous daily use.

Deliverables: hardened reconnect; startup configuration; background
operation and lifecycle management; graceful shutdown and crash recovery;
performance instrumentation ([TESTING.md](TESTING.md) §4); installer or
package; configuration validation polish; **dedicated security review**
against [SECURITY.md](SECURITY.md) §6-§7; long-duration soak testing.

Exit criteria: multi-day continuous operation between two Windows
workstations without manual intervention; transient network loss and peer
restarts recover automatically; clipboard reliability requirements still
hold under soak; security review findings resolved or accepted by ADR.

## Phase 7 — Rich Clipboard (images and files)

**Goal:** extend the reliable clipboard from text to **images and files** —
built and hardened on **Windows first**, so the platform-neutral protocol and
engine work is settled where the real issues surface, before the cross-platform
port (Phase 8) carries it to macOS and Linux. Re-sequenced ahead of
cross-platform by maintainer decision (2026-08-11): the hard part is neutral
(prioritization, chunked protocol, transaction engine), so hardening it once on
Windows turns the later port into "implement the clipboard trait for formats
already designed" rather than designing rich clipboard three times.

Design captured 2026-08-11 (Proposed [ADR 0013](adr/0013-interactive-over-bulk-prioritization.md),
[ADR 0014](adr/0014-chunked-rich-clipboard-transfer.md)). Three linked pieces,
in dependency order:

1. **Interactive-over-bulk frame prioritization** (ADR 0013). Split the
   session's single FIFO send path into High (input, control, keepalive) and
   Background (bulk) classes; input always preempts bulk *between chunks*.
   Foundational — makes "background transfer never interferes with live input"
   (priority #5 / NFR-5) real, and forces bulk to be chunked (a big frame is
   unpreemptable).
2. **Chunked rich-clipboard transfer, images first** (ADR 0014). The chunking
   ADR 0005 predicted. Images (screenshots/snips, a few MB) are the value:
   native raster format shipped **verbatim** (no transcode, no codec, no
   compression on a 2.5 GbE LAN), eager chunked sync consistent with text,
   chunks sized to ADR 0013's latency budget, before-allocation bounds (NFR-1),
   hash-dedup so a re-paste moves zero bytes. Plus the Windows platform
   clipboard read/write for the raster format.
3. **Files/folders — deliberately minimal, spooled virtual-file paste
   ([ADR 0015](adr/0015-spooled-virtual-file-paste.md)).** Not a drop folder
   (the first draft's model, rejected on user experience): a completed
   transfer spools internally and is offered to the OS paste mechanism as a
   **virtual file list** (`CFSTR_FILEDESCRIPTORW` + `CFSTR_FILECONTENTS`
   through an `IDataObject` we own), RDP-style — the paste destination is
   wherever the user presses Ctrl+V. Folders are zipped by the sender to a
   single blob and never extracted by the receiver; per-peer permission is
   default-off; names are validated reject-not-repair (no paths/`..`/drive
   letters); sizes and counts are capped. This is the first peer-controlled
   write surface onto disk, so the spool/paste threat additions live in
   SECURITY.md alongside the ADR (both landed 2026-08-12; implementation not
   yet scheduled).

Exit criteria:

- Images copy/paste **both directions, byte-identical**, reliably through a
  soak; re-pasting the same image transfers zero payload bytes (hash-dedup).
- **Live input stays responsive during a concurrent bulk transfer** — measured:
  input latency bounded under a saturating background transfer (the ADR 0013
  guarantee), not merely subjectively smooth.
- Delivered files leave Crossover's internal spool and materialize in a
  user-visible location only through an explicit user paste, with every ADR
  0015 guardrail enforced; oversized or path-dubious inputs are rejected
  **observably** (FR-3.6), never a traversal write or a silent drop.
- No regression in text-clipboard reliability (the Phase 2 stress gate still
  passes) or in input latency/correctness.

The hermetic stress and fault-injection suites extend to cover chunked transfers
and the priority guarantee. Files/folders may run as a second sub-milestone
after images, given the added security surface.

Verified 2026-08-20 — **all four criteria met, phase closed**:

- **Images, both directions, byte-identical, plus hash-dedup**: the
  2026-08-16 two-machine session (7h45m, ~144 MiB received, real snips into
  Paint/Word/browser on the far side), and feature/116's session evidence
  that an image the receiver already holds costs one offer and one
  `AlreadyHave` decline with nothing following it onto the wire.
- **Live input stays responsive during a concurrent bulk transfer,
  measured**: the wired measurement of 2026-08-21T00:19:07Z under ten
  back-to-back 200 MiB file transfers with continuous input on the same
  writer — socket write avg 0.019 ms / max 0.147 ms, total queue-to-wire
  avg 0.43 ms / max 72.2 ms, inside ADR 0013's per-chunk budget (full record
  in the marker above and docs/SOAK.md's Phase 7 input-latency section).
- **Files materialize only through an explicit user paste, guardrails
  enforced, refusals observable**: the 2026-08-18 → 2026-08-19 files session
  — both directions byte-identical from the spool via Ctrl+V, `TooLarge`
  refused live at 268,500,992 bytes, loop prevention suppressing a
  spool-path probe exactly once ([ADR 0015](adr/0015-spooled-virtual-file-paste.md)'s
  2026-08-19 addendum).
- **No regression in text-clipboard reliability or in input
  latency/correctness**: the Phase 2 stress gate is hermetic and gates every
  merge, and no hardware session in this phase left input stuck or looped
  the clipboard.

Small follow-ups carried out of the phase, none of them blocking:

- **A ~72 ms tail in the interactive lane** (2026-08-21 measurement, one
  sample in 4,558) with socket writes ≤ 0.147 ms — a pre-writer
  scheduling/queueing stall to investigate, not bulk head-of-line blocking.
- **The service relaunches the worker into a dying session at logoff**,
  because it acts on the session-change notification before the `Logoff`
  stop reason arrives. Cosmetic: the relaunch fails harmlessly.
- **Exit code `0x40010004` (`DBG_TERMINATE_PROCESS`) is labelled
  `crashed=true`** in the supervision log, when at logoff it is Windows
  terminating the worker deliberately. Cosmetic: it misleads whoever reads
  the log next (docs/SOAK.md, 2026-08-20 session).

## Phase 8 — Dynamic Display Topology

**Goal:** replace "which side is this machine on" with an arrangement the
user draws. Monitors from both machines appear in one editor and are dragged
into the layout they physically sit in, and edge crossing follows from that
geometry instead of from a flag.

Re-sequenced ahead of cross-platform by maintainer decision (2026-08-16):
two Windows machines that share a keyboard and mouse well are worth more
than three platforms that share one adequately, and the topology model is
easier to change before three platform crates depend on it than after.

What exists today is the floor this replaces. `--left` / `--right` declares
a two-machine left–right pair (ADR 0009); a machine with several monitors is
treated as **one desktop**, so the crossing edge is the outer edge of the
whole desktop rather than a seam between monitors
([SOAK.md](SOAK.md) records that as an honest limitation); and feature/107
made the topology re-read at runtime when displays change, which this phase
builds on rather than repeats.

**Design recorded 2026-08-20.** The phase's decisions are in
[ADR 0018](adr/0018-drawn-display-topology.md) (Accepted) — the drawn layout
in one shared coordinate space, edges derived from exact adjacency,
protocol v4, config schema v2, the worker↔editor state file, and the new
`crossover-topology` crate. The UI toolkit decision deliverable 5 calls for is
[ADR 0019](adr/0019-layout-editor-toolkit.md) (Accepted): egui through eframe,
in its own on-demand user-session binary, `apps/crossover-layout`, which the
service never touches. ADR 0018 supersedes ADR
0009's *topology* only; the crossing mechanism (fractional position, the
negotiated engine as the trigger's target, the re-arm hysteresis, the cursor
mask) is retained and restated there.

Deliverables:

1. **A topology model that is a layout, not a side.** Both machines'
   monitors placed in one shared coordinate space, with edges derived from
   adjacency. This supersedes ADR 0009's side model and therefore needs an
   ADR of its own — the crossing *mechanism* (fractional position, negotiated
   transfer) is expected to survive; what changes is where edges come from.
2. **Per-monitor edges.** Dragging individual monitors only means something
   if a seam between two of one machine's monitors can differ from its outer
   edge, which the current "one desktop" treatment cannot express.
3. **An arrangement both machines agree on.** One layout describes two
   machines, so ownership, editing, and disagreement have to be answered:
   who holds it, how it reaches the peer, and what happens when they differ.
   If it travels over the session, it is protocol work and needs the ADR to
   say so.
4. **Persistence** in the versioned startup configuration, replacing the
   `side` setting rather than sitting beside it.
5. **The editor itself** — monitors shown to scale, dragged, snapped. This
   is the project's first GUI and needs a **UI toolkit decision recorded as
   an ADR** (a core library choice, per `adr/README.md`), including where it
   runs: the worker is a headless service-launched process (ADR 0011), so an
   editor is a separate user-session surface, not a mode of the worker.

Exit criteria:

- A layout drawn in the editor produces crossings that match it, including a
  seam between two monitors of the same machine and a corner where three
  monitors meet.
- Mixed DPI and mixed resolution behave: a pointer leaving a 4K monitor at
  40% of its edge arrives at 40% of the adjacent edge, whatever the scaling.
- A display added, removed, or rearranged at runtime updates the layout
  without a restart and without a stuck cursor (the feature/107 property,
  now with more to get wrong).
- The arrangement survives restart, and two machines that disagree resolve
  observably rather than silently mis-crossing.
- No regression in seamless transfer's existing guarantees: control returns
  at the reverse edge, no stuck keys, no cursor left hidden.

## Phase 9 — Cross-Platform Validation

**Goal:** prove the architecture is genuinely portable.

Deliverables: `crossover-platform-macos` and `crossover-platform-linux`
(created now, not before — [ARCHITECTURE.md](ARCHITECTURE.md) §3.1). The
risk catalogues this phase requires **before** implementation are written:
[platform-risks-macos.md](platform-risks-macos.md) (M-1..M-10) and
[platform-risks-linux.md](platform-risks-linux.md) (L-1..L-9).

Two findings from writing them change how the phase should start:

- **L-1 decides the Linux port's shape.** Wayland prohibits global input
  capture and injection by design; the sanctioned routes are compositor
  portals whose coverage varies. Verify that on current GNOME and KDE
  *before* any Linux code, because the answer is either "a port" or "X11
  today, Wayland when the portals are ready".
- **M-5 and L-9 agree that `CF_DIB` is the outlier.** Neither macOS nor
  Linux clipboards understand it, and PNG is the plausible interchange
  format. Drafted as [ADR 0016](adr/0016-image-interchange-format.md)
  (Proposed): the receiver advertises what it can install, the sender
  produces it by converting its own local content, and a receiver never
  decodes what a peer sent — which keeps an image decoder off the path that
  handles hostile input. Windows-to-Windows stays verbatim DIB.

Rich-clipboard image and file support (Phase 7) carries over here as new
implementations of the clipboard trait, not new protocol design.

Exit criteria: core feature set works Windows↔Windows, Windows↔macOS,
Windows↔Linux, macOS↔macOS, Linux↔Linux — and macOS↔Linux requires no
protocol changes.

## Phase 10 — Productization

Potential work, each item gated on preserving the security and clipboard
reliability requirements: tray application, graphical configuration (the
*topology* editor moved to Phase 8), peer discovery, >2 peers, further rich
clipboard formats
(e.g. HTML), drag-and-drop, software updates, diagnostics UI, optional
secure WAN operation.

---

## Working practices

- Decompose phases into tasks small enough to understand, test, review, and
  revert independently ("Define the ClipboardItem type with unit tests",
  not "Implement Phase 2").
- Keep the repository buildable after each integrated change.
- Record deviations from the specification suite deliberately — update the
  docs, don't diverge silently.
