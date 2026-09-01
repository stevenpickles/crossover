# Crossover Testing Strategy

Testing is an architectural requirement, not an afterthought. A feature that
worked once manually is not done. The architecture exists partly *so that*
the hard parts — protocol, state machines, clipboard semantics — run in
tests without hardware, sockets, or a second computer.

---

## 1. Test layers

| Layer | Runs | Scope |
|-------|------|-------|
| Unit | every CI run, all 3 OSes | pure logic in every crate |
| Property-based | every CI run | invariants over generated inputs (proptest) |
| Fuzz | smoke in CI, extended runs scheduled | network-facing parsers |
| Integration (test-peer) | every CI run | real networking + TLS against a scripted peer, single machine |
| Fault injection | every CI run | integration scenarios with induced failures |
| Platform (Windows) | Windows CI + real hardware | real Win32 clipboard/input/display behavior |
| Stress / long-duration | phase gates (see [ROADMAP.md](ROADMAP.md)) | reliability exit criteria |

### 1.1 Unit tests

Strong coverage is required for: protocol framing and validation, version
negotiation, clipboard dedup/ordering/conflict/loop-prevention, retry state
machines, trust-store validation and revocation, input state tracking,
`ReleaseAllInput`, control transfer, topology mapping, configuration
validation.

Core state machines are pure `(state, event) -> (state, commands)` functions
([ARCHITECTURE.md](ARCHITECTURE.md) §5), so these tests are deterministic —
no sleeps, no real time, no I/O.

### 1.2 Property-based tests

Where generated inputs meaningfully exceed hand-written cases:
serialization round-trips, framing (arbitrary split/coalesce of byte
streams must decode identically), clipboard ordering and dedup under
arbitrary interleavings, input state transitions (any event sequence +
disconnect ⇒ `ReleaseAllInput` leaves nothing pressed), control-transfer
state machine (never two owners, never zero owners after convergence).

### 1.3 Fuzzing

Every network-facing parse path in `crossover-protocol` has a fuzz target
(cargo-fuzz, in `fuzz/`). Goals: arbitrary bytes never panic, never
allocate unboundedly, never corrupt state; invalid sequences fail safely
into the fail-closed path ([PROTOCOL.md](PROTOCOL.md) §7). Decode targets
also assert re-decode equality: anything that decodes must survive an
encode → decode round trip unchanged. CI runs a short smoke of every
target on each change (`fuzz-smoke` job); longer runs are scheduled
later.

### 1.4 Integration via the headless test peer

`tools/test-peer` is a scriptable Crossover peer for CI on a single
machine. It can: connect, authenticate (with valid or invalid credentials),
negotiate (any version range), run clipboard transactions (offer/send/ack,
delayed ack, missing ack, duplicate items), disconnect/reconnect, and send
arbitrary malformed or out-of-order messages.

Integration tests drive the real application core + real TLS + real TCP
against the test peer, with platform traits backed by in-memory fakes.

### 1.5 Fault injection

Deliberately induced, asserted-on scenarios: dropped connections and
reconnects, delayed/duplicate/stale messages, clipboard lock contention
(fake `ClipboardProvider` returns transient failures), missing
acknowledgements, peer crash mid-transaction, abrupt TLS termination,
disconnect while keys are held down, rapid clipboard replacement bursts,
and a **saturating background transfer** — every send queue full and the
socket stalled behind a peer that will not read — with live input injected
into the middle of it (ADR 0013; `tools/test-peer/tests/priority.rs`).

Chunked transfers (ADR 0014) add their own faults, because a transfer with
many messages has failure modes a single frame does not: a session **torn
mid-stream** (nothing partially installed, no buffer left pinned, and the
same item transferring cleanly on reconnect as a fresh transaction —
`tools/test-peer/tests/stress.rs`); a transfer **accepted and then
abandoned** by a silent peer, which must expire rather than pin its
reassembly buffer for the life of the session; and **malformed chunk
sequences** — gaps, repeats, wrong lengths, foreign item ids — each
fail-closed, counted once per doomed transfer, and fatal only on
repetition (PROTOCOL.md §7). A streaming image is also run through the
saturation case above, since a chunk being preemptable is the whole reason
ADR 0014 chunks at all.

File transfers (ADR 0015) reuse that machinery and add the fault the
others cannot have: one that ends with bytes on **disk**, and now one that
ends with a *promise* on the clipboard — an offer that never lands deletes
the entry and reports the failure, rather than leaving bytes nothing
advertises. The engine's
hermetic tests assert each abandonment path leaves nothing registered and
the partial deleted — a tampered final chunk, a spool write that fails, a
rename that fails, a lost session, an expired deadline, a superseding
offer, and chunks arriving ahead of the acceptance — and the driver test
runs a whole transfer through a real temporary directory to show the bytes
land and are promoted. Refusals are asserted by *reason*, not merely as
refusals: no grant, no spool, no room, and too large are four different
answers a sender acts on differently (NFR-3).

That last one asserts *structurally*: arrival positions and frame counts,
never elapsed time. A wall-clock latency bound on a loaded CI runner
measures the runner, so the guarantee is stated as "everything still queued
arrives after the input frame, and nothing is dropped" rather than as a
number of milliseconds. Numeric latency belongs in §4's measurement, not in
a gate.

Injected clipboard contention must cover **both** retry phases
([ADR 0005](adr/0005-clipboard-transaction-flow.md), addendum
2026-09-01), because a hold the fast phase absorbs and a hold that
outlives it are different faults and only the second one lost items on
hardware. A contention scenario that never exceeds `max_attempts` proves
the blip case and nothing else — so the hermetic gate
(`stress::sustained_contention_still_delivers_every_item`) injects a hold
past the fast budget on a fraction of its items and asserts, **from the
engine's own `clipboard_installs_parked` counter**, that the parked phase
was actually reached. That distinction is the point: a harness that
asserts on its own bookkeeping proves the injection was scheduled, never
that the code path it was aimed at ran. The read half needs the same treatment and one
thing more:
`a_reconnect_re_announce_survives_a_contended_clipboard`
fails a reconnect's re-announce read for longer than
the fast nudges *and never changes the clipboard again*, which is the only
shape that catches a read waiting on a notification that will not come.

Fault injection is the primary evidence for the reliability requirements
(FR-6.x) and clipboard guarantees (FR-3.x).

### 1.6 Platform tests (Windows)

Separated from cross-platform logic tests; exercise real Win32 behavior:
clipboard observation/update/contention, sequence-number change detection,
input injection (including UIPI-blocked targets — detectably failing),
monitor enumeration and DPI mapping, cursor positioning, hook installation
and hook-loss recovery, shutdown cleanup.

Some of these need real interactive sessions and run on dedicated Windows
runners or manually per release, not in every headless CI job — but they
exist as automated tests, not checklists.

**Three exceptions, and they are honest ones.** Clipboard images (ADR 0014)
have a part no automation can reach: what a *third-party* application
publishes, and whether one accepts what Crossover installs. Automation
covers everything either side of that — a fabricated DIB round-trips
through Win32 verbatim, canonicalizes to a stable length, and is refused
above the ceiling. The third asks the *system* a question rather than the
code: whether Windows honours the clipboard-history and cloud-sync
exclusions a virtual file list declares (F16), which only a human with
Win+V and a second signed-in machine can answer. All three are
`#[ignore]`d and run deliberately:

```
cargo test -p crossover-platform-windows -- --ignored manual_a_real_snip
cargo test -p crossover-platform-windows -- --ignored manual_an_installed_image
cargo test -p crossover-platform-windows -- --ignored manual_the_offer_stays_out
```

| Test | What the human does |
|------|---------------------|
| `clipboard::tests::manual_a_real_snip_is_read_as_a_stable_image` | Take a snip (`Win+Shift+S`) **before** running; the test asserts the Snipping Tool's own DIB reads as an image, sits inside the ceiling, and yields identical bytes on consecutive reads |
| `clipboard::tests::manual_an_installed_image_pastes_into_other_applications` | Run it, then paste (`Ctrl+V`) into Paint, Word, and a browser compose box, and confirm the gradient appears in each |
| `virtual_file::tests::manual_the_offer_stays_out_of_clipboard_history_and_cloud_sync` | Run it, then check three things it prints: the item does **not** appear in Win+V; pasting into a folder produces the file with its content, opening without a Protected View or SmartScreen prompt, but with `ZoneId=1` in its `Zone.Identifier` stream; and a second machine on the same Microsoft account with clipboard sync on does not see it. The last two are the invariant-7 half — a "yes" there is a finding, not a nuisance |

Both are also on the two-machine list in [SOAK.md](SOAK.md), where the
interesting version is the same paste after the image crossed the wire.

Note that every test in this file that drives the real clipboard is
serialized behind one process-wide lock and tolerates `Busy`, because the
clipboard is a machine-global lock any application may hold. A failure
that reports `OpenClipboard ... Access is denied` on *every* such test —
including ones this change did not touch — is the desktop, not the code;
reproduce it on a clean checkout before debugging it.

## 2. CI

GitHub Actions builds and tests on **Windows, Linux, and macOS from
Phase 0**, before non-Windows platform code exists — this keeps
platform-independent crates honest (NFR-4).

Required checks:

```
cargo fmt --check
cargo clippy --workspace --all-targets   (warnings denied)
cargo build --workspace
cargo test --workspace
```

The `dependencies` job enforces the dependency policy with `cargo-deny`
(`deny.toml`): RustSec advisories, license compatibility against an
explicit permissive allow-list, and crates.io-only provenance. Unlike
coverage it **gates**, because a vulnerable or unmaintained dependency
is a defect with a known fix. It also runs on a daily schedule, since a
new advisory can land against unchanged code. Locally: `cargo deny
check`.

CodeQL static analysis runs in its own workflow (`.github/workflows/
codeql.yml`) over Rust, the workflow files, and the soak script. It uses
advanced setup because GitHub's default setup does not offer Rust. Its
expected yield is modest by design — safe Rust prevents most of what
CodeQL hunts for — so it targets where the safety argument is weakest:
the Win32 FFI, whose correctness rests on hand-written SAFETY comments,
and the workflows, where script injection is a real supply-chain class.
It is kept out of `ci.yml` so an analyzer problem cannot redden the
merge gate.

GitHub's own services cover what a build-time check cannot. Dependabot
alerts and security updates watch the GitHub Advisory Database and open
fix PRs when an advisory lands against unchanged code; `cargo-deny`
blocks the merge, Dependabot proposes the upgrade. Version updates
(`.github/dependabot.yml`) additionally keep SHA-pinned actions current,
which is otherwise invisible until a runner deprecation warning appears
in a log. Secret scanning with push protection is enabled on the
repository — relevant here because Crossover handles key material, and a
committed private key is unrecoverable once pushed.

The `fuzz-smoke` job runs every fuzz target briefly on each change.

The `coverage` job measures line/region/branch coverage with
`cargo-llvm-cov` on Windows (the only leg where the real-OS platform code
runs), publishes the browsable HTML as a build artifact, and prints the
per-file summary to the job page. Locally:

```
cargo llvm-cov --workspace --html      # target/llvm-cov/html/index.html
cargo llvm-cov report --summary-only   # per-file table
```

Coverage is **reported, not gated**. A percentage threshold rewards tests
written to move a number; the useful question is which paths a change
leaves unexercised, which the report answers directly. Low branch
coverage in error-handling code is a standing invitation to add
fault-injection cases (docs/TESTING.md §1.5), not a build failure.
Added as the project matures: documentation build, MSRV check, protocol
compatibility tests across supported versions.

## 3. Phase-gate testing

Exit criteria in [ROADMAP.md](ROADMAP.md) are verified by automation where
stated — notably Phase 2's clipboard stress gate: **≥10,000 automated
bidirectional clipboard updates with zero corruption, zero sync loops, zero
silent failures, zero crashes**, and a diagnostic for every ultimately
failed update.

## 3.1 Two-machine soak

The stress gate is hermetic by design. Real clipboards, a real network,
and two independent machines are covered by the manual soak in
[SOAK.md](SOAK.md), whose output is a report to interpret rather than a
build verdict — a live desktop can always interfere, and a red build
that says nothing about Crossover is worse than no build at all.
`tools/soak-report.py` summarizes the structured logs from both sides.

## 3.2 The layout editor: what a human still has to look at

`crossover-layout` is a window, and ADR 0019 chose egui so that almost none
of it needs one: every screen the editor can show — the empty states, the
drawn scene, the snap guides, the blocking and warning diagnostics, the
Save button's enabled state, the size panel (its empty state, what it says
about the selected screen, and how it refuses an entry), and the
unsaved-changes dialog — is asserted headlessly in `cargo test` on all
three OSes, through the same `render::draw_frame` the real window calls
(`apps/crossover-layout/src/test_support.rs`). The snap arithmetic, the
rigid-group drag, the size override and the re-pack it triggers, the
scene→`Layout` round trip, and the revision assignment are ordinary unit
and property tests beside them.

What is left is what a headless pass structurally cannot answer: whether it
*looks* right, whether it *feels* right, and whether the file it writes
actually changes what the worker does. This is that list, run by hand per
release and after any change to the editor.

| # | Check | What a pass looks like |
|---|-------|------------------------|
| E-1 | **Opens crisply on a mixed-DPI desk.** Start `crossover layout` with the window on a 100% monitor, then drag the window onto a 150%/200% one and back | Text and rectangle strokes stay sharp on both, and the arrangement rescales to fit without stretching. This checks that the OS's DPI change reaches egui — the aspect ratio itself is `Viewport::fit`'s, and already proven |
| E-2 | **Two physically-equal screens draw equal.** With a 4K monitor at 150% beside a 1080p at 100% — both the same size on the desk — look at the two rectangles | They are the same size. A 4K screen drawn twice its neighbour's is the seeding path broken: for measured panels because equal panels measure equally, and for unmeasured ones because the DIP fallback divides out the scale (ADR 0018) |
| E-2c | **Physically different screens draw different, and guesses say so.** On a desk mixing panels of genuinely different sizes — a laptop's built-in screen beside an external monitor is the ordinary case — and including, if there is one, a screen the OS cannot measure (a virtual, remote, or non-PnP display), look at the whole canvas | The rectangles are in the proportions of the real screens, not of their pixel counts: a 13" laptop panel beside a 27" monitor draws roughly half its height, where before this it drew the same size or larger. Any screen whose physical size could not be read — or whose reported size is not one a real panel could have — is captioned `(size estimated)` and is drawn at a believable size beside its neighbours rather than as a sliver or a wall. On a desk where *nothing* on either machine can be measured, no rectangle is captioned that way and the picture is the one this editor always drew. Nothing is blocked, and Save stays available (ADR 0018, addendum 2026-08-22) |
| E-2b | **Screens are captioned by the name on the bezel.** Look at every rectangle, on both machines' groups, on a desk that mixes monitors the OS can name (an ordinary external display) with ones it cannot (a virtual, remote, or non-PnP display) — and, if two identical monitors are attached, at that pair | A named screen reads `DELL U2720Q` — its EDID product name, the string Windows Settings shows for it, not `\\.\DISPLAY1`. A laptop's built-in panel reads `Internal Display` (its EDID has no name, so we substitute our own English constant rather than reproducing the shell's localized one). Anything else the OS will not name — virtual, remote, non-PnP — falls back to its device string rather than showing a blank or a placeholder. Two identical screens on **one** machine read `… (1)` and `… (2)`, so the pair is still tellable apart; the same model on the *other* machine is not numbered along with them. The ordinal and resolution stay on every caption. The rule itself is unit-tested (`caption.rs`); what needs eyes here is whether the platform hands over the string the user recognises, and whether it still fits inside the rectangle at ordinary zoom |
| E-2d | **A wrong size can be corrected by hand.** Click any screen, read the size panel on the right, then type a different width in millimetres and apply it (`Enter` in either field, or the Apply button) | Clicking selects: the rectangle is ringed on the canvas and the panel names *that* screen — the same caption the rectangle carries, its pixel resolution, and its drawn size in millimetres. With the proportion lock on (its default), typing a width fills the height in the screen's current ratio; unticking it allows a free pair. Applying redraws the rectangle at what was typed, closes the seam beside it again (the screens beside it **in that row** shuffle along, none overlapping, the machine staying where it was dragged — a screen sitting on a row of its own, such as one plugged in since the arrangement was saved, does not move at all), lights **Unsaved changes** and the Save button, and — on a screen that was captioned `(size estimated)` — removes the caption, because a stated size is not a guess. Saving writes it like any other arrangement |
| E-2e | **A refused size says why, and draws nothing.** With a screen selected, enter `10`, then `9000`, then `about a foot` | Each is refused in the panel, in the panel's own words, naming the 50–3000 mm range a panel can be. The rectangle does not change, the Save button does not light, and nothing is clamped to the nearest legal size behind your back |
| E-2e2 | **Apply on untouched fields does nothing.** Select a screen, touch nothing, and press Apply (or Enter) | The panel says it is already drawn at that size. The rectangle does not move, the Save button does not light, and no screen shuffles — including on a screen whose drawn size is not a whole number of millimetres, which is every screen the editor had to size from pixel counts |
| E-2f | **The detected size can be got back.** On a screen the machine *could* measure, override its size, then press **Use detected size**; then select a screen captioned `(size estimated)` | The first snaps back to the size the machine detected, says so, and re-packs the machine. On the estimated screen the control is greyed out — there is no measurement to return to, which is exactly what the caption was saying |
| E-2h | **A correction leaves the rest of the machine alone.** On a machine with a saved arrangement, plug in a display the saved arrangement does not name (it is drawn below the placed ones, marked `unplaced`), then correct the size of one of the *placed* screens | Only the corrected screen and the ones following it in its own row move. The unplaced screen below does not shift, the placed row's seams stay closed, and correcting the size back returns every rectangle to exactly where it was |
| E-2g | **A correction is not undone by the once-a-second re-read.** Override a screen's size (do not save), then wait ten seconds without touching anything; repeat with a screen whose size the machine *does* report, and again after saving and reopening the editor | The corrected rectangle stays corrected throughout — the editor re-reads the worker's facts every second and must not redraw a size the user stated, any more than it may move a machine back where it was dragged. After a save it comes back at the corrected size on a fresh editor, because what was written is the rectangle itself |
| E-3 | **Drag and snap feel right.** Drag one machine's group toward the other, slowly, from several units out; repeat zoomed in and zoomed out (resize the window) | The whole group moves rigidly; the snap catches about a pointer's width from the seam *at either zoom*; guides appear as it catches and the status bar names what caught (`Snapping …: edges meet`); nothing jitters, creeps, or slides away from the cursor while held |
| E-4 | **Empty states against a real worker.** Close the editor, stop the worker (`crossover service stop`, or end `crossover run`), delete `~/.crossover/state/topology.json`, reopen the editor — then start the worker with the editor still open | The empty state names `crossover run` and `crossover service install`; within a couple of seconds of the worker starting, the canvas fills in on its own with no restart. Stopping the worker again leaves the last-known arrangement on screen marked `not responding`, rather than blanking |
| E-5 | **Unsaved-close confirmation.** Drag something, then close the window | The dialog appears. **Cancel** leaves the window open with the drag intact; **Discard** closes and `config.toml` is unchanged; **Save and close** writes, then closes |
| E-6 | **A save that cannot happen says why.** Make `~/.crossover/config.toml` unparseable (a stray `[`), then save | The status bar names the whole chain (`writing the config file failed: the existing config file is not valid TOML…`) and the file is left exactly as it was |
| E-6b | **A save is not visibly undone while the worker catches up.** Drag something, save, then watch the canvas for five seconds without touching it | The arrangement stays exactly where it was saved. The editor re-reads the state file once a second and the worker only picks the edit up on its own ~2 s config poll, so for a few seconds the state file still describes the *old* arrangement — a snap-back in that window is indistinguishable from a save that silently failed, which is what `session.rs`'s post-save hold exists to prevent. The status bar keeps reporting the worker's real state (running/not responding, peer connected or not) throughout |
| E-6c | **A display change lands while an edit is unsaved.** Drag something without saving, then plug in or unplug a monitor | The new screen appears within a second or two, alongside the arrangement the drag left — not after a save, and not instead of it. An unplugged one disappears the same way |
| E-7 | **The loop closes: save → worker re-read → crossing matches the drawing.** With both machines running, draw an arrangement — including at least one case the side model could not express (a seam between two of *one* machine's own monitors, or an over/under placement) — save, and watch the worker's log | The worker adopts the new revision within a few seconds and with no restart, and the cursor then crosses **where the drawing says it does**, and nowhere else |

E-7 is the interesting one, and it is deliberately **the soak's job** rather
than this file's: it needs two machines, two desks' worth of real monitors,
and a link, which is exactly what [SOAK.md](SOAK.md) is for and where the
Phase 8 exit criteria are signed off. It is listed here so a release
checklist run on a single machine knows it has *not* covered it.

## 4. Performance measurement

Numeric latency targets are established by measurement (NFR-5).
Instrumentation (tracing spans + local counters) measures: network RTT,
message latency by class, clipboard transaction latency, input queue depth,
reconnect duration, control-transfer latency. Measurements are local only
(FR-7.5).

## 5. Definition of Done

A feature is complete only when every applicable item holds:

- requirements it implements are identified (FR/NFR numbers) and documented
- implementation complete; errors handled; security implications considered
- diagnostics exist where §FR-7.3 demands them; no sensitive data logged
- unit + property tests pass; integration and fault-injection tests updated
  and passing; platform tests where applicable
- CI green on all three OSes
- documentation current; ADRs written where required
- no unrelated architectural changes smuggled in
