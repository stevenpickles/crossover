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
  `superseded` is normal if both machines copied simultaneously.
- **Zero** `clipboard_unavailable` in the absence of deliberate
  contention; a few during it is the bounded-retry design working, and
  each one is visible rather than silent.
- p50 latency in the low tens of milliseconds on a LAN; the max may
  spike when another application holds a clipboard.
- Every disconnect is followed by a reconnect with no pairing.

What warrants investigation:

- **Any** content mismatch — copy something on A, confirm the exact
  bytes on B (this is release-blocking).
- Sync traffic that continues when nobody is copying — the signature of
  a loop.
- `clipboard_unavailable` at rest.
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
  cursor, and clipboard sync kept working across the whole window — over a
  hundred clipboard transactions applied across the two sides, and repeated
  control transfers every day of the soak.
- **Peer-outage recovery, at full scale.** Machine A was off from midday
  08-12 (UTC) to early 08-14. Machine B's reconnect supervisor retried the
  entire outage at the capped 30 s backoff (attempt counts into the
  seventies) and re-established on its own the moment A returned.
- **Worker relaunch, for real.** Early on 08-11 (02:21–02:40 UTC) machine
  B's worker exited immediately after startup, repeatedly; the service
  relaunched it on the ADR 0011 backoff (1 s doubling to the 30 s cap) until
  the 02:40 launch came up cleanly, computed its topology, connected, and
  applied a waiting clipboard item. Recovery was fully automatic.
- **Failures stayed bounded and observable.** One inbound clipboard item
  could not be installed (five attempts, then a logged
  `clipboard_unavailable` — no hang, no loop). Transient injection warnings
  (SendInput rejected during a blocked-input window, SetCursorPos timeouts)
  were logged and self-cleared. No stuck key or button after any transfer,
  disconnect, or relaunch.

Findings feeding follow-up work:

- **Static display topology (scheduled as feature/107).** The virtual
  desktop and seamless edge are computed once at worker startup. Unplugging
  or powering off the external monitor mid-run leaves the stale edge in
  place — the worker keeps believing the monitor is there, so the edge can
  sit at still-reachable coordinates and hand control across when the user
  does not expect it. Under a boot-started background service this is an
  everyday event (docking, monitor power-off), not a corner case.
- **Silent worker exit (diagnosability).** During the 08-11 relaunch loop
  the worker logged `starting` and nothing else — it exited before reaching
  its run loop without recording why. The window is consistent with a
  locked session or displays not yet available, but the log cannot say; a
  worker that exits before serving should log the reason at WARN.
- **Mislabeled Win32 error text (cosmetic).** Some `SetCursorPos` failures
  carry unrelated `GetLastError` text (e.g. a WSAEWOULDBLOCK socket
  string): the failure is real and transient, but the label is stale errno
  noise and worth cleaning up when touching that path.
