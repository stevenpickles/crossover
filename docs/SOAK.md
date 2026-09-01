# Two-Machine Soak Runbook

The hermetic stress test (`tools/test-peer/tests/stress.rs`) proves the
clipboard protocol correct at volume and gates every merge. It cannot
prove anything about *real* clipboards, *real* networks, or the timing
of two independent machines — that is what this runbook is for.

Read the difference honestly:

| | Hermetic stress | This soak |
|---|---|---|
| Clipboard | In-memory fake | Real Windows clipboards |
| Peers | One process | Two machines, one network |
| Determinism | Total | None (a live desktop interferes) |
| Verdict | Red build | A report you interpret |

Nothing here is a build gate. A failure means "investigate", not
"revert".

---

## What this proves

1. **Reliability** — every copied item reaches the other machine's
   clipboard byte-identical, or fails observably (FR-3.2, FR-3.6).
2. **No loops** — an item applied on the far side never echoes back
   (FR-3.3); a loop is a release-blocking defect.
3. **Latency** — copy-to-applied round trip, measured on the
   originating machine's clock alone, so no clock skew between the two
   machines enters the number (docs/TESTING.md §4).
4. **Reconnect resilience** — pulling the network mid-run resumes sync
   without re-pairing and without losing items (FR-6.1, FR-6.2).

## Setup

Machine **A** listens, machine **B** connects. Either can originate
copies; the roles only determine who dials.

### 1. Build once, copy the binary

On the development machine:

```
cargo build --release
```

Copy `target/release/crossover.exe` to machine B (file share, USB, or
`scp`). No toolchain is needed on B — the binary is self-contained apart
from the Microsoft Visual C++ runtime, which any current Windows has.

### 2. Open the firewall on the listener

**This is the single most common reason a first two-machine run looks
broken when nothing is wrong**: Windows silently drops the inbound
connection and B just retries forever.

On machine A, in an **elevated** prompt:

```
netsh advfirewall firewall add rule name="Crossover" ^
    dir=in action=allow protocol=TCP localport=27677
```

Remove it afterwards with:

```
netsh advfirewall firewall delete rule name="Crossover"
```

### 3. Find A's address

On machine A:

```
ipconfig | findstr IPv4
```

Use the address on the LAN both machines share.

### 4. Pair

On machine A:

```
crossover --name machine-a pair --listen
```

It prints a one-time code. On machine B, within two minutes:

```
crossover --name machine-b pair <A-address>:27677
```

Type the code when prompted. Both sides print the pinned fingerprint;
**check they match** — that comparison is the ceremony's whole point.

### 5. Run with logging captured

Machine A:

```
set RUST_LOG=info
crossover run --listen > soak-a.log 2>&1
```

Machine B:

```
set RUST_LOG=info
crossover run --connect <A-address>:27677 > soak-b.log 2>&1
```

Both should print `Session established`.

## The soak

With both running, drive clipboard changes and let them settle. A
simple generator, on either machine (PowerShell):

```powershell
1..500 | ForEach-Object {
    Set-Clipboard -Value "soak $env:COMPUTERNAME $_ $(Get-Date -Format o)"
    Start-Sleep -Milliseconds 200
}
```

Vary it deliberately — this is where a live run earns its keep:

- **Both machines copying at once** for a stretch (conflict resolution:
  both sides must converge on the same item, and their logs must agree
  on which one won).
- **Large items** — paste a few hundred KB of text to exercise the
  offered flow (above 64 KiB) rather than only the inline flow.
- **Contention** — copy from another application (browser, editor)
  while the generator runs.
- **Reconnect** — disable machine B's network adapter for ~30 seconds,
  re-enable it, confirm sync resumes with no re-pairing.
- **Restart** — close and reopen `crossover run` on B; trust must
  persist.

## Reading the results

Both logs are structured. The analysis script summarizes them:

```
python tools/soak-report.py soak-a.log soak-b.log
```

It reports, per machine: transactions closed by result, latency
percentiles (p50/p95/max), retry counts, contention events, disconnects
and reconnects, and any error-level line.

### Measured baseline (2026-08-07, two Windows machines on one LAN)

The Phase 2 closure run, for comparison rather than as a target — a
different network or machine will differ, and that is fine:

| | Value |
|---|---|
| Transaction latency (p50) | 4–6 ms |
| `read_busy` events | 0–1 per run |
| Peak clipboard writes/sec | 3 |
| `Set-Clipboard` failures in other apps | none |
| Items transmitted per 200 rapid copies | ~15 |

That last row is the one worth internalizing: with the settle window
(ADR 0006), most copies in a rapid burst never travel, because they were
superseded before the clipboard settled. Fewer items transmitted is the
design working, not sync failing. Verify by *content*, never by count.

What good looks like:

- `applied` accounts for essentially every transaction; a handful of
  `superseded` is normal. It no longer means only "both machines copied at
  once": since ADR 0005's 2026-09-01 addendum it also counts an **install
  that never landed** and was outranked — by a newer item from the peer,
  by a local copy made while it was parked, or by an item that already
  matched this clipboard. Read it beside `installs parked`: supersessions
  with no parked installs are the conflict race, supersessions with parked
  installs are contention losing to the user, and both are healthy.
- **Zero** `clipboard_installs_failed` in the absence of deliberate
  contention. This is the run report's clipboard-reliability number: it
  counts inbound items the destination clipboard never took, which is
  content the user lost, and it prints beside `applied` on the summary's
  first clipboard line. A few during staged contention is the bounded-retry
  design working, and each one is visible rather than silent.
- `clipboard_installs_parked` is *not* a failure — it counts installs that
  outlived the fast retry budget and went to the slower parked cadence
  (ADR 0005, addendum 2026-09-01), most of which still land. Its line
  prints only when it happened. A run with parked installs and zero failed
  ones is the fix doing exactly its job; a rising ratio of failed to parked
  says the 20 s budget is not enough for this machine and is worth
  investigating rather than raising.
- `clipboard_abandoned` means **a peer was there and did not answer**.
  Since ADR 0006's 2026-09-01 addendum a local copy made with no live
  session mints no transaction at all, so nothing can expire against a
  peer that was never asked. Before that change an evening with the peer
  asleep produced one `abandoned` per copy — twenty in one evening on
  machine A — and the counter could not be read for the silent stalls it
  exists to catch. A non-zero value now is a real unanswered transaction.
- `clipboard_offline_changes` is *not* a failure — it counts local copies
  made while no peer was connected. They are held, not lost: establishing
  a session re-reads the clipboard and offers whatever is current
  (ADR 0006's trigger 3), so one offer follows a gap of any length. Its
  line prints only when it happened, and a large count beside a small
  `sent` count is a pair that spent the run apart, not a clipboard that
  stopped working. `tools/soak-report.py` counts the stretches from the
  log and says the same in its notes.
- p50 latency in the low tens of milliseconds on a LAN; the max may
  spike when another application holds a clipboard.
- Every disconnect is followed by a reconnect with no pairing.

What warrants investigation:

- **Any** content mismatch — copy something on A, confirm the exact
  bytes on B (this is release-blocking).
- Sync traffic that continues when nobody is copying — the signature of
  a loop.
- `clipboard_installs_failed` at rest.
- `clipboard_abandoned` at rest with sessions up throughout: with the
  offline case removed, this is a peer that accepted and went quiet, or
  an offer that never reached one.
- `clipboard_offline_changes` in a run where the pair was supposed to be
  connected the whole time — the copies are safe, but the sessions were
  not up when the operator thought they were.
- Parked installs at rest, in any number: the fast budget covers everything
  a healthy desktop does to its own clipboard, so parking at rest means
  something on the machine is holding it for seconds at a time. Find out
  what before deciding it is benign.
- Latency growing steadily over the run (a leak or unbounded queue).

Record the outcome in the phase's exit-criteria notes
(docs/ROADMAP.md), including the conditions: how long, how many items,
what interference was staged.

---

## Troubleshooting: every clipboard operation fails with "Access is denied"

Seen twice on a development machine (2026-08-07): `OpenClipboard` returns
error 5 for **every** process — the platform tests fail in a block, and
even PowerShell's `Set-Clipboard` fails — while `GetOpenClipboardWindow`
reports no holder (a process that opened the clipboard with a NULL window
handle is invisible to it). Thirteen consecutive test-suite runs could
not reproduce it, so it is environmental, not a Crossover defect: the
signature of a wedged per-session Clipboard User Service. The fix, from
an elevated prompt:

```
Restart-Service cbdhsvc*
```

If clipboard tests fail this way, check `Set-Clipboard` in PowerShell
first — if that also fails, the machine is wedged and the test results
mean nothing until the service is restarted.

### Reading the holder from the log line (feature/162)

Hardware evidence (2026-09-01, machine A) found the ordinary, briefer
cousin of the above: at 5 of 8 peer reconnects, `OpenClipboard` failed for
about a second with the same "Access is denied", but recovered on its own
— routine contention (R-5), not a wedged service, and every such failure
now names its holder in the reason string.

That reason surfaces at two different log levels, and it matters which
one an operator is reading:

- **Where it actually appears at `RUST_LOG=info`** (this soak's own
  documented setting, above, and the worker's default filter when
  `RUST_LOG` is unset): `crates/crossover-core/src/clipboard_driver.rs`
  logs the *first* `Busy` for a given clipboard item at `warn!` —
  `write busy` / `offer busy`, with `clipboard_id` and the holder-naming
  `error` field — and every retry after that (every 200 ms, up to the
  retry budget) at `debug!`, so a sustained contention episode produces
  exactly one visible line per item, not a flood. A line that never
  clears eventually meets the engine's own give-up line in
  `crates/crossover-core/src/clipboard.rs` (`clipboard item could not be
  installed`, also `warn!`), which the `clipboard_id` field ties back to
  this one.
- **The reason string itself**, produced in
  `crates/crossover-platform-windows/src/clipboard.rs`, is what both of
  those log lines' `error` field carries:
  - `OpenClipboard failed (clipboard held elsewhere?): Access is denied.
    (0x80070005); held by pid 1234 "SomeApp.exe" (window class "Foo")` —
    an external application (Clipboard History, a password manager, an
    RDP client are the usual suspects); nothing to fix in Crossover. A
    class name Win32 could not read prints as `(window class unreadable)`
    — unquoted, on purpose, so it can never be mistaken for a window
    genuinely classed that word.
  - `…; held by this process (our own clipboard guard on thread T, site
    "read"/"write"/"ole")` — the contention is **internal**: this
    process's own `OpenGuard` (the ordinary text/image path) collided
    with itself, most often against the OLE virtual-file apartment thread
    (`crossover-platform-windows::virtual_file`, site `"ole"`). Worth a
    closer look if it recurs, since nothing external explains it. This is
    the shape that needed its own tracking (`OWN_HOLD` in `clipboard.rs`):
    every call site here opens with a `NULL` window handle, which
    `GetOpenClipboardWindow` cannot see at all — without it, this exact
    case would misreport as the "unidentified" bucket below.
  - `…; held by this process (pid N, thread T, window class "...")` — the
    same internal-contention finding, reached instead through a real
    window `GetOpenClipboardWindow` could see (some other in-process
    clipboard use we do not control, rather than one of our own marked
    call sites).
  - `…; held by an unidentified owner (no window)` — two distinct causes
    read identically here, and neither one alone warrants chasing the
    service restart below:
    1. **Benign and transient.** The holder released the clipboard in the
       gap between our failed open and this lookup — inherent to asking
       *afterwards*, and the ordinary shape of routine contention that
       clears on its own.
    2. **Some other process opened with a `NULL` window handle**, the same
       way this codebase's own call sites do, so it is invisible to
       `GetOpenClipboardWindow` for the same reason ours would be (were
       `OWN_HOLD` not there to catch it first).

    Only if this line **persists** — recurring well past a few retries,
    or turning up as the wholesale "every process" failure the top of
    this section describes — does it become worth suspecting the wedged
    Clipboard User Service and reaching for `Restart-Service cbdhsvc*`.
    A handful of transient occurrences during a reconnect burst is not
    that; it is R-5 working as designed.

The prefix (`OpenClipboard failed (clipboard held elsewhere?)` /
`OleSetClipboard failed (clipboard held elsewhere?)`) is unchanged and
still what to `grep` for; the holder clause is appended after it.

## Phase 3 soak: remote mouse (two machines)

This is the Phase 3 exit criterion that no single machine can show:
driving machine B's pointer with machine A's mouse, and proving that
repeated control cycles and mid-control faults never leave a stuck
button or a dead mouse (FR-4.4, FR-5.1). The hermetic property tests in
`crossover-core` prove the state machine; this proves the two real
machines.

Pair and connect exactly as for the clipboard soak above (same binary,
same firewall rule, same pairing ceremony). Then, with both sides
running `crossover run` (A `--listen`, B `--connect <A>:27677`), both
consoles accept commands:

```
c   take control of the peer
r   release / hand back
q   quit
```

### The procedure

1. **Take control.** On machine A, type `c`. A prints "You now control
   the peer"; B prints "The peer is now controlling this machine." Move
   A's mouse — **B's cursor moves**, and A's does not (A's local pointer
   is suppressed; that is capture working). Click and scroll; they land
   on B.
2. **Hand back.** On A, type `r`. A's mouse comes back to life; B's
   cursor is left wherever it was, with **no button held** — verify by
   clicking on B that nothing was already down (no stuck drag).
3. **Repeat rapidly.** Alternate `c` and `r` a few dozen times, moving
   and clicking each time you hold control. Not once should a button be
   left down on B, and A's mouse must be alive after every `r`.
4. **The fault that matters — disconnect mid-drag.** On A, take control
   (`c`), press and *hold* A's left button while moving (a drag on B),
   and while still holding, pull B's network (disable its adapter) or
   `q` on B. On A the control ends ("session lost"); on B, when it comes
   back, **the button must not be stuck** — B synthesized the release
   from its own record the moment the session dropped. This is the
   release-blocking scenario; a stuck button here fails Phase 3.
5. **Revoke from the controlled side.** Take control from A again, then
   on **B** type `r`. A stops controlling immediately ("the peer revoked
   your control"); A's mouse returns. The local user's escape hatch
   works even while being driven.

### What good looks like

- While A holds control, A's cursor is frozen and B's tracks A's mouse
  one-to-one; motion feels immediate on a LAN (NFR-5).
- After every hand-back, revocation, or disconnect, **B holds no button
  and no key** and B's mouse is fully alive. A stuck button or dead
  pointer is release-blocking — record it and stop.
- Every transition is narrated on both consoles (NFR-3): no silent
  changes of who is in control.
- Injection into an elevated window on B may silently do nothing (UIPI,
  R-1) — expected and documented; A's log notes when injection is
  likely to have been swallowed.

Record the outcome in the Phase 3 exit-criteria notes
(docs/ROADMAP.md): how many control cycles, whether any button ever
stuck, and the subjective pointer feel.

## Phase 3 probe: input capture (single machine)

Suppression cannot be proven by CI: the automated tests cover tag
filtering, translation, state transitions, and a synthetic end-to-end
event through the real hook and Raw Input pipeline, but only a human
moving a real mouse can confirm that the *local cursor does not move*
while capture holds it. That is what this probe is for. One machine, no
peer needed.

```
cargo test -p crossover-platform-windows manual_probe_capture -- --ignored --nocapture
```

What happens, in order:

1. The probe prints a warning, then captures for **ten seconds**. The
   mouse goes dead — the cursor must not move, clicks must not land,
   scrolling must do nothing. **That is suppression working.** Keep
   moving, clicking, and scrolling anyway; the keyboard still works
   (Ctrl+C aborts if something goes wrong).
2. After ten seconds the probe releases capture and the mouse comes
   back to life on its own.
3. It prints how many motion, button, and scroll events were observed
   while the mouse was dead, and whether capture stayed healthy.

What good looks like:

- The cursor did not move at all during the window — any movement while
  "dead" means suppression failed, which is a defect.
- Event counts reflect what you actually did (hundreds of motion events
  from continuous movement; your clicks and scrolls present).
- `capture healthy at end: true`, and the mouse is fully alive
  afterwards — a mouse that stays dead after release is the
  release-blocking defect class (FR-4.4).

Honest limitation: the hook-loss watchdog (R-2) cannot be staged
manually without deliberately wedging the pump thread, so this probe
does not exercise it; its decision logic is unit-tested, and the
loss path is exercised only if Windows actually removes the hook.

From Phase 4 on, the probe freezes the **keyboard** too — both hooks
install together. You cannot Ctrl-C out during the ten seconds; it
auto-releases. The report also counts key events. In a live session
(not the probe), the way out while the keyboard is captured is the
escape gesture: **press both Control keys at once**, which is caught in
the hook, never sent to the peer, and hands control straight back.

## Phase 4 soak: remote keyboard (two machines)

This is the Phase 4 exit criterion that no single machine can show:
typing on machine B with machine A's keyboard, common shortcuts landing
correctly, and — the release-blocking part — proving that repeated
control cycles and a disconnect at an *arbitrary* moment never leave a
key or a modifier logically pressed on B (FR-4.1–FR-4.4). The hermetic
property tests in `crossover-core` prove the state machine ends clean on
every interleaving; this proves the two real machines and two real
keyboards.

Pair and connect exactly as for the clipboard and mouse soaks above
(same binary, same firewall rule, same pairing ceremony, same `c` / `r`
/ `q` console). The keyboard is captured and injected on the same grant
as the mouse — one `c` takes both.

### The procedure

1. **Take control and type.** On machine A, type `c`. Put the caret in a
   text editor on **B** and type a paragraph on A's keyboard — letters,
   digits, punctuation, Enter, Backspace. Every character must land on B,
   in order, once each. A's own keyboard is captured: your keystrokes do
   **not** reach A's foreground app (that is suppression working).
2. **Modifiers and chords.** Still controlling, exercise the shortcuts
   real use depends on: Shift for capitals, `Ctrl+C` / `Ctrl+V` (which
   also round-trips through the clipboard — the copied selection on B
   syncs back to A), `Alt+Tab`, the Windows key, arrow keys, Home/End,
   and a function key or two. Chords must keep their ordering — a held
   Shift must still be down when the letter arrives, so capitals are
   capitals, not stray lowercase.
3. **Auto-repeat.** Hold a key down. B repeats it (the OS repeat on the
   source is forwarded as repeat events), and releasing stops it cleanly
   with no trailing characters.
4. **The keyboard escape.** While A is controlling and A's keyboard is
   captured, **press both Control keys at once**. Control hands straight
   back — A's keyboard comes alive, B is left with nothing held — and the
   Control presses themselves must **not** land on B (no phantom Ctrl on
   B afterward). This is the local user's escape hatch; it is caught in
   the hook and never forwarded.
5. **Repeat rapidly.** Alternate `c` and `r` a few dozen times, typing a
   few characters and one chord each time you hold control. Not once
   should a key or modifier be left down on B, and A's keyboard must be
   alive after every `r` — verify by typing on A.
6. **The fault that matters — disconnect with a modifier held.** On A,
   take control (`c`), press and *hold* a modifier (Shift, or Ctrl) while
   typing so it is logically down, and while still holding it pull B's
   network (disable its adapter) or `q` on B. On A the control ends
   ("session lost"). When B comes back, **nothing is stuck**: type
   normally on B and confirm it is not all capitals (stuck Shift) and
   shortcuts are not firing (stuck Ctrl) — B synthesized the release for
   every held key from its own record the moment the session dropped.
   This is the release-blocking scenario; a stuck modifier here fails
   Phase 4.
7. **Revoke from the controlled side.** Take control from A again, then
   on **B** type `r`. A stops controlling immediately, A's keyboard
   returns, and B holds nothing. The local user's escape works from the
   keyboard-and-console side even while being driven.

### What good looks like

- Every character and chord lands on B exactly once and in order; a held
  modifier is still held when the key it modifies arrives.
- After every hand-back, escape, revocation, or disconnect, **B holds no
  key and no modifier**: typing on B is immediately normal. A stuck key
  or a phantom modifier is release-blocking — record it and stop.
- The both-Control escape always returns control and **never** reaches
  B; no stray Control state is left on either side.
- Every transition is narrated on both consoles (NFR-3).
- Injection into an elevated window on B may silently do nothing (UIPI,
  R-1) — expected and documented; type into an ordinary editor to see the
  keystrokes land.

### Honest limitations

- **Physical-key model (ADR 0008).** A key travels as its USB HID usage,
  reproduced as the *same physical key* on B; the produced text is
  carried alongside, but text-fallback injection is deferred past this
  phase. With **mismatched keyboard layouts** the character on B follows
  B's layout, not A's — soak with matching layouts, or read a divergent
  character as the known limitation, not a defect.
- Dead keys, IME composition, and global OS hotkeys the shell claims
  before the hook (e.g. `Ctrl+Alt+Del`) are out of scope for this phase.
- **Apps with their own key handling interpret forwarded keys their own
  way.** A native editor or IDE that binds Home/End itself (smart-home to
  first non-whitespace, custom selection widgets like Scintilla) may
  respond to a forwarded `Shift+Home`/`Shift+End` differently from a
  plain text box — the forwarding is correct into standard controls
  (`crossover-platform-windows` proves capture, injection, and
  injection→selection all drive a shifted navigation selection), so any
  divergence is the application's behavior. Confirm by pressing the same
  chord on that machine's **local** keyboard in the same app: identical
  behavior means it is the app, not Crossover.

Record the outcome in the Phase 4 exit-criteria notes
(docs/ROADMAP.md): how many control cycles, whether any key or modifier
ever stuck, whether the escape ever leaked, and the subjective feel of
typing on the far machine.

## Phase 5 soak: seamless edge transfer (two machines)

This is the Phase 5 exit criterion, and the payoff of the whole project:
the two machines behave like neighboring monitors. The cursor crosses a
screen edge and control — pointer *and* keyboard — follows on its own,
then returns at the reverse edge, with no console command. The hermetic
tests prove the topology math, the edge detector, the engine, and the
placement seam in isolation; only two real machines side by side prove
they compose into the seamless illusion (ADR 0009).

### Setup

Pair and connect exactly as for the earlier soaks (same binary, firewall
rule, pairing ceremony). Then run each machine with its **side of the
pair**, arranged physically as `A | B` — A on the left, B on the right:

```
# Machine A (left screen), listening:
crossover --name machine-a run --listen --left  > soak-a.log 2>&1

# Machine B (right screen), dialing A:
crossover --name machine-b run --connect <A-address>:27677 --right > soak-b.log 2>&1
```

Each side prints its whole-desktop geometry and edge at startup — check it:

```
Desktop: 7680x2400 (all monitors). Seamless: Left screen, crossing on its Right edge.
Desktop: 3840x2160 (all monitors). Seamless: Right screen, crossing on its Left edge.
```

The left machine's **right** edge links to the right machine's **left**
edge. Differing resolutions are fine — the crossing position travels as a
fraction of the *edge monitor*, mapped through each machine's own geometry,
so mismatched resolutions and DPI land the cursor at the matching height.

If a soak ever needs to separate cursor behavior from control transfer, add
`--no-cursor-mask` to either side to run with the cursor never hidden.

### The procedure

1. **Cross to the peer.** Both start local. Move A's cursor into A's
   **right** edge. Control transfers on its own: A's console prints "You
   now control the peer", B's prints "The peer is now controlling this
   machine", and **B's cursor appears at B's left edge at the same height
   the cursor left A**. A's own pointer is now frozen; moving A's mouse
   drives B's cursor.
2. **Keyboard follows.** With B under control, type — the keystrokes land
   on B, no separate action. Try a shortcut or two.
3. **Cross back.** Drive B's cursor (with A's mouse) into B's **left**
   edge. Control returns on its own: B reclaims, A's pointer comes back to
   life at A's right edge at the matching height, and A is local again.
4. **One cursor, on the active machine.** Throughout, exactly one cursor
   should be visible — on whichever machine you are driving. Crossing A → B
   hides A's cursor and shows B's; returning shows A's and hides B's. Never
   two cursors, and never (for long) none.
5. **The cursor fail-safe.** If a cursor ever goes missing, just move the
   mouse or press a key on the machine you are sitting at — its cursor
   reappears within a moment. This is the safety net (ADR 0009): local
   input on a machine that is not driving the peer means the user is there.
6. **Repeat, many times.** Move A → B → A through the edges a few dozen
   times, using only the mouse — never the console. Every cycle should
   transfer cleanly, place the cursor sensibly, and leave **no stuck key
   or button** on either machine.
7. **Clipboard rides along.** At points during the cycling, copy on one
   machine and paste on the other (both directions). Clipboard sync must
   keep working throughout the control transfers — the two subsystems are
   independent (FR-5.4).
8. **The overrides still work.** The console `c` / `r` and the
   both-Control escape remain: while controlling, the escape hands back
   instantly; `c` / `r` force a transfer or reclaim regardless of edges.
9. **The fault that matters — converge under loss.** Induce packet delay
   or loss *during* a crossing — a tool like `clumsy`, or disabling B's
   adapter for a couple of seconds mid-transfer. Confirm the system always
   converges to **exactly one owner**: never both machines controlling,
   never a dead cursor on both. A lost acknowledgement times out back to
   local; a disconnect mid-control releases everything (`ReleaseAllInput`)
   and returns both to local until the session re-establishes.

### What good looks like

- Crossing the edge transfers within a moment; the cursor appears on the
  far machine at the same proportional height and keeps moving smoothly
  (the Phase 3 pointer feel carries over).
- The keyboard follows the pointer with no extra step.
- After every A → B → A cycle, **both machines hold no key and no
  button**, and both pointers are alive; the escape and console overrides
  still work.
- **Exactly one cursor is visible** at a time — on the machine you are
  driving. A stray missing cursor is rescued by touching that machine's
  mouse or keyboard, and quitting (`q` / Ctrl-C) or losing the connection
  always restores the cursor.
- Copied content matches across the machines throughout the cycling.
- Under induced delay or loss, exactly one machine ends up in control — or
  both end local after a disconnect. Never a split brain, never a dead
  cursor on both.
- Every transition is narrated on both consoles (NFR-3).
- The shutdown metrics (on `q` / Ctrl-C) report the control gains, given,
  and hand-backs — a rough tally of how many crossings the run drove.

### Honest limitations

- **Immediate crossing (ADR 0009).** Touching the linked edge transfers
  at once — there is no dwell or push-through this phase. A cursor parked
  against that edge will cross; if accidental crossings prove annoying in
  the soak, that is the trade recorded in ADR 0009 to revisit, not a bug.
- **Multi-monitor edge.** A machine with several monitors treats them as
  one desktop, so the crossing edge is the *outer* edge of the whole
  desktop, not a seam between monitors. The crossing fraction maps against
  the specific monitor on that edge (the outermost one), so mismatched-
  resolution monitors and the dead space between them place the cursor
  correctly — a crossing from a 2160-tall monitor lands at the matching
  height on a 2160-tall one, not shifted by a taller neighbour.
- **DPI.** The process is per-monitor DPI aware, so geometry and cursor
  coordinates are real pixels across mixed-DPI monitors (the startup line
  shows the true resolution, e.g. a 3840×2400 panel as 3840×2400, not its
  scaled size). The crossing itself maps by fraction, so differing
  resolutions transfer regardless.
- **Cursor visibility is a display nicety, with a fail-safe.** Only one
  cursor is shown — on the active machine — by blanking the system cursor
  on the machine you are not on (`SetSystemCursor`). It is applied off the
  control loop and restored on quit, on lost connection, and on the next
  launch; and, whatever happens, local input on a machine that is not
  driving the peer shows its cursor again. The previously-noted latent item
  — a brief cursor/keyboard-target mismatch right after a crossing — was the
  visibility cue racing ahead of the capture/placement it stood for; it is
  fixed (feature/95, see Phase 6 Findings). `--no-cursor-mask` turns masking
  off entirely.
- The Phase 4 keyboard caveat still applies: a native editor with its own
  Home/End handling may treat forwarded shifted-navigation its own way.
  (The separate Right-Shift-drops-symbols defect the seamless soak found —
  Right Shift arriving E0-extended — is fixed, not a limitation.)

Record the outcome in the Phase 5 exit-criteria notes (docs/ROADMAP.md):
how many A → B → A cycles, whether control ever split or stuck, how
accurately the cursor landed on entry, whether the clipboard stayed
consistent, and what happened to ownership under induced loss.

## Phase 6 soak: unattended background operation (two machines)

This is the Phase 6 exit criterion: the seamless pair from Phase 5, but running
**unattended via the background service** (ADR 0011) across multiple days and
reboots, with no console babysitting. What Phase 5 proved by hand, Phase 6 must
sustain on its own. Run the Phase 5 procedure *inside* this — the seamless
behavior is what must keep working; this section adds the service, the config,
and the endurance.

### One-time pairing bootstrap

Pairing is a deliberate interactive ceremony (ADR 0002) — one machine shows a
code, you type it on the other — and is **not** automatable; that human step is
the security boundary. So it happens once, up front, and its trust persists:

- Trust lives in the logged-in user's DPAPI store. The service launches the
  worker **as that same console user**, so it reuses the trust with no
  re-pairing. **Pair as the exact Windows user who will be logged in during the
  soak** — pairing as a different user creates an identity the soaking worker
  will not have, and it silently will not connect.
- Pair with the service **stopped** (or before installing it): pairing and the
  worker both bind port 27677, so they cannot both listen at once.
- Confirm with `crossover peers` on each side; each must list the other's
  *current* identity. Revoke stale duplicates with `crossover peers remove
  <id>` so it is unambiguous which identity is live — a regenerated identity (a
  wiped store, or a run under a different user) leaves an old entry that will
  never connect again.

### Configuration (the service reads config.toml, not flags)

The service starts `crossover run` with no arguments, so each machine's role and
side come from `~/.crossover/config.toml` (not CLI flags). Arranged
`A | B` — A left, B right:

Machine A (left screen, listens):

```toml
schema_version = 1
[device]
name = "machine-a"
[network]
listen = "0.0.0.0:27677"
[seamless]
side = "left"
```

Machine B (right screen, dials A at its LAN address):

```toml
schema_version = 1
[device]
name = "machine-b"
[network]
connect = "192.168.1.151:27677"
[seamless]
side = "right"
```

The dialing side (B) owns the reconnect supervisor, so pointing B at A means a
reboot of either machine recovers on its own: B retries until A is back;
A's listener re-accepts when B returns.

### Install and run unattended

On each machine, as the soak user, from an elevated console:

1. `crossover service install` (or `choco install crossover -y -s <source>`).
2. `sc.exe start Crossover` — or reboot; auto-start brings it up.
3. `crossover status` shows `Background service: installed, running`, and a
   `crossover.exe` runs **as the user** (Task Manager → Details → User name).

Then use the machines normally. Over the soak window (target: multiple days):

- Cross A ↔ B through the linked edges per the Phase 5 procedure — seamless
  transfer, keyboard follow, one visible cursor, clipboard riding along — and
  confirm it keeps working across the whole window.
- **Reboot** each machine at least once: the service must auto-start and the
  link re-establish with no manual step and no re-pairing.
- **Pull the network** briefly (unplug or disable the adapter): both workers
  reconnect on their own when it returns.
- **Kill the worker** (`Stop-Process` on the `crossover.exe` running as you):
  the service must relaunch it within ~1 s.

### What good looks like

- Multi-day continuous operation with **no manual intervention**: the service
  stays running, the worker stays up (or is relaunched fast), and seamless
  crossing keeps working the whole time.
- Reboots and transient network loss recover automatically, never re-pairing.
- Clipboard reliability still holds under the soak (Phase 2 requirements).
- No stuck key or button after any transfer, disconnect, or relaunch.

### Honest limitations

- **Headless: no console overrides.** Under the service the worker has no
  console, so the `c` / `r` console commands are unavailable — control transfer
  is edge-driven only. The both-Control **escape still works** (it lives in the
  key path, not the console).
- **Observability is the log file.** The service worker is headless, so its
  console output goes to `NUL` (ADR 0011) — but it also writes structured logs
  to **`~/.crossover/logs`** (daily-rotating `crossover.<date>.log`, a week
  kept), which is where you read back reconnects, control transfers, worker
  relaunches, and errors after a multi-day run. `crossover status` and Task
  Manager give the at-a-glance state; the log files give the history.
  The service daemon itself (`crossover-svc.exe`, the `LocalSystem` process
  that launches and watches the worker) keeps a **separate** log at
  **`%ProgramData%\Crossover\logs`** (`crossover-svc.<date>.log`) — this is
  where a worker's exit code, a crash-vs-intentional-stop classification, and
  backoff/relaunch timing live, distinct from the worker's own protocol-level
  log (ADR 0011 addendum, 2026-08-19).

### Findings

- **UAC / secure desktop (fixed, feature/87).** The first live soak found that
  if the *controlled* machine raised a UAC elevation prompt while a peer was
  driving it, Windows switched it to the **secure desktop** — where a
  user-privilege process cannot inject (by design; you cannot drive a UAC prompt
  remotely) — and the control link **wedged**: neither side returned to local,
  and the controller's masked cursor stayed hidden with no recovery. Root
  cause: there is no liveness once controlling (input batches are
  fire-and-forget), the controlled side could not tell its injection was being
  dropped (`SendInput` reports success even when the secure desktop discards
  it), and the cursor fail-safe skips the controller. Fix: the controlled side
  now detects the secure desktop (`OpenInputDesktop` denies a user process) and
  releases the grant, so the controller returns to local and un-hides its
  cursor, and the log names the reason. Immediate manual recovery is the
  both-Control escape on the controller. A controller-side liveness ack (for the
  case where the controlled side's notification is itself lost) remains a
  possible follow-up.
- **Reversal stall (fixed, feature/93).** Reversing the control direction — be
  driven by the peer, then take it over yourself — stalled: the first edge
  crossing did nothing, the second worked. The linked edge does double duty
  (leave while local, return while controlled) and right after a transfer the
  cursor rests *on* it, so a rising-edge detector primed latched and swallowed
  the next crossing. Position/time fixes both failed on hardware (a few-pixel
  entry inset became a hair-trigger that broke the forward crossing; a firing
  dwell added palpable latency). Fix: genuine local input on the controlled
  machine reclaims control to **neutral** (neither side controlling), told apart
  from the peer's own injection by the system input tick re-baselined on each
  injection. From neutral the cursor is free in the interior, so the next
  crossing is an ordinary rising edge — reversing is "touch this machine, then
  cross," with no dwell and no re-cross (ADR 0009).
- **Elevated / higher-integrity windows (fixed, feature/94, ADR 0012).** With an
  elevated window focused on the *controller*, crossing to the peer hid the
  local cursor but captured and forwarded nothing, and the capture watchdog —
  which only fails closed when raw input flows while the hook is silent — was
  starved of that evidence and left the cursor wedged hidden. Cause: Windows
  UIPI skips a medium-integrity process's low-level hooks over a higher-integrity
  foreground window, and drops injection into one (R-1). The worker ran at the
  user's UAC-filtered (medium) token. Fix: the service now launches it with the
  user's full (elevated) linked token, so it runs high-integrity and its hooks
  and injection reach elevated windows; standard-user sessions have no split
  token and are unaffected. The worker still runs as the user, never SYSTEM (ADR
  0011 invariant intact); the escalation is recorded as SECURITY threat T11.
- **Transfer cue ordering (fixed, feature/95).** Right after a crossing the
  window-focus/keyboard target felt briefly mismatched: the cursor-visibility
  cue was emitted during the engine step, before the transition's capture
  start/stop and cursor placement ran — so the cursor could hide before local
  input was really suppressed (first keystrokes leaking locally), or be shown and
  jump before the return placement landed. Fix: the cue is held and applied only
  *after* those actions complete, so "cursor gone → driving the peer" / "cursor
  back → local" tracks the real capture state. This resolves the Phase 5
  "known latent item" above.

### Outcome: passed (soak 2026-08-11 → 2026-08-14)

The soak ran unattended between machines A and B for four days,
service-launched on both sides, with **no manual intervention and no
re-pairing**. The exit criteria held:

- **Continuous operation.** Seamless crossing, keyboard follow, one visible
  cursor, and clipboard sync kept working whenever both machines were up —
  68 distinct clipboard transactions applied across the two sides, and
  repeated control transfers on every day the pair was running together.
- **Peer-outage recovery, at full scale.** Machine A was off from midday
  08-12 (UTC) to early 08-14, and machine B was itself shut down for
  multi-hour stretches inside that window. Whenever B was up it retried at
  the capped 30 s backoff (one contiguous run reached attempt 139), and the
  session re-established on its own within seconds of both machines being
  back — never with a manual step.
- **Worker relaunch, for real.** Early on 08-11 (02:21–02:40 UTC) machine
  B's worker exited immediately after startup, repeatedly; the service
  relaunched it on the ADR 0011 backoff (1 s doubling to the 30 s cap) until
  the 02:40 launch came up cleanly, computed its topology, connected, and
  applied a waiting clipboard item. Recovery was fully automatic.
- **Failures stayed bounded and observable.** Three inbound clipboard items
  could not be installed (bounded retries — up to five attempts — then a
  logged `clipboard_unavailable`; no hang, no loop). Transient injection warnings
  (SendInput rejected during a blocked-input window, SetCursorPos timeouts)
  were logged and self-cleared. No stuck key or button after any transfer,
  disconnect, or relaunch.

Findings feeding follow-up work:

- **Display topology change mishandled (fixed, feature/107; hardware check
  pending).** The maintainer observed the seamless edge not following a
  monitor unplug/power-off. The edge *geometry* was never the stale part —
  it is re-read from the OS on every 8 ms poll — but three things were:
  the rising-edge detector's at-edge latch was not invalidated by a layout
  change, so an unplug could turn an interior cursor column into the
  linked edge in a single tick and fire a control transfer by itself; the
  only topology log line was the startup one, stale the moment the layout
  moved; and a display change makes Windows reload the system cursors,
  which can un-blank a hidden cursor mask. feature/107 re-primes the
  detector across a layout change (a moved edge is never an arrival),
  logs the new layout, and re-asserts a hidden mask. Under a boot-started
  background service, docking and monitor power-off are everyday events,
  not corner cases. Known residual: when Windows itself keeps a sleeping
  monitor in the layout (common over HDMI), the desktop genuinely still
  extends there and Crossover follows Windows — nothing to detect. The
  two-machine unplug/replug check joins the Phase 7 soak procedure.
- **Silent worker exit (diagnosability).** During the 08-11 relaunch loop
  the worker logged `starting` and nothing else — it exited before reaching
  its run loop without recording why. The window is consistent with a
  locked session or displays not yet available, but the log cannot say; a
  worker that exits before serving should log the reason at WARN.
- **Mislabeled Win32 error text (cosmetic).** Some `SetCursorPos` failures
  carry unrelated `GetLastError` text (e.g. a WSAEWOULDBLOCK socket
  string): the failure is real and transient, but the label is stale errno
  noise and worth cleaning up when touching that path.

## Hardware check: display topology changes (two machines)

The feature/107 fix for the display-topology finding above is fully covered
by unit tests against a scripted display; what only hardware shows is how
*Windows* reports this pair's real monitors through a change. With the pair
running seamlessly (machine A has the external monitor):

1. **Unplug the external monitor** (cable out, not power) while the cursor
   sits on the laptop panel. Within a health tick the log must show
   `display topology changed` with the single-monitor layout — and nothing
   else: no control transfer fires on its own, and the peer stays where it
   was.
2. **Cross afterwards.** The laptop's own edge is now the linked edge;
   crossing it must transfer as usual and return cleanly.
3. **Replug.** The log shows the two-monitor layout again; crossing now
   happens at the external monitor's far edge, not the laptop's.
4. **Unplug while driving the peer** (cursor hidden). The mask must stay
   blanked through the change (the re-assert), and the both-Control escape
   must still return control.
5. **Monitor power-off instead of unplug.** Note what the log shows: over
   DisplayPort the monitor usually leaves the layout (same as unplug);
   over HDMI Windows may keep it, in which case the desktop genuinely
   still extends there and the edge staying put is correct — record which
   this hardware does, so the earlier finding's residual is documented
   for this pair.

## Phase 7 hardware validation: clipboard images (two machines)

ADR 0014's platform slice is the only part of image transfer that
automation cannot finish. Everything up to the OS boundary is covered by
tests — chunking and reassembly, the negotiated send gate, a fabricated
DIB round-tripping through Win32 verbatim, refusal above the ceiling. What
no test can reach is what a *real* application publishes onto the clipboard
and whether a *real* application accepts what Crossover installs.

Run this after the Phase 6 soak closes and before the branch merges.
`FeatureFlags::ADVERTISED` is `ALL` on this build, so image transfer is
live between two peers that both run it.

### Single machine first (cheap, catches the obvious)

```
cargo test -p crossover-platform-windows -- --ignored
```

1. `manual_a_real_snip_is_read_as_a_stable_image` — take a snip
   (`Win+Shift+S`) first. It must read as `CF_DIB`, inside the ceiling,
   with two consecutive reads byte-identical. **An unstable length here
   predicts a sync loop**, so treat any mismatch as release-blocking.
2. `manual_an_installed_image_pastes_into_other_applications` — then paste
   into Paint, Word, and a browser compose box. The blue/green gradient
   must appear in each.

### Then the pair

With both machines running (Phase 5/6 setup), on machine A:

1. **Snip → paste.** `Win+Shift+S`, snip a region, paste on B into Paint
   and into Word. The image must arrive within a second on the LAN and be
   pixel-identical to the snip.
2. **The other direction**, same check.
3. **No loop.** After a transfer settles, watch both sides for 30 s: no
   further clipboard traffic, and `clipboard_loop_suppressed` in the
   metrics increments once per applied item, not repeatedly. Any repeating
   offer/apply cycle is release-blocking.
4. **Mixed content.** Copy a block of cells from Excel on A, paste on B.
   The *text* must arrive (the deliberate precedence rule), not a picture
   of the cells.
5. **Big image.** Screenshot a full 4K display (`PrtScn`), paste on B, and
   confirm arrival and that live mouse/keyboard stay responsive **while it
   transfers** — that is ADR 0013's preemption claim, on real bytes.

   **Then measure it, because "felt smooth" is not the exit criterion.**
   Keep the mouse moving and type continuously for the whole transfer, so
   input is genuinely competing with the bulk stream. On quit (`q` /
   Ctrl-C), the controlling machine's shutdown block ends with:

   ```
     input:      12043 events sent (86 keys), 0 events received (0 keys)
                 queue-to-wire avg 41us, max 2.8ms (over 12043 frames)
   ```

   That is how long an input frame waited between being handed to the send
   path and reaching the wire — queueing *and* the writer's wait behind a
   bulk frame already in flight, which is what ADR 0013 and ADR 0014's
   chunking jointly bound. Record both numbers in the run's notes.

   **You do not have to quit to get them.** The same figures are written to
   `~/.crossover/logs` every 15 minutes while the run continues, so a
   service-launched soak leaves them behind however it ends — including
   `Stop-Service`, which terminates the worker outright and skips the
   shutdown block entirely (ADR 0011). Interim records are marked, and every
   field is cumulative, so the interval containing the big transfer is the
   one where the maximum steps up:

   ```powershell
   Select-String 'execution metrics' "$env:USERPROFILE\.crossover\logs\crossover.*.log" |
     ForEach-Object { $_.Line -replace '.*(interim=\S+).*(input_queue_max_us=\S+).*', '$1 $2' }
   ```

   **Record the link type with the numbers.** ADR 0013's chunk-size
   arithmetic assumes wired 2.5 GbE; a figure taken over WiFi is measuring a
   different thing, and the 2026-08-16 reading (mean 1.94 ms, max 309.8 ms)
   was wireless. A reading without its link is not comparable to anything.
   The wired reading that closed the criterion — and how the window was
   verified clean before it was believed — is the input-latency section at
   the end of this file.

   The block also attributes the wait:

   ```
                 queue-to-wire avg 1.9ms, max 309.8ms (over 7571 frames)
                   waiting for the writer avg 1.2ms, max 305.0ms; for the socket avg 0.7ms, max 12.3ms
   ```

   **Waiting for the writer** means the frame sat while the writer was busy
   with something else — bulk in flight, which a dedicated writer task would
   fix. **Waiting for the socket** means these few dozen bytes themselves
   took that long to be accepted — backpressure or the link, which no local
   scheduling improves. Which half dominates decides what to do about it.

   **Trust the maximum, not the mean.** Every field is cumulative, so the
   mean falls as idle input accumulates after the transfer — a real reading
   went 1942 -> 1424 -> 1248 -> 797 -> 586 µs across successive records
   while nothing improved. Difference two records to get the interval, or
   read the max, which only moves when something worse happens.

   **What good looks like:** a mean in the tens of microseconds, and a
   maximum in single-digit milliseconds. A maximum in the *hundreds* of
   milliseconds means interactive frames queued behind bulk — the lane
   split is not doing its job, and that is release-blocking however smooth
   the session felt. `0 events sent` with no latency line means the
   measurement did not happen: input flows from the machine in control, so
   read this block on the machine you were driving *from*.
6. **Over the ceiling.** A capture larger than 64 MiB (a dual-4K span) must
   be skipped with a log and **must not** disturb the session: copy text
   immediately after and confirm it still synchronizes.
7. **A copy during a transfer.** Copy something new on A while a large
   image is still streaming; the newer item must win and the older one must
   close out rather than stall.
8. **Interop with a text-only peer**, if one is available: a build without
   the feature bit must still synchronize text and simply never receive
   images.

If clipboard operations fail wholesale with "Access is denied", read the
troubleshooting section above before concluding anything.

Record the outcome in the Phase 7 exit-criteria notes (docs/ROADMAP.md):
which applications accepted the pasted image, observed transfer times by
size, whether input stayed responsive during transfer, and any loop or
stall seen.

## Phase 7 hardware validation: files (two machines)

ADR 0015's platform slice — the spool, the STA thread and its
`IDataObject`, the zone marking, and the loop-prevention layers — has the
same shape of gap images had: everything up to the OS boundary is
hermetic (docs/TESTING.md §1.6), and what only two real machines and a
real wire can show is a real file crossing it. This is that session.

### Outcome: passed (2026-08-18 → 2026-08-19)

The session ran between machines A (listener) and B (dialer) over a
direct 2.5/10 GbE link, initial build `e689a3b`, final retests on the
build carrying PRs #43–#46:

- **Single-file transfer, both directions**, original name preserved,
  byte-identical content; opens without SmartScreen or Protected View but
  carries `ZoneId=1` in `Zone.Identifier` (ADR 0015's 2026-08-17 zone
  decision, confirmed live).
- **Folder and multi-selection transfer**: one Stored-entry zip per
  selection, contents identical after extraction on the receiving side.
- **`AlreadyHave` dedup**: a re-paste of identical content settled
  near-instant, with nothing on the wire.
- **Refusals, typed and observable**: a >256 MiB selection refused as
  `TooLarge` on the sender, observed live at 02:00:44 UTC — 268,500,992
  bytes against the 268,435,456-byte `MAX_CLIPBOARD_FILE_BYTES` — with
  nothing partial sent and the clipboard path unwedged afterward. (The
  junction/reparse-point and >256-entry refusal cases are covered by the
  automated suites; the hardware run's live refusal exercised `TooLarge`.)
- **Loop prevention**: no un-commanded offers appeared during offer-held
  and post-paste watch windows, sender clipboard content was preserved,
  and a deliberate provocation — copying a file from inside the spool
  path — was suppressed silently, `clipboard_loop_suppressed` incrementing
  exactly once.
- **docs/TESTING.md §1.6's third exception (F16, invariant 7),
  wire-crossed**: the received offer does not appear in Win+V clipboard
  history and does not cloud-sync; the pasted file opens unprompted with
  `ZoneId=1`.
- **Throughput and responsiveness**: a 200 MiB / 130-entry zip packed in
  ~2 s on the sender; input stayed responsive throughout large transfers,
  p50 input-path latency ~3 ms during sends.

### Defects found and fixed (all merged to `dev`)

1. **Edge-transfer bounce.** Entry placement on the return-trigger
   column, with a 1 px re-arm margin, caused take/revoke cycles down to
   ~150 ms under a hand tremor at the seam — ADR 0009's deliberately
   deferred push-through risk materializing. Fixed by PR #44
   (feature/137): re-arm hysteresis (`REARM_MARGIN` = 24 px) plus
   generation-stamped crossings.
2. **Control-request lockout.** A ~4.7 s-late answer caused a 3 s
   timeout, then a retry was denied `AlreadyControlled` by the very grant
   it held — a ~7 s lockout that self-healed. Fixed by PR #45
   (feature/139): the grant-holder's retry now refreshes its grant
   instead of being denied, a timeout cancels the request on the wire
   rather than only locally, and the stray-grant undo was narrowed.
3. **Inbound head-of-line block.** Control frames were broadcast to, and
   gated on, the clipboard driver's queue — ADR 0013's interactive/bulk
   separation was outbound-only, with no inbound equivalent. Fixed by
   PR #46 (feature/140): routing by frame type, edge mode carried as a
   watch level with the generation in the value, and placement re-primed
   on the crossing. Recorded limit: there is still no inbound preemption
   of a genuinely saturated same-driver queue, by SPECIFICATION.md §2's
   priority order.
4. **Silent worker death on B, 02:05:38 UTC — an open observation, not a
   resolved defect.** The peer saw a TCP RST; the service relaunched the
   worker in ~1.3 s, the session re-established, no input was left stuck,
   and Windows Error Reporting has no record of it. The log tail that
   might have explained it was lost to the non-blocking appender. **Root
   cause remains unknown** — the service observed the exit code but
   recorded it nowhere at the time. Fixed forward, not diagnosed, by
   PR #43 (feature/138): a durable supervision log in
   `%ProgramData%\Crossover\logs` naming exit codes and stop reasons (ADR
   0011 addendum, 2026-08-19). A recurrence is now diagnosable; this one
   was not.

### Retests on the post-#46 build

The seam no longer bounces under deliberate wiggling (deliberate
crossings remain instant); ~20 rapid crossings ran with zero timeouts and
zero denials.

### Deliberately skipped

The mid-transfer network-disconnect hardware case was not exercised: at
2.5 Gbps a 200 MiB transfer completes sub-second, leaving no practical
"mid" for a manual disconnect to land in. Coverage rationale:
engine-level fault injection is the primary evidence for FR-6.x per
docs/TESTING.md §1.5, and the 02:05:38 incident above was a real
abrupt-disconnect recovery observed live — clean in ~1.3 s, no stuck
input, no partial state — even though it was not itself a file transfer.

### Operational notes

- **`file_receive` must be granted on both machines.** It is default-off
  per ADR 0015 and grants only the receiving direction — a two-way test
  needs `crossover peers allow-files <device-id>` run on both sides. The
  first attempt on this session failed `NotPermitted` by design, because
  it had only been granted on one.
- **Clock skew between the machines was ~0.9 s**, which complicated
  correlating the two logs by timestamp during the worker-death
  investigation. Recommend NTP sync ahead of future soaks so cross-machine
  log correlation does not need manual offset arithmetic.

## Phase 7 hardware validation: input latency on a wired link (two machines)

This is the images section's step 5 measurement — queue-to-wire input
latency under a saturating bulk transfer — taken on the link ADR 0013's
arithmetic was written for. The 2026-08-16 reading failed the criterion
(mean 1.94 ms, max 309.8 ms) but was taken over **WiFi**, so it could not
distinguish "the chunking is too coarse" from "the link is slow", and the
chunk size was held at 64 KiB pending exactly this run.

Two things make this run different from the informal wired reading taken
during the files session, and both are the point:

- **The contention is on one writer.** The sending machine drove
  interactive input *and* bulk file data over the same connection at the
  same time. Bulk from a machine that is not also sending input measures
  nothing about preemption.
- **The window is verifiably clean.** A link drop inside the window puts a
  reconnect stall into the maxima and quietly invalidates the reading.

### Outcome: passed (2026-08-20 → 2026-08-21)

Direct wired link, machine A (Intel I225-LMvP 2.5 GbE, dock-attached,
listener, 192.168.3.20) ↔ machine B (10 GbE NIC, dialer, 192.168.3.10),
negotiating 2.5 Gbps full duplex. Both machines ran `dev` at `f69afc8`
(post-PR #48).

### The procedure

1. **B's service restarted at 00:04:06 UTC** so its cumulative counters
   started from zero. That instant is the measurement start, T.
2. **Ten distinct 200 MiB random-content files**, copied on B and pasted
   onto A back-to-back between 00:04:34 and 00:05:13 UTC — 39 s in total,
   ~1 s per delivery. Distinct content, so hash-dedup could not shortcut
   any of them into a no-op.
3. **Continuous input throughout**: B held control of A and kept mouse and
   keyboard moving for the whole 39 s, so every transfer competed with live
   input on B's writer.
4. **Hands off afterwards** until the interim `execution metrics` line at
   T+15 (00:19:07 UTC), which is the record read below.

### Results

B's interim metrics line at 2026-08-21T00:19:07Z: 4,558 input samples over
4,561 input events, `frames_sent=36,732`, `bytes_sent=2,098,396,126`,
`clipboard_files_sent=10`, `clipboard_file_sent_bytes=2,097,152,000`.
File-delivery latency was p50 906 ms / p95 1080 ms.

| | avg | max |
|---|---|---|
| socket accepting the bytes (`input_write`) | **0.019 ms** | **0.147 ms** |
| waiting for the writer (`input_lane`) | 0.41 ms | 72.2 ms |
| queue-to-wire, total (`input_queue`) | 0.43 ms | 72.2 ms |

**Clean-window verification** (do this before believing any of the above):
A's log shows one unbroken session spanning the whole T → T+15 window, and
B reports `reconnect_attempts=0`. So every input sample coexisted with
genuine transfer traffic and nothing else. An earlier attempt the same day
**was** polluted — an environmental NIC link drop landed inside the window —
and was discarded rather than reported.

**Verdict: pass, and the chunk size is settled.** The worst socket write
under full saturation was 0.147 ms — below the 0.21 ms ADR 0013 costs a
*single* 64 KiB chunk at 2.5 GbE, and against 1.94 ms mean / 309.8 ms max
over WiFi. The WiFi failure was the physical link, not the chunking design.
**The 64 KiB chunk size stands** (maintainer, 2026-08-20), recorded in ADR
0013's 2026-08-20 addendum and ARCHITECTURE.md §5.4; the writer-task
redesign that measurement was meant to price is not warranted.

### The one open observation

A **single ~72 ms tail event** in the interactive lane, while socket writes
stayed at or below 0.147 ms. That places it *before* the writer — a one-off
scheduling or queueing stall — and not behind bulk bytes in the socket,
which is the failure this measurement exists to catch. One outlier among
4,558 samples, against averages of 0.41 ms (lane) and 0.43 ms (total); the
operator perceived nothing at the time. Recorded as a future investigation
in docs/ROADMAP.md's Phase 7 follow-ups, not as a blocker.

### Also settled this session: the silent worker death is explained

The 2026-08-19 02:05:38 UTC worker death on B — recorded above as an open
observation with no root cause — **recurred on 2026-08-20 at 02:06 UTC**,
this time with PR #43's durable supervision log running. It reads
unambiguously: the worker exited **`0x40010004`
(`DBG_TERMINATE_PROCESS`)** immediately after a session-change
notification, and the service then recorded `reason=Logoff`. Windows was
tearing the worker down at user logoff. **Environmental, not a crash** —
which is why Windows Error Reporting had nothing on it and why the process
left no panic behind. The fix-forward from the files session did exactly
what it was built to do: the recurrence was diagnosable in one read.

Two cosmetic follow-ups fall out of it, neither scheduled:

- The service **relaunches the worker into the dying session** before it
  sees `Logoff`, because it acts on the session-change notification first.
  The relaunch fails harmlessly, but it is noise in the log at every
  logoff.
- `0x40010004` is **labelled `crashed=true`**, when at logoff it is Windows
  terminating the worker deliberately. It misleads whoever reads the log
  next.

### Environmental: machine A's dock-attached NIC flaps

Worth recording because it cost real investigation time and will again.
Machine A's I225 NIC is attached through a USB4 dock and **flaps
chronically**: Windows `e2fnexpress` events 27/57 across 2026-08-18 → 08-20
match Crossover's session drops **to the second**, and the NIC itself logs
corrected PCIe AER errors. Outages ran 4–20 s.

Both peers had logged these as `10054` — "forcibly closed by the remote
host" — which reads like the peer misbehaving and is why PR #48 added
`local_link` diagnostics. **PR #48 is validated by this session**: A's
session-end lines now stamp `local_link="up"` or `"down"`, and during this
session they correctly showed `"up"` — the drops that evening were not A's
wire. A session end that says `10054` with `local_link="down"` is your own
NIC; one that says `10054` with `local_link="up"` is not.

**Crossover's recovery behaved correctly through every one of these drops**:
reconnect in ~10–11 s (dominated by 2.5 GbE autonegotiation, not by
Crossover's backoff), no stuck input, no clipboard loops.

### Operational notes

- **Restart the service to zero the counters** before measuring. Every
  metrics field is cumulative, so a run started from a long-lived worker
  reports maxima belonging to some earlier event.
- **Read the metrics on the machine you were driving *from***, and check
  its `reconnect_attempts` and the *other* machine's session continuity
  before trusting a maximum. That check is what separated this run from the
  discarded one.

## Phase 8 soak: the drawn display topology (two machines)

These are the Phase 8 exit criteria, and the loop the phase exists to close:
an arrangement **drawn** at one desk decides where the cursor crosses at
both. Everything up to the OS and the wire is hermetic — the layout model
and its validation, the adjacency derivation, per-span hysteresis, the
`(revision, origin)` resolver and its hash tiebreak, and every screen the
editor can paint (docs/TESTING.md §3.2, ADR 0019's headless pass). What
only two desks can show is whether the picture matches the desk: whether
the seams the user drew are the seams the cursor finds, whether a dock, an
unplug or a monitor going to sleep moves them correctly under a live
session, and whether two machines that disagree converge where a human can
see it happen.

docs/TESTING.md §3.2's **E-7 is this section's Pass 1**, and is listed
there precisely so a single-machine release checklist knows it has not
covered it. E-1 to E-6c are the single-machine editor checks and are *not*
repeated here — run them first, on machine A, because a broken canvas
wastes a two-desk session.

### Setup

The standing pair from Phase 6/7, now on a different subnet and with a
bigger desk (2026-09-01): machine **A** (development machine,
`192.168.3.20`, **three monitors** — two `DELL U2723QE` panels, one of
them portrait, beside the laptop's `Internal Display`, mixed DPI) listens;
machine **B** (`192.168.3.10`, one monitor) dials. Both under the
background service (ADR 0011), which is how the pair actually runs. B is
not permanently on the LAN — it moves between offices and sleeps — so a
session that drops and re-establishes every fifteen to ninety minutes is
this pair's normal regime, not a fault to chase; A's log shows it as
`session ended … os error 10054` followed ~12 s later by
`session established`.

Two consequences of that desk for the checks below. Wherever a check says
"A's two monitors", read it as any adjacent pair of A's three. And the
arrangement saved on both machines before this session (revision 12,
drawn on B) places only **two** of A's screens — the third was added
afterwards and is `unplaced` in the editor — so `edge: none of this
machine's live screens matches the drawn arrangement` appears
transiently at every dock, sleep, and wake on A. That is check 2.5's
diagnosed behaviour, not a finding; **redraw the arrangement for the
three-screen desk in Pass 1 before reading anything else**, and only then
count entry-point warnings.

Five things to do before starting, each of which has cost time when
skipped:

1. **Build both machines from the same commit.** Protocol v4 raises the
   floor as well as the ceiling (ADR 0018, ADR 0017's rule), so a v3 peer
   is refused at `Hello` with a version-range mismatch and the session
   never establishes. A mixed pair does not connect at all — that is the
   designed behaviour, not a fault to debug.
2. **Install all three binaries.** `crossover.exe`, `crossover-svc.exe`
   and now `crossover-layout.exe`, which must sit **beside**
   `crossover.exe`: `crossover layout` resolves the editor as a sibling of
   the running executable and never consults `PATH` (ADR 0019). A missing
   editor is a clear error naming the path it looked at.
3. **Write down which physical screen is which device string**, on both
   machines. Every diagnostic in this section names `\\.\DISPLAY1` and
   friends, never "the left one", because the device string is what the
   layout is keyed on (ADR 0018). `crossover layout`'s canvas labels each
   rectangle with it; note them against the monitors on the desk.
4. **NTP-sync both machines.** Carried forward from the files session: a
   ~0.9 s skew made cross-machine log correlation manual arithmetic, and
   this section correlates two logs constantly.
5. **Know the three log sinks.** The worker's own log is
   `~/.crossover/logs/crossover.<date>.log`; the service's supervision log
   is `%ProgramData%\Crossover\logs`; and — new this phase — the **editor**
   writes to `~/.crossover/logs` too (ADR 0019's 2026-08-21 amendment),
   because a release editor is a GUI-subsystem binary with no console to
   report to. A diagnostic from any of the three binaries of one install
   lands in one directory.

**One thing to settle before the first check, because half the diagnostics
below come in pairs.** Most layout messages are dual-channel: a `tracing`
line with structured fields, and a plain sentence on the console. Under the
background service the worker **has no console** (SOAK's own Phase 6
limitation — it is launched with no window and its output goes to `NUL`,
ADR 0011), so the console halves are visible only in a **foreground
`crossover run`**. Under the service, read the log; the two carry the same
facts, and only the log is guaranteed. The same applies to the `c` / `r`
console commands, which do not exist under the service — the both-Control
escape does, because it lives in the key path rather than the console.

If a check below wants both halves, run that machine in the foreground for
it. Everything else in this section is designed to be read out of
`~/.crossover/logs`.

Most checks below are read out of the worker log. The two greps used
throughout:

```powershell
$log = "$env:USERPROFILE\.crossover\logs\crossover.*.log"
Select-String 'layout sync:|topology:' $log | Select-Object -Last 40
Select-String 'display topology changed|edge:|control: ' $log | Select-Object -Last 40
```

The run's metrics block also carries a topology line, printed by any run
that sent, adopted or refused an arrangement — and printing all three
counts including the zeroes, so an unreported sync is visible:

```
  layout:     2 sent, 1 adopted from the peer, 0 rejected
```

---

### Pass 0 — the implicit regression (nothing drawn yet)

**Exit criterion 5** (no regression in seamless transfer's existing
guarantees), and the baseline everything after it is measured against. Run
the pair exactly as Phase 6/7 left it: both machines on the deprecated side
model, A `--left` (or `[seamless] side = "left"`), B `--right`. Nothing in
this pass may differ from Phase 6/7 behaviour.

**Expect deprecation warnings, and read them as a pass, not a fault.** A
flag produces, at `warn` in the log and — in a foreground run only — on
stderr:

```
deprecated: draw an arrangement with `crossover layout` instead (ADR 0018)   flag="--left"
Warning: --left is deprecated; draw an arrangement with `crossover layout` instead (ADR 0018).
```

A `[seamless] side` key in the config file produces the same pair worded
for the key (`deprecated: [seamless] side is retired by ADR 0018; …`). One
warning per run per source. Their **absence** is the anomaly here, not
their presence.

| # | Check | What a pass looks like |
|---|-------|-------------------|
| 0.1 | **Cross A → B.** Drive A's cursor into the outer right edge of A's desktop | Control transfers on its own; B's cursor appears at B's left edge at the matching height; A's pointer freezes. Exactly as Phase 5 |
| 0.2 | **Cross back.** Drive B's cursor into B's left edge | Control returns; A's pointer comes back at the matching height. Repeat a dozen times, mouse only |
| 0.3 | **A's internal seam stays inert.** Drive across the seam between A's two monitors, both directions, several times | Nothing transfers. The side model's one-desktop treatment is intact, and this is the behaviour Pass 1 will deliberately change |
| 0.4 | **The escape chord.** While A controls B, press both Control keys | Control returns instantly; no Control lands on B; A's keyboard is alive |
| 0.5 | **Cursor mask.** Watch the cursor through a dozen cycles | Exactly one cursor, on the machine being driven. No cycle leaves none for more than a moment; local input on a hidden-but-not-driving machine restores it |
| 0.6 | **Dock/undock and worker crash-relaunch.** Undock A (or unplug its second monitor), cross, replug; then `Stop-Process` A's `crossover.exe` | `display topology changed; the crossing spans follow the new layout` appears within a health tick, with no transfer firing by itself; the service relaunches the worker within ~1 s (`%ProgramData%\Crossover\logs`); the session re-establishes with no re-pairing |

*A failure in this pass, in any of its shapes:* a crossing that does not
fire or does not return; a stuck key, button, or hidden cursor after any
cycle; a transfer firing by itself across the dock/undock in 0.6; a
relaunch that does not happen or that needs a re-pair; or A's internal seam
in 0.3 transferring anything at all.

**A failure in Pass 0 stops the session**: it is a regression against a
closed phase, and nothing measured after it would mean anything.

---

### Pass 1 — the drawn layout (E-7)

**Exit criteria 1 and 2.** This is the pass the phase exists for. Draw the
real desk, save it, watch it reach the other machine, then check that the
cursor crosses where the drawing says and nowhere else.

#### 1.1 Draw it

On **A**, `crossover layout`. Drag A's group and B's group into the
arrangement the monitors physically sit in, and **snap the seams** — the
status bar names each catch (`Snapping machine-b: edges meet`), and
abutment is exact with zero tolerance (ADR 0018), so a seam that did not
visibly snap is a wall. Then Save.

*A pass:* the Save button was enabled (`Unsaved changes`, not `Cannot be
saved yet`), and the status bar reports
`Saved (revision N). The worker picks it up shortly.` The editor log
carries `saved the drawn arrangement to the config file` with `revision`.

*A failure:* a blocking diagnostic that names no rectangle, a snap that
catches nothing at either zoom, or a rectangle that creeps away from the
pointer while held (E-3's single-machine check, escalating here).

#### 1.2 The worker re-reads it — no restart

*Log line, on A, within ~2 s* (the config modification-time poll):

```
topology: config re-read picked up a changed layout   revision=N origin=…
layout sync: the config file now names a newer arrangement   revision=N origin=…
```

*A failure:* nothing within a few seconds, or
`topology: config re-read has an invalid [layout]; keeping the last good one`
— the editor wrote something the worker will not take, which is a defect in
the pair of them, not a user error.

#### 1.3 The `LayoutSync` goes out

*Log line, on A:*

```
layout sync: stated this machine's arrangement to the peer   session=… revision=N origin=…
```

and `layout: 1 sent, …` in the next metrics record.

#### 1.4 B adopts it, observably at both ends

*Log line, on B* — the adoption, in one of **three** shapes. Which one
depends on whether B held an arrangement of its own, and on whether the
narration rate limit is open:

```
layout sync: adopted the peer's arrangement; this machine held none   adopted_revision=N adopted_origin=…
layout sync: adopted the peer's arrangement; the one this machine held is superseded
    adopted_revision=N adopted_origin=… superseded_revision=M superseded_origin=…
layout sync: adopted the peer's arrangement (narration rate-limited)
    adopted_revision=N adopted_origin=…
```

The first two are at `warn`/`info` and carry a matching console sentence in
a foreground run (`Adopted the display arrangement drawn on the peer
(revision N)…`). **The third is at `debug` and has no console half**: one
narration per five seconds is allowed, and the rest are still recorded, one
level down. Rapid editing will reach it — check 3.3 and any burst of saves
— and **a quiet adoption must not be read as a missing one**. Turn the log
level up (`RUST_LOG=debug`) for any pass that edits quickly, or the
adoptions after the first will look like silence.

At `debug`, `layout sync: resolved the peer's arrangement` names the
resolution label and both keys, which is the line to read when a resolution
surprises you.

**On a first-ever adoption onto an implicit run, B also logs the honest
cost** (ADR 0018's 2026-08-21 amendment):

```
layout sync: this run crosses by no drawn arrangement, so the adopted one takes
effect at the next start (it is saved to the config now)
```

That is not a failure. Restart B's worker once and continue — it applies
once per machine, not per edit, and it is why this pass restarts B before
1.6.

#### 1.5 B's config is upgraded and persisted

*Check on B:* `crossover config` shows `schema_version = 2`, a `[layout]`
section with `revision = N` and the same `origin`, **and no `[seamless]
side` key** — adoption counts as the first write, which is what performs
the schema 1 → 2 upgrade. Comments and other sections must survive
verbatim (`toml_edit`, not serialize-and-truncate).

*A failure:* a lost comment, a mangled `[network]` section, or a `side` key
still present alongside `[layout]`.

#### 1.6 The seam the side model could not express

Draw **B between A's two monitors** (`A1 | B | A2`), save, and let both
sides adopt. Then drive A's cursor from A1 into what used to be an internal
seam.

*A pass:* control transfers to B at that seam; driving on into B's right
edge crosses to **A2**; and A1's *outer* left edge — a wall in this drawing
— transfers nothing. This is deliverable 2, and it is the single check that
most clearly separates this phase from ADR 0009.

*A failure:* the crossing still happens only at A's outer desktop edge (the
worker is still on the side model — check 1.2 and 1.4 again), or the seam
fires but the cursor arrives on the wrong screen (`control: no live monitor
here is named by the arriving entry point…`, below).

#### 1.7 A three-monitor corner

Draw one deliberately: stack A1 above A2 and place B to the right so its
left edge meets the point where A1's bottom-right corner touches A2's
top-right corner. Save, adopt, then approach that exact corner from each of
the three screens, slowly, several times.

*A pass:* every approach resolves to exactly one destination, the same one
each time from the same direction. Spans are half-open `[start, end)`, so
the shared coordinate belongs to exactly one span by arithmetic — a corner
that answers differently on different approaches is the failure this
check exists for.

*A failure:* an approach that fires nothing, or one that alternates
destinations.

#### 1.8 Mixed DPI: 40 % in, 40 % out

**Exit criterion 2**, and the one to measure rather than eyeball. With A's
4K monitor at 150 % beside B's 1080p at 100 % (or whatever this desk's
mismatch is), put a **physical ruler or a taped mark at 40 % down the
drawn seam** on the source monitor. Cross there, five times, from each
side.

*A pass:* the cursor arrives within a few percent of 40 % down the
destination edge, every time, in both directions. The mapping is
proportional through the *drawn* edges — units cancel and no scale factor
enters (ADR 0018) — so a systematic offset is a real defect, not rounding.

*A failure to recognize specifically:* an arrival that is correct on the
tall monitor and wrong on the short one is the desktop-bounding-box
mapping leaking back in (docs/PROTOCOL.md §6.1's "the fraction is taken
against that **monitor**, not against the box"). Capture the `control:
placing cursor at the arriving entry point` debug line — it carries
`monitor`, `edge`, `fraction`, `revision`, `x`, `y`, which is enough to do
the arithmetic by hand.

#### 1.9 Rapid crossings at a span boundary (the bounce class)

The Phase 7 files session found the edge-transfer bounce (PR #44); per-span
hysteresis is where that property now lives, and a regression reproduces
the oscillation. On a **multi-span edge** — 1.6's `A1 | B | A2` has one, and
1.7's corner has two adjacent spans — do three things at the boundary:

1. **Wiggle across it laterally**, staying hugged against the edge, sliding
   from one span into its neighbour. *Nothing should fire*: lateral motion
   clears nothing, so the neighbour was never armed.
2. **Cross deliberately, twenty times in a row**, alternating spans.
3. **Park the cursor on the entry column** after a crossing and let a hand
   tremor work on it for ten seconds.

*A pass:* deliberate crossings remain instant; the wiggle and the tremor
produce no transfer at all; zero control-request timeouts and zero
`AlreadyControlled` denials over the twenty crossings.

*A failure:* take/revoke cycles in the hundreds of milliseconds — the
`REARM_MARGIN` (24 px, perpendicular) is not being applied per span.

#### 1.10 An inert edge portion — part of an edge is a wall

Draw B so it abuts only **part** of A1's facing edge (a short monitor
against a tall one, offset vertically). Save and adopt.

*A pass:* pushing the cursor at the abutting portion crosses; pushing at
the portion above or below it does nothing at all, repeatedly, and the
cursor simply stops at the desktop boundary. Connectivity is deliberately
not required (ADR 0018) and a free edge is a legal drawing, not an error.

*A failure:* a crossing from the non-abutting portion — a tolerance has
crept into the derivation, which is exactly what ADR 0018 refused.

---

### Pass 2 — the arrangement changing under a live run

**Exit criterion 3**: a display added, removed, or rearranged at runtime
updates the layout **without a restart and without a stuck cursor** —
feature/107's property, with considerably more to get wrong now that a
screen carries spans rather than the desktop carrying one edge.

| # | Check | What a pass looks like |
|---|-------|-------------------|
| 2.1 | **Undock / replug mid-session, cursor local.** With the drawn arrangement live, unplug A's second monitor, then replug | Within a health tick: `display topology changed; the crossing spans follow the new layout` with `monitors` and `crossings`. **No transfer fires by itself** — the detector re-derives and re-primes, never emits, across a layout change. Crossing works immediately afterwards at whatever seams survive, with no restart |
| 2.2 | **Undock mid-control** (this machine is *being driven*) | The cursor mask stays blanked through the change (the re-assert), and the grant is still endable: the both-Control escape at the controller returns control, and genuine local input on the controlled machine reclaims to neutral. **No stuck key, no stuck button, no cursor left hidden** — this is criterion 5 inside criterion 3, and it is release-blocking |
| 2.3 | **Monitor power-off instead of unplug.** Power the monitor off, wait a minute, power it on | Record what *this* hardware does, per link type: over DisplayPort the monitor usually leaves the layout (same as unplug); over HDMI Windows often keeps it, in which case the desktop genuinely still extends there and the spans staying put is correct. The Phase 6 residual, now re-measured against a drawn layout |
| 2.4 | **The editor reflects it live.** Leave `crossover layout` open on A throughout 2.1 | The new screen appears within a second or two, and an unplugged one disappears the same way — **alongside** an unsaved drag, not instead of it (E-6c, here against a real second machine) |
| 2.5 | **A partially unmatched arrangement.** Power off or unplug the monitor a drawn span depends on, and leave it off | The drawn-but-absent monitor **keeps its place** on the canvas but loses its native-resolution line from the label — the mirror of the ghosted `(unplaced)` treatment a *live* monitor the drawing does not place would get. The worker logs the degradation rather than going quiet. Crossings through that screen's spans stop; every other span keeps working. Nothing is *rejected* — an arrangement legitimately names screens that are not attached right now (ADR 0018's 2026-08-21 amendment), which is what lets a drawing survive an undock |

*A failure in this pass:* a transfer firing by itself on a layout change
(the detector re-derived but did not re-prime — a moved edge must never be
an arrival); a cursor left hidden after 2.2, or a grant that cannot be
ended by any surviving route; crossings that stop working entirely after a
change and only return on a restart; an editor that needs a save before a
docked monitor appears, or that drops an unsaved drag to show one; or a
change that produces no `display topology changed` line at all, which makes
every later symptom undiagnosable.

The lines Pass 2 is anchored to, all from the worker log:

```
display topology changed; the crossing spans follow the new layout   monitors=[…] crossings=…
edge: none of this machine's live screens matches the drawn arrangement; crossing by
  cursor is off until one does (local input, the console, and the escape gesture still
  end a grant)                                                revision=… publication=… monitors=…
edge: the display stopped reporting its monitors; seamless detection is suspended
  until it answers again
edge: the display is answering again; seamless detection resumes
```

and, when a whole desk goes unmatched at the moment of adoption:

```
layout sync: the adopted arrangement names none of this machine's attached screens, so
  nothing here crosses anywhere until one of them comes back   revision=… drawn=[…] attached=[…]
```

**During any edit's propagation window, expect degraded placement and read
it as designed.** For the few seconds between one machine adopting a new
revision and the other doing so, crossings carry an `EntryPoint` stamped
with a revision the receiver does not hold, and the receiver says so:

```
control: the entry point was derived from a layout revision this machine does not hold;
  placing on the desktop-bounds edge instead — the transfer itself is unaffected
                              monitor=… sender_revision=… local_revision=… edge=…
control: no live monitor here is named by the arriving entry point; placing on the
  desktop-bounds edge instead — the transfer itself is unaffected
```

Placement degrades; **control never does**. A grant that fails, splits, or
hangs in that window is a real defect; a cursor landing at the desktop edge
instead of the drawn one, briefly, is not.

---

### Pass 3 — disagreement, and surviving a restart

**Exit criterion 4**: the arrangement survives restart, and two machines
that disagree **resolve observably** rather than silently mis-crossing.

#### 3.1 Edit on B while A is disconnected

Pull B's network (disable the adapter). With the link down, open
`crossover layout` on **B** — it must stay usable, drawing the peer's
last-known monitors with `connected: false`, which is exactly what ADR 0018
retains them for — draw a visibly different arrangement, and save. Then
re-enable the adapter.

*A pass, on reconnect:* the newer revision wins on both machines, and
**the loser says so in full**. On A:

```
layout sync: adopted the peer's arrangement; the one this machine held is superseded
    adopted_revision=… adopted_origin=… superseded_revision=… superseded_origin=…
```

If instead A's arrangement is the newer one, B's save is the loser and B
logs the mirror image (and says it on the console too, in a foreground
run):

```
layout sync: the arrangement just saved to the config file is superseded by a newer one
  this run already holds; it will not be used
    adopted_revision=… adopted_origin=… superseded_revision=… superseded_origin=…
```

*A failure:* the two machines end up crossing by different arrangements
with nothing in either log saying which won — that is precisely the silent
mis-crossing the criterion forbids. Confirm convergence behaviourally too:
after the dust settles, a crossing at a seam that exists in only one of the
two arrangements must behave the same way from both ends.

#### 3.2 Restart both

Reboot A and B (or `Restart-Service Crossover` on each).

*A pass:* both come back crossing by the same arrangement, with no editor
run and no re-pairing. `crossover config` on each shows the same
`revision`/`origin`. Cross a few times at a drawn seam to confirm
behaviourally, not just on disk.

*A failure:* either machine coming back on the side model (the `[layout]`
section was lost, or `schema_version` regressed), the two disagreeing on
`revision` with neither re-syncing, or an arrangement that is right on disk
but not in force — crossing still happening at the old outer edge, which
means the startup path read the file and did not publish it.

#### 3.3 Revision numbering advances past what was adopted

Open the editor on the machine that **lost** 3.1, drag something, and save.

*A pass:* the new save's revision is one past the highest revision either
file has seen — so it beats the adopted one and propagates, rather than
tying with it. Watch the far machine adopt it.

*A failure:* a save that numbers *into* a revision already adopted. Two
different arrangements at one revision is the anomaly the hash tiebreak
exists to survive, not a state to reach on purpose; if it happens, the log
says so and the sighting is worth a defect:

```
layout sync: two different arrangements claim the same revision and origin; resolving
  by content hash (ADR 0018)
```

---

### Pass 4 — adversarial-adjacent

Not a security test — a hostile *trusted* peer stays out of scope
(SECURITY.md §6) — but the failure modes a real desk actually produces:
half-written files, a stopped worker, an editor left open across a crash.
The property under test throughout is **degrade, don't die**.

#### 4.1 Hand-corrupt A's config mid-run

With everything running, open `~/.crossover/config.toml` on A and break
it in three escalating ways, restoring between each:

1. **A stray `[`** — the file is not TOML at all.
2. **A structurally invalid `[layout]`** — two monitors given the same `id`
   within one device, or a `width = 0`.
3. **A `[layout]` naming a device this machine is not paired with** — the
   residue of a re-pair.

*A pass, in order:*

```
topology: config re-read failed; keeping the last good configuration
topology: config re-read has an invalid [layout]; keeping the last good one
layout sync: the config file names an arrangement of machines this run is not connected
  to; it is not used (redraw it with `crossover layout` after pairing)
```

Each warns **once per failure streak**, not once per 2 s tick, and the run
keeps crossing by the last good arrangement throughout. Restore the file
and confirm the warning state clears and the arrangement is re-read.

*A failure:* the worker exits, stops crossing, or repeats a warning every
tick. A worker that dies on a bad config is a worker the service will
relaunch into the same bad config — an ADR 0011 relaunch loop, which is why
this degradation exists.

#### 4.2 Delete the state file under a running editor

With `crossover layout` open on A, delete
`~/.crossover/state/topology.json`.

*A pass:* the editor keeps the drawn arrangement on screen for a few
consecutive bad reads (the grace period), then demotes honestly to the
empty state — `Worker: not running`, or `Worker: state file unreadable —
<reason>` when the file is present but unusable. The editor log records the
transition exactly once:

```
the worker's state file could not be used            reason=…
no drawn arrangement survived the read-failure grace period; showing the empty state
```

Then let the worker's next write recreate it: the canvas fills back in on
its own, with `the worker's state file is readable again` logged once.
Repeat with the file *present but corrupt* (truncate it, or bump its
version field by hand) — the reason must **name why**, not just report
absence.

*A failure:* a blank canvas with no reason given, a demotion that flashes
on a single transient read, or a log line once per second.

#### 4.3 Kill the worker under an unsaved editor edit

With an unsaved drag on screen in the editor, `Stop-Process` A's
`crossover.exe`.

*A pass:* the state file stays where it is, so its heartbeat simply goes
quiet — the editor keeps drawing the last report and says
`not responding — showing its last report`, **with the unsaved edit
intact**. The service relaunches the worker within ~1 s; the editor's
status returns to running and the edit is *still* there. Save it then, and
confirm the freshly relaunched worker picks it up (1.2's line).

*A failure:* the edit is discarded, or the editor blanks. A demotion is
allowed to lose facts about the worker; it must never lose the user's
drawing. (An edit **is** correctly discarded by one event, and only one: a
*different* peer appearing, which is a re-pair.)

---

### Known residuals to watch

Named here rather than discovered mid-session. None of these is a defect to
be surprised by; each is a decision with a cost, and the soak's job is to
find out whether the cost is real on this hardware.

- **The inert-while-`Returning` window — the strong fix is deliberately not
  implemented.** A machine that is *being controlled* reclaims by crossing
  a span; a crossing map with no spans therefore removes that particular
  reclaim path. `crossover-core`'s `CrossingMap::inert` names three ways a
  caller may honour the contract, and the explicit-layout source takes
  route 3 — reclaim paths that never ran through spans — rather than route
  1, retaining the previous map's spans for the `Returning` direction. The
  surviving exits are: **genuine local input** on the controlled machine
  (`ControlEvent::LocalInputReclaim`), **`r` at the controller's console**,
  **both Control keys at the controller**, and **disconnect**. Note that
  under the background service the second of those is **not available** —
  the worker has no console — so on a service-launched pair the real exits
  are local input, the escape chord, and disconnect. The honest
  caveat is on the first of them: the local-input detection re-baselines the
  system input tick after every injection the peer makes, so *while the
  controller is actively driving*, a local event can be re-baselined past
  before a poll observes it — it resolves the moment the controller pauses.

  **Provoke this once, deliberately.** On an explicit drawn arrangement,
  with A driving B, unplug (or power off) the monitor B's crossing span
  depends on **mid-grant**, while A keeps the mouse moving continuously.
  Then try to get B back with local input alone, and time it. Record: how
  long it took, whether pausing A's mouse for a second resolved it
  immediately, and whether the escape chord still worked (and `r`, if this
  provocation is run with A in the foreground rather than under the
  service). **If a
  user is genuinely unable to reclaim in that window, route 1 is the change
  to make**, and it belongs in `CrossingMap::inert` where the span ids live
  — the NOTE in that function says so, and this soak is the evidence it
  asks for.

- **One restart before a first-ever adopted arrangement drives the
  cursor.** A run holding no drawn arrangement — an implicit
  `--left`/`--right` run, or seamless off — adopts and persists a layout the
  peer sends, but has no live crossing source for the publication to
  replace, so it begins crossing by it at the *next* start (ADR 0018's
  2026-08-21 amendment). It applies **once per machine, not per edit**, and
  it is logged at the moment it applies (`…takes effect at the next start
  (it is saved to the config now)`). Check 1.4 restarts B for exactly this;
  do not read the restart as a workaround for a bug.

- **Degraded placement during an edit's propagation window is expected and
  diagnosed.** For the few seconds between one machine adopting a revision
  and the other doing so, a crossing carries an `EntryPoint` the receiver
  cannot honour and the cursor lands on the desktop-bounds edge instead of
  the drawn one, with `control: the entry point was derived from a layout
  revision this machine does not hold…`. Placement degrades; the grant does
  not. Count them if they are frequent — a *steady* stream outside an edit
  window means the two machines are not converging, which is Pass 3's
  business.

- **A saved arrangement whose screens are all absent is inert, not
  rejected.** The rule "a monitor neither peer has reported" was removed
  from the rejection list (ADR 0018 / PROTOCOL.md §6.2, amended
  2026-08-21) because it would make a drawing forget the desk on every
  undock. What is owed instead is observability, and check 2.5 is where it
  is verified.

- **Carried from earlier phases, unchanged:** injection into an elevated
  window may be swallowed (UIPI, R-1); an application with its own Home/End
  handling interprets forwarded shifted navigation its own way (Phase 4);
  and there is still no inbound preemption of a genuinely saturated
  same-driver queue (Phase 7, PR #46).

---

### Exit criteria → checks

Every Phase 8 exit criterion in docs/ROADMAP.md, and the checks that sign
it off. A criterion with no passing check is a criterion not met,
whatever else the session showed.

| docs/ROADMAP.md Phase 8 exit criterion | Checks |
|---|---|
| A layout drawn in the editor produces crossings that match it, **including a seam between two monitors of the same machine** and **a corner where three monitors meet** | 1.1–1.4 (the loop closes), **1.6** (the same-machine seam), **1.7** (the three-way corner), 1.9 (span boundaries), 1.10 (an inert edge portion). Baseline contrast: 0.3 |
| Mixed DPI and mixed resolution behave: a pointer leaving a 4K monitor at 40 % of its edge arrives at 40 % of the adjacent edge, whatever the scaling | **1.8** (measured with a ruler, both directions). Single-machine companion: docs/TESTING.md §3.2 E-2 |
| A display added, removed, or rearranged at runtime updates the layout **without a restart and without a stuck cursor** | **2.1** (cursor local), **2.2** (mid-control — the stuck-cursor half), 2.3 (power-off vs. unplug), 2.4 (editor live), 2.5 (partially unmatched). Baseline: 0.6 |
| The arrangement **survives restart**, and two machines that disagree **resolve observably** rather than silently mis-crossing | **3.1** (disagreement, both diagnostics), **3.2** (restart), 3.3 (revision numbering), 1.5 (persisted and upgraded), 4.1 (a config that cannot be trusted degrades rather than mis-crossing) |
| No regression in seamless transfer's existing guarantees: control returns at the reverse edge, no stuck keys, no cursor left hidden | **All of Pass 0** (0.1–0.6), plus 2.2 (mid-control change), 1.9 (no bounce), 4.2 (the editor stays diagnosable when the worker's report goes away), 4.3 (worker death mid-edit). Clipboard sync must also keep working throughout — copy across at points in every pass |

### The standing rule

Anything this session finds becomes **fix commits on this branch, or a
recorded follow-up with its evidence** — and only then does the
current-phase marker in docs/ROADMAP.md move. That is the order every
previous phase closed in, and it is the reason the marker is worth
believing.

Record the outcome in the Phase 8 exit-criteria notes (docs/ROADMAP.md):
which arrangements were drawn (including the same-machine seam and the
three-way corner), the measured 40 % arrival error, how many crossings ran
at a span boundary and whether any bounced, what happened on each runtime
display change, how the disagreement resolved and how long it took, and —
specifically — what the inert-while-`Returning` provocation showed.
