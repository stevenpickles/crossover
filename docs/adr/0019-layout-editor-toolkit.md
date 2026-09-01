# 0019. The layout editor is an egui/eframe application in its own user-session binary

Status: Accepted
Date: 2026-08-20

## Context

Phase 8's fifth deliverable is the editor itself, and
[ADR 0018](0018-drawn-display-topology.md) already decided everything about
*what* it edits: the layout model, the units it draws in, the state file the
worker publishes for it, and the config file an edit travels back through. Two
questions were left, and `adr/README.md` records them as this ADR's: **what the
editor is built with** — a core-library selection, the first trigger in that
file's list — and **where it runs**, which ADR 0018 raised (the worker is
headless and service-launched, so the editor is a user-session surface of its
own) without settling.

Four constraints shape the answer.

- **One codebase for the UI, on three operating systems.** Phase 9 ports
  Crossover to macOS and Linux. Every other platform-specific thing in this
  tree hides behind a `crossover-platform` trait with one implementation per
  OS, and that is affordable because each surface is small and precisely
  described: inject a key, read the clipboard, enumerate monitors. A GUI is
  not that. Three implementations of a drag-and-drop canvas is three sets of
  hit-testing, dragging, and snapping to keep identical, and the whole point
  of the layout is that the drawing means the same thing at both desks. So
  the deciding requirement is a toolkit that gives one codebase for the
  window, the canvas, and the input handling, with **no per-platform UI
  code** — not one that makes three native front ends convenient.
- **It cannot live in the worker.** ADR 0011's worker is headless and started
  by a `LocalSystem` service into the user's session;
  [ADR 0012](0012-elevated-worker-integrity.md) then gives it the user's
  elevated linked token so it can drive elevated windows. Neither property
  belongs anywhere near a window a person clicks in.
- **The editor handles nothing untrusted and holds nothing secret.** Its
  inputs are a state file this machine's own worker wrote and a config file
  the user owns (ADR 0018); it has no peer, no socket, and no credentials.
  Whatever it is built from, it should be arranged so that this stays true by
  construction rather than by intention.
- **The dependency policy is permissive-licence-only and crates.io-only**
  (`deny.toml`, [ARCHITECTURE.md](../ARCHITECTURE.md) §7). A GUI stack is the
  largest dependency this project will have taken; it has to pass the same
  gate as everything else, not a relaxed one.

And one thing that is *not* a constraint, worth saying because it excludes a
whole family of answers: the editor is small. It draws rectangles, drags them,
snaps them, and writes a file. There is no 3D, no video, no rich text, no
document model.

## Decision

### The toolkit is egui, through its eframe shell, on the glow backend

`eframe = { default-features = false, features = ["glow", "wayland", "x11"] }`.
The default feature set is off deliberately, and what is left is the minimum
that opens a window and paints in it:

- **`glow`, not `wgpu`.** The editor paints flat rectangles and text. The
  OpenGL path is available on every machine this will run on, and the wgpu
  stack — the largest single component of eframe's defaults — would buy the
  editor nothing it can use.
- **`x11` and `wayland`**, enabled from the start. They are inert on Windows
  and macOS, and having them in the manifest now means the Phase 9 Linux build
  is a compile rather than a manifest change.
- **`accesskit` off**, and therefore no screen-reader support today. Recorded
  as a known gap in the consequences below rather than left to be discovered.
- **`default_fonts` off** — see the font section, which is where the licence
  policy bit.

egui is chosen over the alternatives below for three reasons.

**Immediate mode matches this editor's shape.** The editor's entire state is a
set of rectangles and, at most, which one the pointer is dragging. In an
immediate-mode toolkit, hit-testing, dragging, and snapping are arithmetic
inside the code that paints the frame; in a retained toolkit they are a widget
with its own state machine that has to be kept in agreement with the model. The
second is more machinery for the same picture.

**It renders itself, identically everywhere.** egui does not wrap native
controls, so the arrangement looks and behaves the same on all three OSes. For
most applications that is a drawback worth arguing about. Here it is the
property the feature is *for*: a layout drawn at one desk and adopted at the
other should not be two different pictures.

**It is testable without a screen.** An egui pass runs with no window at all —
input in, painted shapes out — so the editor's screens can be asserted on in
the ordinary `cargo test` gate, on three CI runners none of which has a display
server. A GUI verifiable only by looking at it would be a permanent hole in the
Definition of Done ([TESTING.md](../TESTING.md) §5); this one is not.

Licence: MIT OR Apache-2.0, both already allowed.

### The editor is its own binary: on demand, user-launched, plain integrity

A new workspace member, **`apps/crossover-layout`**, producing
`crossover-layout.exe`. Three properties, each a deliberate contrast with the
worker:

- **On demand.** The service never starts it, stops it, supervises it, or
  knows it exists. ADR 0011's launcher supervises exactly one child — the
  worker — and gains nothing to do here: an editor that is not open is not a
  failure to recover from.
- **User-launched.** From the shell, or with `crossover layout`, which finds
  `crossover-layout.exe` beside the running `crossover.exe` and starts it
  detached. That subcommand exists because the binaries install into
  `%ProgramFiles%\Crossover` together and a user who knows one Crossover
  command should not have to learn where its files live.
- **Plain integrity.** Explicitly *not* ADR 0012's elevated linked token. The
  worker is elevated because it must inject into elevated windows; the editor
  drives nothing, injects nothing, and reads no protected state, so elevation
  would be privilege with no purpose — and an elevated window is a UI surface
  that a non-elevated one is not. The standing consequence is ADR 0018's,
  already recorded there: the config file has a medium-integrity writer (this
  editor) and a high-integrity reader (the worker), contained the way
  everything from that file always was — validated on load, before use.

**The two processes never rendezvous.** Every exchange is the file-based one
ADR 0018 specifies: the worker publishes `~/.crossover/state/topology.json` and
the editor reads it; an edit goes back through `config.toml`, which the worker
re-reads on its modification-time poll. No socket, no pipe, no shared memory,
no lifetime coupling — so there is no channel to authenticate, and either
process can be absent without the other noticing anything worse than a stale
timestamp.

### The dependency graph is where the isolation is enforced

`apps/crossover-layout` depends on **the GUI stack and `crossover-topology`,
and on no other Crossover crate** — not `crossover-core`, `-protocol`,
`-security`, or any `crossover-platform*`. (`crossover-topology` arrives with
the canvas; this branch establishes the crate with the GUI stack alone.) A
reviewer verifies that by reading one `Cargo.toml` and `cargo tree`, which is
exactly the technique ADR 0011 used for `crossover-svc`.

What it buys is the mirror image of what it bought there, and both directions
matter:

- Outward, as in ADR 0011: the process that a person drives — the one that
  will grow file dialogs and drag handles — contains no TLS, no protocol
  decoder, no input injector, and no trust store.
- Inward, which is new: **the process that holds the keys and injects input
  does not contain the GUI stack.** eframe brings roughly 90 transitive crates
  on Windows (213 counting every target's), a windowing layer, an OpenGL
  loader, and all of their `unsafe`. None of that belongs in `crossover.exe`.

**One honest exception, stated rather than discovered later.** eframe enables
`egui-winit`'s `clipboard` feature unconditionally, so the editor links
`arboard` — a clipboard library. It never calls it: nothing in the editor reads
or writes the clipboard, and the clipboard path that matters (ADR 0005, ADR
0014) is the worker's, in another process. If that ever becomes more than a
dormant dependency, it is worth an amendment here.

**This ADR is the crate-creation record `adr/README.md` requires**, on ADR
0011's precedent: creating a workspace crate is architecturally significant,
but a crate created *as part of* a decision is recorded in that decision rather
than in an ADR of its own.

### Fonts: `default_fonts` off, the Go fonts embedded

egui bundles its own faces, and `cargo deny` rejects them: `epaint_default_fonts`
is `(MIT OR Apache-2.0) AND OFL-1.1 AND Ubuntu-font-1.0`, and neither font
licence is on this tree's allow-list. Neither is a threat to an MIT binary, and
a narrow per-crate exception would have been defensible — but the Ubuntu font
licence forbids selling the font by itself and is not OSI-approved, OFL-1.1
requires that the licence travel with the font and that derivatives stay under
it, and a font is the one dependency in this decision that is trivially
substitutable. Widening a permissive-only policy for a replaceable asset is the
wrong trade.

So `default_fonts` is off and the editor embeds **Go-Regular and Go-Mono**
(Bigelow & Holmes, **BSD-3-Clause** — a licence the policy already allows) from
`assets/fonts/`, installing both families before the first frame. The
monospace family is a real monospace face because monitor device strings
(`\\.\DISPLAY1`) are the text this editor shows most of.

Two costs, accepted: the Go fonts cover Latin, Greek, and Cyrillic, so a
character outside that — an emoji, a CJK device name — renders as a box rather
than falling back to another face; and BSD-3-Clause asks that its notice
accompany a binary distribution, which is why `THIRD-PARTY-NOTICES.txt` now
ships in the portable archive and the Chocolatey package.

### `unsafe_code = "forbid"` is unchanged

The workspace lint stands and the editor crate inherits it like every other:
NFR-6 is about the code this project writes, and the windowing and GL `unsafe`
inside the GUI stack is a **supply-chain** question — the one `deny.toml` and
the dependency policy answer — not a lint one.

## Alternatives Considered

- **iced.** Genuinely viable: Rust, cross-platform, MIT, actively developed,
  and it renders itself for the same reason egui does. Rejected on fit rather
  than on merit — its retained, Elm-style architecture turns "drag this
  rectangle and snap it to that edge" into a custom widget with its own state
  to keep in step with the model, where immediate mode makes it arithmetic in
  the paint code, and its drag-a-thing-on-a-canvas idiom is the less
  trodden path of the two. The redraw cost that would argue the other way does
  not exist for one window of rectangles.
- **Slint.** Capable, and aimed squarely at small tools like this one.
  Rejected on licensing: it is GPL, or royalty-free under conditions, or
  commercial. `deny.toml` allows permissive licences only, and copyleft is
  absent from that list on purpose — taking Slint would mean either changing
  this project's obligations or negotiating an exception to them, for a
  toolkit whose advantages over egui here are aesthetic.
- **Tauri.** Rejected twice over. It is a webview plus a second language and
  its build toolchain, and the webview is a *different engine per OS* — which
  is the one thing the deciding requirement above was written to avoid. Its
  supply-chain shape (a JavaScript toolchain beside the Rust one) is also off
  brand for a tree that keeps every dependency on crates.io and audits it
  mechanically.
- **The three native toolkits** — Win32/WinUI, Cocoa, GTK or Qt. Fails the
  deciding requirement outright rather than on a trade: three implementations
  of the same canvas, three sets of drag behaviour, and three chances to
  disagree about a drawing that has to mean exactly one thing. It would also
  triple the cost of every subsequent editor change, in a phase whose whole
  value is in the editor.
- **A mode of `crossover.exe`** — `crossover layout` opening its own window
  in-process. Rejected for the reason ADR 0011 rejected a service mode, run in
  the other direction: it would put several hundred GUI crates into the
  process that holds the device key and injects input, and the isolation above
  would rest on discipline instead of on a dependency graph.
- **A web UI served by the worker.** Rejected: it would have the headless,
  elevated worker listening on a local HTTP port and rendering an interface —
  new attack surface on the one process that already has network exposure, to
  avoid shipping a binary.
- **wgpu instead of glow.** Rejected: a large graphics stack for flat
  rectangles, when the OpenGL floor glow targets is available everywhere the
  editor runs.
- **A narrow `deny.toml` exception for egui's bundled fonts** instead of
  embedding our own. Rejected — see the font section: it widens a
  permissive-only policy for the most replaceable dependency in the tree.

## Consequences

- **A third shipped binary.** `scripts/build.ps1`, both install paths, the
  Chocolatey package, and the portable archive all grow an entry. It also
  changes how an upgrade releases its file locks: a running editor holds
  `crossover-layout.exe` exactly as a running service holds
  `crossover-svc.exe`, but nothing supervises it, so removing the service is
  no longer sufficient. All three scripts now close **whatever is running out
  of the install directory** — selected by executable path, so a development
  copy elsewhere is untouched — asking each window to close before forcing
  anything. The editor carries the same stamped identity as the other two
  (`apps/build_identity.rs` as an included source file, so it costs no
  dependency edge), which means `build.ps1`'s "these binaries were built
  together" check covers all three.
- **A new subcommand, `crossover layout`.** It resolves the editor as a
  sibling of the running executable and never consults `PATH`, so it cannot
  start a different install's copy; a missing editor is a clear error naming
  the path it looked at.
- **A release editor is a GUI-subsystem binary**, so no console window trails
  it when it is started from Explorer — at the cost that
  `crossover-layout --version` reaches a redirected stdout but not the console
  of a shell that typed it. Attaching to the parent console is Win32, which
  this crate is deliberately unable to reach; the same facts are in the exe's
  version resource, which is where Explorer and `build.ps1` read them.
- **`deny.toml` gains two licences**, covering four crates, each named where
  it is allowed: `BSL-1.0` (`clipboard-win` and `error-code`, reached through
  `arboard`) and `Zlib` (`foldhash` and `slotmap`, egui's internals). Both are
  permissive and neither adds an obligation to a shipped binary. The font
  licences were *not* added — see above — and the Go fonts' notice ships with
  the artifacts instead.
- **This is the largest dependency addition the project has made** — about 90
  transitive crates per platform, most of them windowing and graphics. The
  mitigations are structural rather than hopeful: they are confined to one
  binary that processes no untrusted input and holds no secret, they are all
  on crates.io, and the daily `cargo deny` audit covers them — so an advisory
  in the windowing stack now fails the gate like any other. The duplicate-
  version warnings that come with a tree this size stay warnings, as
  `deny.toml` already decided.
- **No accessibility support today.** `accesskit` is off, so the editor is not
  reachable by a screen reader. It is a feature flag away, and turning it on
  is a decision about dependency surface rather than about architecture — but
  as of this ADR the honest statement is that the editor is a visual tool
  only.
- **Text coverage is Latin, Greek, and Cyrillic**, the consequence of
  embedding two faces rather than taking egui's fallback stack. A device or
  monitor name outside that renders as boxes. If that ever bites a real user,
  the fix is another BSD/MIT/Apache face in `assets/fonts/`, not a policy
  change.
- **Phase 9 gains a third binary to port**, and it is the cheapest of the
  three: the Linux display-server features are already enabled, and the
  Wayland platform risk that gates the port (L-1,
  [platform-risks-linux.md](../platform-risks-linux.md)) is about the
  *worker's* input capture, not about an ordinary application window.
- **The editor's screens are unit-testable, headless, in the ordinary gate**
  on all three OSes — which is what keeps this GUI inside the Definition of
  Done rather than beside it.

**Amendment (2026-08-21):** the editor's read of the worker's state file
turned up a diagnosability gap this ADR's original dependency graph could
not fill: `crossover-layout` is a GUI-subsystem binary in release builds
(the consequence above), so a run with no console attached — an ordinary
release install, launched from Explorer or `crossover layout` — has
**nowhere to report** a state file it could not use (a version mismatch, a
torn or hand-edited document). `eprintln!` reaches such a console when one
exists and reaches nothing otherwise, which is the wrong trade for a
diagnostic NFR-3 requires: silence about *why* the editor is showing an
empty canvas is exactly the failure mode this branch's read-and-classify
work exists to avoid.

The fix is `apps/crossover-layout/src/logging.rs`: a minimal, **file-only**
`tracing-subscriber` installation into `~/.crossover/logs`, the same
directory `apps/crossover/src/logging.rs` and its `crossover-svc`
counterpart already write to, so a diagnostic from any of the three
binaries of one install ends up in one place. No console layer (useless in
the one build configuration that needed a sink at all), no panic hook
(unlike the other two binaries, this one's `main.rs` already reports its
one fatal failure mode — no window — by other means, and a panic inside
egui's own paint loop is not a case this amendment adds handling for).

This adds three direct dependencies, and the honest accounting is that only
two of them are new: `tracing` was already transitively present —
`winit`, under `eframe`, depends on it — so naming it directly turns an
existing edge into a declared one rather than adding to the dependency
graph a `cargo tree` diff would show. `tracing-subscriber` and
`tracing-appender` are the genuinely new part, and the smallest pair that
makes a release build's diagnostics reach a file at all; both are already
in `deny.toml`'s allow-list and already audited daily, since `apps/crossover`
and `apps/crossover-svc` have depended on them since Phase 0. The sentence
this ADR's "dependency graph" section states — "the GUI stack and
`crossover-topology`, and on no other Crossover crate" — is unaffected:
these are the same first-party-free family the other two binaries already
carry, not a fourth Crossover crate and not a new supply-chain relationship
this tree has not already accepted.
