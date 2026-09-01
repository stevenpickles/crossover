# Changelog

Notable changes to Crossover. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) — with
the pre-1.0 caveat that the wire protocol and on-disk formats may still
change between minor versions. Protocol compatibility is negotiated per
session and versioned separately (`PROTOCOL_VERSION` in
`crossover-protocol`); a release note says so whenever that number moves.

Builds that are not tagged releases identify themselves as such —
`0.2.0-dev.7.gabc1234` — and say where they came from. Run
`crossover version` on any binary to see exactly what it is.

## [0.2.0] — 2026-09-01

Two things the first release could not do. Files and folders now travel on
the clipboard, and the screen arrangement is something you **draw** rather
than something you declare with `--left` or `--right`. The wire protocol
moves from 2 to 6 over the course of this release: **both machines must be
upgraded together**, and a mixed pair refuses cleanly at the handshake
rather than establishing a session that dies later.

### Added

- **Files and folders travel between the two machines.**
  ([ADR 0015](docs/adr/0015-spooled-virtual-file-paste.md)) Copy a file, a
  folder, or a multi-file selection in Explorer on one machine and paste it
  on the other. A single file arrives verbatim under its own name; a folder
  or a multi-entry selection arrives as one Stored — never compressed —
  archive. The bytes are streamed a chunk at a time straight through to a
  spool directory and never held in memory on either side, which is why a
  file may be 256 MiB where an image is capped at 64. Nothing is offered for
  paste until it has been received whole and its SHA-256 and length match
  what was offered; every other outcome deletes the partial and registers
  nothing. `AlreadyHave` dedup applies here as it does to images, so
  re-copying something the peer already holds costs one offer and one
  decline. Validated on two machines over a wired link on 2026-08-19,
  byte-identical in both directions.

- **A peer may only write files to your disk if you say so.**
  `file_receive` is a per-peer grant that is **off by default** — pairing
  does not set it — granted explicitly with `crossover peers allow-files` /
  `deny-files` and printed for *every* peer by `crossover peers`, granted or
  not: a permission you can only see when it is on is one you cannot audit.
  The sending side has its own flag, `clipboard_send`, but it is a different
  thing and it is worth being exact about: pairing **does** set it, so it
  defaults to *on*, and this release enforces it for **files only** — a peer
  without it is refused before a selection is walked, while text and images
  still travel without consulting it at all (see Known limitations).
  Revoking either reaches a running worker
  within a single trust-store poll, exactly as active-session revocation
  already did, and re-pairing drops a standing `file_receive` rather than
  carrying it forward: pairing is not consent to a filesystem write surface.
  That grant can never arrive by upgrade either — the trust store moves to
  format 2 with a frozen version-1 decoder, and the new flag is written as a
  literal `false` rather than read from any byte of an older store.

- **A pasted file says where it came from.** It carries a
  `Zone.Identifier` marking it **Local intranet** (`ZoneId=1`) — the
  accurate description of bytes that arrived from a paired machine on your
  own network — so an ordinary document opens without a Protected View
  banner or a SmartScreen challenge on every paste between your own
  computers. Crossover still never opens, launches, previews, or
  shell-associates anything it received.

- **Every refusal a file transfer can make is typed, bounded, and visible.**
  Entry count (256), archive depth (32), and cumulative bytes are judged
  *during* the walk, from what was actually read, so a file growing under
  the walk cannot write past the cap; a symlink, junction, or unreadable
  entry refuses the **whole** selection rather than quietly sending
  something other than what you chose; and a refusal is a log line naming
  the reason, not silence. The temporary artifact is delete-on-close, so no
  refusal path — and no crash mid-build — leaves one behind.

- **You draw the arrangement of your screens.**
  ([ADR 0018](docs/adr/0018-drawn-display-topology.md),
  [ADR 0019](docs/adr/0019-layout-editor-toolkit.md)) `crossover layout`
  opens an editor showing both machines' monitors, to scale, in one shared
  space. Drag them into the arrangement they have on your desk — rectangles
  snap so seams close exactly — and save. Crossing edges are then derived
  from that drawing rather than from a single `--left`/`--right` side, which
  means **a seam between two monitors of the same machine**, **a corner
  where three monitors meet**, and **part of an edge that is a wall** are
  all expressible for the first time. Crossing stays proportional through
  the drawn geometry, so mixed DPI cancels: leaving one screen 40% of the
  way up an edge arrives 40% of the way up the other. The editor is its own
  on-demand, plain-integrity binary — `crossover-layout.exe`, which the
  packages install **beside** `crossover.exe` — that the background service
  never touches and that links none of the network, TLS, or protocol code.

- **The arrangement is one arrangement, shared by both machines.** A layout
  saved at either desk travels over the session with a revision, and the
  newer revision wins — ties broken by origin device id and then by a
  SHA-256 of the layout's own content, so two machines always converge on
  one answer. The loser says so by name, in the log, with both revisions and
  both origins. An adopted layout is persisted before it is applied and
  takes effect without a restart on any machine that already holds a drawn
  arrangement.

- **The worker publishes what it knows, so the editor can draw it.** A state
  file at `~/.crossover/state/topology.json` carries both machines' live
  monitors, the current layout, and a heartbeat; it is written atomically
  and coalesced, and the last known monitors survive a transient enumeration
  failure rather than blanking the desk. The editor reads it and says
  honestly why it cannot draw when it cannot — "the worker is not running",
  "waiting for the peer", or "unsupported version" — instead of an empty
  window. The worker re-reads the config the editor writes, so a saved
  arrangement takes effect without stopping anything.

- **The layout editor names monitors the way you do.** A rectangle is now
  captioned with the monitor's own name — `DELL U2720Q`, the string
  Windows Settings shows for it — instead of its device string
  (`\\.\DISPLAY1`), for both machines' screens. A laptop's built-in panel,
  which has no such name to read, reads `Internal Display`. Anything else
  the OS will not name still shows its device string, and two identical
  screens on one machine are numbered `(1)` and `(2)` so the pair stays
  tellable apart. The name is a caption only: arrangements, saved layouts,
  and where the cursor crosses all still address a monitor by its device
  string, unchanged.

- **The layout editor draws your screens in their real proportions.** A
  rectangle is now seeded from the panel's actual size in millimetres, so a
  13" laptop screen beside a 27" monitor is drawn about half its height
  instead of the same size — and the arrangement you draw is the
  arrangement you experience, since a cursor crosses at the height the
  drawing says it does. Screens whose size cannot be read (a virtual or
  remote display, a panel with no readable EDID) — or whose reported size
  is not one a real panel could have — are drawn from their pixel counts,
  scaled to sit believably beside the screens that could be measured, and
  captioned `(size estimated)` so it is clear which rectangles are guesses.
  Where *nothing* on either machine can be measured, every rectangle is
  drawn from pixels exactly as before and nothing is captioned, since there
  is no difference to point out. Only *new* rectangles are sized this way: an
  arrangement you have already drawn and saved is never rescaled behind
  your back, and a rectangle you are in the middle of dragging is never
  resized under the pointer.

- **You can correct a screen's size in the layout editor.** Click any
  screen and a panel on the right names it, shows its resolution, and gives
  its drawn width and height in millimetres to edit. Type the real size of
  a screen your machine measured wrongly — or could not measure at all —
  and the rectangle is redrawn at it, with the screens beside it in that row
  shuffling along so the seams stay closed — screens elsewhere on the
  machine, such as one plugged in since the arrangement was saved, stay
  exactly where they are. Editing one dimension
  fills the other in the screen's current proportions unless you untick the
  lock. A size no panel could be (under 50 mm or over 3000 mm on a side) is
  refused with a note rather than quietly rounded into range, and **Use
  detected size** puts a rectangle back on the size your machine reported —
  greyed out for a screen captioned `(size estimated)`, since there is no
  measurement to go back to. A size you have stated stops being captioned
  as a guess, is not undone by the editor's once-a-second re-read of the
  worker, and is kept by saving the arrangement like any other change: the
  correction *is* the rectangle, so there is nothing else to store and
  nothing new on the wire.

- **Monitors report how big they physically are.** Each machine now reads
  the real width and height of every attached panel — in millimetres, off
  the monitor's own EDID — and reports it to the other machine and to the
  layout editor's state file, which is what the editor's proportional
  drawing (above) is built on. A screen whose size cannot be read or does
  not look believable (a projector, a virtual display, a remote session)
  simply reports none, and is drawn from its pixels as described above. The
  measurement is proportion only — arrangements, saved layouts, and where
  the cursor crosses all still address a monitor by its device string.

- **The background service keeps its own durable log.** `crossover-svc`
  runs as `LocalSystem` and had been logging worker supervision to a stderr
  nobody could read, so a worker that died left no trace of why. It now
  writes a daily-rotating file under `%ProgramData%\Crossover\logs`
  recording every transition it already drove: worker launched, worker
  exited with its exit code in decimal *and* hex plus a crash
  classification, a service-initiated stop **and why** (service stopping,
  logoff, session change), launch failures, the backoff delay chosen, and
  which SCM control fired. `crossover service install` and `crossover
  service status` print the path, so it is discoverable without already
  knowing it exists.

### Changed

- **`--left` and `--right` are deprecated in favour of the drawn layout,
  and the configuration file moves to schema version 2**
  ([ADR 0018](docs/adr/0018-drawn-display-topology.md)). An arrangement
  lives in a `[layout]` section of `config.toml` and is what a running
  worker crosses by. Nothing you already have stops working: a v1 file, or a
  lingering `[seamless] side`, loads as an implicit layout whose behaviour
  is byte-for-byte what it was, and the flags still work while warning that
  they are deprecated. An explicit `[layout]` beats the flags — the one
  place this project's usual CLI-wins rule is deliberately reversed, because
  the drawing is the more specific statement. A semantically invalid
  `[layout]` produces a loud diagnostic and no layout rather than a fatal
  error, so a service-launched worker cannot relaunch-loop over a typo in a
  config file.

- **Wire protocol moves to version 6, and accepts nothing older**
  ([ADR 0018](docs/adr/0018-drawn-display-topology.md), amended 2026-08-21
  and 2026-08-22). Each monitor a machine reports now carries two further
  optional facts about itself, added since version 4: its **product name**,
  so the *other* machine's editor can caption the rectangle, and its
  **physical size**, so that editor can draw it in proportion to the real
  screen. Each adds a byte to every monitor of every report, and no feature
  bit can hide either. **Both machines must be upgraded together**; a mixed
  pair refuses cleanly at the handshake naming both version ranges, rather
  than establishing a session that dies on the first report. Nothing about
  crossing changed — both fields are display-only — but the bytes are on
  the wire regardless, and it is the bytes that force the bump.
- **Wire protocol moves to version 4, and does not accept version 3**
  ([ADR 0018](docs/adr/0018-drawn-display-topology.md)). Crossing control
  now carries an `EntryPoint` — destination monitor, edge, fraction, and
  layout revision — where it used to carry a bare fraction, a structural
  change to messages that already travel between every pair of peers,
  which no feature bit can hide, so **this build cannot connect to a v3
  peer and a v3 peer cannot connect to this build**. Both machines must be
  upgraded together. The failure is a clean refusal at the handshake
  naming both version ranges, not a session that dies later; `crossover
  version` reports the range a build speaks.
- **Wire protocol moves to version 3, and does not accept version 2**
  ([ADR 0017](docs/adr/0017-protocol-version-3.md)). A file descriptor on the
  clipboard offer adds a byte to *every* offer, which no feature bit can
  hide, so **v0.1.0 cannot connect to this build and this build cannot
  connect to v0.1.0**. Both machines must be upgraded together. The failure
  is a clean refusal at the handshake naming both version ranges, not a
  session that dies later; `crossover version` reports the range a build
  speaks.

- **`FILE_CLIPBOARD` is advertised.** The build now negotiates the file
  feature bit, which is the act that lets either half of the file path be
  reached at all; before it, a conforming peer negotiated files away. A
  session that does not negotiate it, or a peer without the matching grant,
  is refused *before* a selection is walked — the walk and the archive are
  the expensive, irreversible step, so nothing is spent to learn what a bit
  already answered.

- **The trust store moves to format 2, and a downgrade is a hard stop.**
  Once a build with this release has written the store, an older binary
  refuses to start rather than misreading a file whose fields have moved.
  This is the correct direction to fail in — the alternative was a store
  that could hand a filesystem-write grant to a peer nobody granted it to —
  but it does mean the two machines cross this line together.

- **Development builds identify as `0.2.0-dev.N` and can be installed over a
  release.** Every build made after `v0.1.0` shipped called itself
  `0.1.0-dev.N`, which SemVer ranks *below* the release already installed,
  so `choco upgrade` correctly refused it as a downgrade and there was no
  way onto the soak machines short of `--force`. The version now lives once
  in `[workspace.package]` and every crate and the nuspec inherit it, so two
  binaries can no longer ship claiming different versions.

### Fixed

- **A contended clipboard no longer costs you the item.**
  ([ADR 0005](docs/adr/0005-clipboard-transaction-flow.md)'s 2026-09-01
  addendum) On hardware, at **five of eight peer reconnects**, something
  external held the machine-global clipboard lock for about a second — a
  hair longer than the install's 5 × 200 ms budget — and every one of those
  re-announced items was dropped for good. The fast phase is unchanged, for
  the millisecond blip it was designed against; past it a `Busy` install is
  now **parked** rather than failed, retried on a 1 s cadence and on the
  settle read that follows a change notification, for up to 20 s. The
  read decides, not the notification, because on Windows a notification
  usually means the *user* just copied something. Three things outrank a
  parked install, each now answered with a typed verdict rather than
  dropped in silence: a genuinely new local copy — the user beats the peer
  — a newer inbound item, and the session ending. The read side had the
  same fault and a latent one behind it: the busy back-off reset only on a
  *successful* read, so one contended episode left every later busy read
  with no nudge scheduled at all, and a read parked "until the next change
  notification" waited for a notification that never comes when the
  clipboard already holds what was copied while the peer was away — which
  is precisely what lost the reconnect re-announce. The counter now resets
  on a successful read, on a genuine notification, and on session
  establishment, and the driver revives on the same 1 s cadence instead of
  going quiet. The whole budget is a worst case of ≈22 s against the
  origin's 60 s patience, pinned by a test so raising either alone fails
  the build, and two new counters keep the difference legible:
  `clipboard_installs_parked` (nearly lost) beside
  `clipboard_installs_failed` (lost).

- **A busy clipboard now says who is holding it.** An `OpenClipboard` or
  `OleSetClipboard` failure used to shrug — "clipboard held elsewhere?" —
  which could equally have meant Clipboard History, a password manager, an
  RDP client, or Crossover's own OLE apartment thread racing itself.
  Establishing which took a manual correlation nobody should have to do.
  The `Busy` reason now names the holder: `held by pid 1234 "SomeApp.exe"
  (window class "Foo")` for another application, `held by this process
  (pid …, thread …, window class …)` for in-process contention, and `held
  by an unidentified owner (no window)` when Windows associates no window
  with the open. The first busy episode per item is logged at `warn`
  rather than once per retry at `debug`, so a contended second is visible
  without turning trace logging on. Never the window *title* and never a
  full executable path — a title can carry document content and a path can
  carry a username; a class name and a bare file name cannot.

- **A copy made while the peer is away is held, not abandoned.**
  ([ADR 0006](docs/adr/0006-clipboard-transmission-triggers.md)'s
  2026-09-01 addendum) The engine minted a deadline-bound transaction for
  every local change whether or not any session existed, so the frame went
  out to an empty sink list and sixty seconds later logged `outbound
  clipboard transaction abandoned` — **twenty of them** over one eight-hour
  evening with the peer asleep. Nothing was broken, but `clipboard_abandoned`
  is the only signal for a class of silent stall, and ordinary offline
  evenings made it unreadable for the thing it exists to find. A local
  change with no live session is now observed, hashed, and recorded exactly
  as before — loop suppression and dedup untouched — and then stops: no
  outbound slot, no timer, no frame. It is counted as
  `clipboard_offline_changes`, and the item that is current when a peer
  arrives is offered whole. Liveness is a **count**, not a flag, because
  this process can hold two sessions at once and a boolean would be
  cleared by the first of them to drop — a clipboard that quietly stops
  working is worse than the fault being fixed. `clipboard_abandoned` now
  means what it was meant to mean: a peer was there and did not answer.

- **Seamless edge transfer no longer bounces at the seam.** Crossing onto
  the other machine parks its cursor exactly on the return-trigger column,
  and a two-pixel hand tremor at 125 Hz was enough to fire a complete
  reverse transfer — which re-parked both cursors on their triggers, so it
  repeated: ten take/revoke cycles in five seconds, observed on hardware.
  The detector is now a Schmitt trigger: a crossing fires only when armed
  *and* touching, and only an observation more than 24 px clear of the
  linked column re-arms it. A deliberate crossing gains no latency — it is
  far clear of the edge on the way in, so its first touch of the column
  still fires — and entry placement is unchanged.

- **A late control answer now corrects itself instead of locking both
  machines out.** On hardware, one answer arrived 4.7 s late and the two
  state machines denied each other for about seven seconds: the requester
  had timed out silently, so a grant stood with nobody believing they held
  it, and the second request was met with `Denied(AlreadyControlled)` —
  denying the very session holding the grant. Three convergence rules fix
  it, all following from message order alone with no clock and no heuristic:
  a re-request from the grant holder **refreshes** the grant (draining every
  key it left held, so a refresh can no more inherit a latched key than a
  hand-back may leave one), a timed-out request **cancels on the wire**, and
  the stray-grant undo yields to our own retry. A request from any *other*
  session is still denied — that distinction is the security boundary and is
  unchanged.

- **An interactive frame is never delayed by a queue it has nothing to do
  with.** Every decoded frame used to be handed to *both* the clipboard
  driver and the control driver and awaited on both, from a strictly serial
  per-session pump — so a `ControlRequest` completed only once a saturated
  clipboard queue had accepted a frame it was about to discard, and the
  125 Hz input stream paid the same toll. Frames now route by message type
  to exactly one driver, with same-driver ordering untouched. Related: the
  edge mode is a level on a `watch` rather than a queue, which removes the
  feedback cycle behind the stall-then-burst signature in the hardware logs,
  and a refreshed grant now re-primes the detector — without which a refresh
  could revoke the grant it had just re-issued.

- **A disconnect now says whether the wire went down on *this* machine.**
  A chronically flapping NIC made both peers log `An existing connection
  was forcibly closed by the remote host`, which was false at both ends —
  nobody closed anything, the local link went down and the OS reported the
  only thing a socket can see. Establishing that took a manual correlation
  of Windows event-log NIC events against two machines' logs. Crossover now
  asks the platform whether the local interface that carries *this peer's*
  traffic is up, and carries the answer on the session-end and
  connect-failure records: `local_link="down"` comes with wording telling
  you to disregard the misleading error above it. Asked per peer, because a
  machine with live Wi-Fi and a dead dock still routes this session over the
  dead one, and three-valued, because "could not tell" is a common answer
  and collapsing it into "up" would manufacture the false confidence being
  removed.

- **A peer can no longer hold a session hostage by stalling in bursts.** The
  write-stall bound cleared its run on any brisk write, so alternating one
  six-second stall with one one-millisecond write kept every frame inside
  the per-write deadline and the session was reported healthy while being
  useless. A slow write now charges its whole duration to a leaky bucket
  that brisk writes and idle gaps pay back at the rate they earned, and the
  session ends when the debt reaches the keepalive timeout. A *continuous*
  stall fills the bucket exactly as fast as the old measure caught it, so
  nothing caught then escapes now.

- **The long-open "worker seen not exiting after a clean shutdown"
  observation is explained.** With the service's durable supervision log
  running it recurred on 2026-08-20 and read as exit `0x40010004`
  (`DBG_TERMINATE_PROCESS`) following a session-change notification, then
  `reason=Logoff` — Windows tearing the worker down at user logoff.
  Environmental, not a crash, and no longer carried as an unexplained
  defect. Two cosmetic residuals of that same logoff sequence remain and are
  listed under Known limitations, because explaining a signature is not the
  same as making the log read correctly.

### Known limitations

- **The drawn topology has not completed its two-machine soak.** Every
  Phase 8 exit criterion is exercised by the test suites and by
  single-machine editor checks; the hardware passes in
  [docs/SOAK.md](docs/SOAK.md) have not been run against this build. The
  roadmap's current-phase marker says so too, and stays at Phase 8.
- **A copy in a format Crossover cannot read can be overwritten by a
  parked install.** The read answers `Ok(None)` both when the user copied
  something in a format that is not synced — an application's private
  format, RTF-only content — and when the clipboard is simply empty, and
  the engine cannot tell those apart. It therefore treats `None` as no
  evidence either way and leaves a parked install to its timer, which
  fires within a second and writes the peer's item over that copy.
  Narrower than the defect it came out of — it needs an unsyncable copy
  inside a parked window rather than any copy at all — but equally silent.
  The fix is a provider-level change distinguishing `Empty` from
  `Unreadable`, deliberately not made inside a retry fix
  ([ADR 0005](docs/adr/0005-clipboard-transaction-flow.md)'s 2026-09-01
  addendum).
- **Windows only.** The platform boundary exists and core, protocol, and
  security crates build and test on Windows, Linux, and macOS, but only the
  Windows implementations of the platform traits are written. The layout
  editor's toolkit was chosen for the same reason — one codebase on all
  three OSes — but only Windows is implemented.
- **Two machines.** The protocol treats peers symmetrically and nothing
  assumes a hub, but more than two is untested and unsupported.
- **Binaries are not code-signed**, so SmartScreen warns on first run.
  Verify the published SHA-256.
- **No automatic updates.** Upgrading means installing the new package — and
  because the protocol floor moved, **both machines at once**.
- **`crossover-layout.exe` must sit beside `crossover.exe`.** `crossover
  layout` launches its sibling by path; a portable extraction that separates
  the two binaries has no editor.
- **Images are capped at 64 MiB and files at 256 MiB**, with at most 256
  entries in one selection, 32 levels of folder depth, and a 1 GiB spool
  budget. An oversized item is refused with a logged reason rather than
  transferred, and a zero-byte file cannot be transferred at all.
- **A paste target that cannot accept an `IStream` cannot paste a received
  file.** Contents are served only as `TYMED_ISTREAM` at index zero;
  materializing up to 256 MiB into memory on demand for any local caller is
  the exact cost delayed rendering exists to avoid.
- **`clipboard_send` is enforced for files only.** It is the first
  permission this codebase enforces anywhere; **text and images still travel
  without consulting it**, unchanged from 0.1.0. Nothing in this release was
  scoped to close that, and it is stated rather than implied.
- **Windows Cloud Clipboard still sees what Crossover writes.** Crossover
  sets no clipboard-history or cloud-sync opt-out formats on text or image
  writes and never has, so on a machine where "sync across your devices" is
  on, received text reaches the signed-in Microsoft account — the user's own
  OS feature acting on their own clipboard. Disclosed rather than
  suppressed, because silently breaking Win+V for ordinary text would be the
  worse surprise; turn it off in Windows Settings if you do not want it. The
  virtual file list *is* excluded from both, because a history entry for it
  is a promise that cannot be kept.
- **One restart before a first-ever adopted arrangement drives the cursor.**
  A run holding no drawn arrangement — a `--left`/`--right` run, or seamless
  off — adopts and persists a layout the peer sends, but has no live
  crossing source for the publication to replace, so it begins crossing by
  it at the *next* start
  ([ADR 0018](docs/adr/0018-drawn-display-topology.md)'s 2026-08-21
  amendment). It applies **once per machine, not per edit**, and it is
  logged at the moment it applies.
- **A layout whose screens are all absent goes inert rather than being
  rejected**, so an undock does not make the drawing forget your desk. While
  inert, the controlled machine cannot reclaim by crossing a span: the exits
  are genuine local input on that machine, both Control keys at the
  controller, and disconnect (the controller's console `r` is unavailable
  under the background service, which has no console).
- **There is still no inbound preemption of a genuinely saturated
  same-driver queue.** Backpressure from a saturated clipboard path reaches
  the peer, and on one ordered TCP stream that delays what the peer sent
  behind it. The narrower guarantee — an interactive frame is never delayed
  by a queue belonging to a driver with no interest in it — is the one that
  was violated and is now held.
- **Responsiveness under a saturating transfer depends on the physical
  link.** On a direct **wired** 2.5 GbE link carrying interactive input and
  bulk file data through one writer, the socket accepted an input frame's
  bytes in **0.019 ms mean, 0.147 ms worst case** over 4,558 samples — below
  the 0.21 ms [ADR 0013](docs/adr/0013-interactive-over-bulk-prioritization.md)
  costs a single 64 KiB chunk, which closes 0.1.0's failing measurement and
  settles the chunk size at 64 KiB. Over **WiFi** the same measurement was
  mean 1.94 ms and worst case 309.8 ms; the failure is attributable to the
  link, not the chunking design, but it is what a wireless pair should
  expect. One ~72 ms tail event appeared in the interactive lane while
  socket writes stayed at or below 0.147 ms — a pre-writer scheduling stall,
  one sample in 4,558, recorded as a follow-up rather than a blocker.
- **The supervision log calls a deliberate logoff termination a crash.**
  The service classifies any non-zero worker exit as `crashed=true`, so
  `0x40010004` (`DBG_TERMINATE_PROCESS`) — Windows tearing the worker down
  at logoff, the signature explained above — is logged as a crash. Cosmetic:
  nothing acts on the flag differently, but it misleads whoever reads the
  log next, which is the whole point of having one.
- **The service relaunches the worker into a dying session at logoff**,
  because it acts on the session-change notification before the `Logoff`
  stop reason arrives. Cosmetic: the relaunch fails harmlessly and the
  backoff absorbs it.
- **Injection into an elevated window may still be swallowed** by UIPI, and
  an application with its own Home/End handling still interprets forwarded
  shifted navigation its own way. Both carried unchanged from earlier
  phases.

### Security

The threat model, trust model, and security invariants remain documented in
[docs/SECURITY.md](docs/SECURITY.md), and files are the first
peer-controlled write surface onto disk, so most of this release's security
work is there. The spool is a boundary **no method of which takes a path** —
the root is resolved once by the constructor and every later operation names
a bare entry relative to an already-open handle, so "never re-resolve by
path" is a property of the signature rather than of discipline; the
junction-swap escalation against the elevated worker is run as a test, not
argued about. A peer-supplied file name is validated by a pure, total,
allocation-free, **reject-not-repair** rule — no API returns a fixed name —
and the corpus is run twice, through the validator and again through the
real decode path, because a peer does not run our encoder. Two new threat
rows are recorded rather than left to be inferred: **T22**, clipboard
history and cloud sync, marked knowingly undefended for text and images, and
**T23**, a peer-supplied layout, bounded with rate-limited narration, a
budget on answers, and violation caps at parity with the clipboard figure.
Three findings during implementation corrected the document rather than the
code — `OleIsCurrentClipboard` alone was not sufficient to recognize our own
clipboard object, the F7 cap-breach rule was overclaiming a fail-closed
termination where the code correctly charges a graduated violation, and the
spool's DACL bullet read slightly stronger than Windows permits. Parsers
that touch network input remain fuzzed on every change — the file name,
control payload, and topology message targets are new this release — and
every queue influenced by a peer is still bounded before allocation.

[0.2.0]: https://github.com/stevenpickles/crossover/releases/tag/v0.2.0

## [0.1.0] — 2026-08-16

The first release. Two Windows machines share one keyboard, mouse, and
clipboard over a mutually authenticated TLS 1.3 link, unattended.

### Added

**Secure link.** Pairing with a typed one-time code (SPAKE2,
[ADR 0002](docs/adr/0002-pairing-verification-mechanism.md)); device
identity as an Ed25519 key pinned by the SHA-256 of its SPKI
([ADR 0003](docs/adr/0003-device-identity-credential.md)); mutual TLS 1.3
on every session; a trust store that survives restarts, and revocation that
terminates live sessions rather than only refusing new ones
([ADR 0010](docs/adr/0010-active-session-revocation.md)).

**Clipboard.** Text and images synchronize in both directions. Images
travel in the source clipboard's own raster format, verbatim — no
transcode, no re-compression
([ADR 0014](docs/adr/0014-chunked-rich-clipboard-transfer.md)) — chunked so
a large transfer never becomes one unpreemptable frame. Re-offering content
the peer already holds costs one offer and one decline instead of
megabytes. Loop prevention, deterministic conflict resolution, bounded
retries on contention, and success defined as "the destination clipboard
was updated", never "the bytes were sent" (FR-3.2).

**Input.** One keyboard and mouse drive both machines. Crossing a
configured screen edge transfers control and keyboard focus with no manual
switch ([ADR 0009](docs/adr/0009-seamless-edge-transfer.md)); control
returns at the reverse edge. Keys travel by physical USB HID usage so
layouts on the two machines cannot disagree
([ADR 0008](docs/adr/0008-keyboard-key-representation.md)). A disconnect
releases every held key and button — a stuck key is treated as
release-blocking.

**Responsiveness under load.** Interactive input and bulk transfers travel
in separate lanes, and input preempts bulk between chunks
([ADR 0013](docs/adr/0013-interactive-over-bulk-prioritization.md)), so
copying a 4K screenshot does not make the mouse stutter. Every queue on the
path is bounded by messages *and* bytes.

**Unattended operation.** A minimal `LocalSystem` service launches and
supervises the worker in the interactive session
([ADR 0011](docs/adr/0011-background-service-launcher.md)); the worker runs
at high integrity so it can drive elevated windows
([ADR 0012](docs/adr/0012-elevated-worker-integrity.md)). The service
binary links no network, TLS, or protocol code at all — that isolation is
enforced by the dependency graph rather than by discipline. Crash-relaunch
uses a capped backoff, validated over a multi-day soak.

**Packaging and provenance.** `scripts/build.ps1` runs the full gate and
produces every deliverable — portable archive, checksum, Chocolatey
package, and an `artifacts.json` manifest — and CI runs the same script, so
a released artifact and a locally built one come from one code path. Every
binary carries its own identity: `crossover version [--json]` and
`crossover-svc --version` report the build version, source commit and
branch, toolchain, target, and the protocol versions the build speaks; the
same values are stamped into the Windows version resource.

**Diagnostics.** Structured logging to a rolling file under
`~/.crossover/logs`, which is what makes a headless service-launched run
diagnosable at all. Execution statistics — frames and bytes by class,
session lifetimes, clipboard outcomes and latency, input queue-to-wire
latency — are written every fifteen minutes and again at shutdown, so a run
leaves its numbers behind however it ends. Panics reach the log rather than
a `NUL` stderr.

### Known limitations

- **Windows only.** The platform boundary exists and core, protocol, and
  security crates build and test on Windows, Linux, and macOS, but only the
  Windows implementations of the platform traits are written.
- **Two machines.** The protocol treats peers symmetrically and nothing
  assumes a hub, but more than two is untested and unsupported.
- **Files and folders do not travel.** Designed in
  [ADR 0015](docs/adr/0015-spooled-virtual-file-paste.md) and deliberately
  not implemented: it is the first peer-controlled write surface onto disk,
  and the design is still under consideration.
- **Binaries are not code-signed**, so SmartScreen warns on first run.
  Verify the published SHA-256.
- **No automatic updates.** Upgrading means installing the new package.
- **Images are capped at 64 MiB** and oversized captures are skipped with a
  log line rather than transferred.
- **Input latency under a saturating bulk transfer does not meet its own
  criterion.** Measured on hardware over **WiFi**: mean 1.94 ms, worst case
  309.8 ms, against a design expectation of tens of microseconds and
  single-digit milliseconds. Input still preempts bulk *between* frames as
  designed, but a frame already being written blocks the session loop until
  the socket accepts it, and on a contended wireless link that can be a
  third of a second. Wired is untested and the arithmetic in ADR 0013
  assumed 2.5 GbE. Nothing is dropped or stuck; a large transfer can make
  the pointer feel less immediate than it does at rest.
- A worker has been seen not exiting after a clean shutdown; the cause is
  unidentified, but a stuck message pump can no longer wedge the process —
  it is named in a warning and detached.

### Security

The threat model, trust model, and security invariants are documented in
[docs/SECURITY.md](docs/SECURITY.md). A dedicated review against them was
carried out before unattended operation shipped
([docs/security-review-phase6.md](docs/security-review-phase6.md)).
Parsers that touch network input are fuzzed on every change, and every
queue influenced by a peer is bounded before allocation.

[0.1.0]: https://github.com/stevenpickles/crossover/releases/tag/v0.1.0
