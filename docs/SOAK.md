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
