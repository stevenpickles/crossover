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

Record the outcome in the Phase 4 exit-criteria notes
(docs/ROADMAP.md): how many control cycles, whether any key or modifier
ever stuck, whether the escape ever leaked, and the subjective feel of
typing on the far machine.
