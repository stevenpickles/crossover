# 0008. Keyboard key representation: physical key by USB HID usage, produced text carried alongside, inject by scan code

Status: Proposed
Date: 2026-08-08

## Context

Phase 4 forwards the keyboard. The capture and injection *mechanisms*
are already decided — [ADR 0007](0007-windows-input-capture.md) chose
low-level hooks (`WH_KEYBOARD_LL` for the keyboard) to suppress, and
`SendInput` to inject, and anticipated this extension. What ADR 0007 did
*not* decide is the question that actually shapes the wire and cannot be
walked back later: **how a key is represented** as it crosses between
machines.

[FR-4.1](../SPECIFICATION.md) is explicit and constraining:

> Input events use platform-neutral representations distinguishing
> physical key identity from OS key representation from produced text.
> The protocol must not permanently require identical keyboard layouts or
> Windows keycodes.

Three forces pull in different directions, and a naive model satisfies at
most two:

1. **Shortcuts are positional.** Ctrl+C is "the key where C sits," not
   the character *c*. A user pressing Ctrl+C on a Dvorak keyboard expects
   Copy, which lives at the physical QWERTY-C position. Shortcuts must be
   forwarded by *physical key*, or every remapped layout breaks them.
2. **Typing is textual.** When the two machines run *different* layouts,
   the character the source user produced (é, ñ, @) is what they meant —
   not whatever the destination's layout yields from the same physical
   key. Text fidelity across mismatched layouts needs the *produced
   character*, not the physical key.
3. **Non-text keys have neither character nor stable OS code.** Arrows,
   F-keys, Esc, Tab, Enter, and the modifiers themselves produce no text,
   and their OS virtual-key codes are platform-specific — the exact thing
   FR-4.1 forbids requiring.

The representation must therefore keep *physical identity* and *produced
text* as distinct, non-conflated slots, and it must be expressible in a
namespace that is neither Windows-specific nor layout-specific.

## Decision

### Physical key identity: USB HID Usage IDs (Usage Page 0x07)

Every key's physical identity travels as its **USB HID keyboard/keypad
usage ID** (Usage Page `0x07`), a `u16`. This is the hardware-level
standard the operating systems already speak underneath their own
abstractions: Windows scan codes, macOS `kHIDUsage_Keyboard*`, and Linux
evdev `KEY_*` all map to and from it through published, bounded tables.

It satisfies FR-4.1 by construction — it is not a Windows keycode, and it
is layout-independent (usage `0x04` is "the physical key labelled A on a
US keyboard" regardless of what any layout maps it to). It is compact
(one `u16`), stable (a frozen published namespace), and needs no string
parsing.

### The wire key event carries physical key + optional produced text

A key transition on the wire is:

```
KeyEvent { key: u16 (HID usage), pressed: bool, repeat: bool, text: Option<String> }
```

- `key` is the HID usage — **authoritative for injection of keys and
  shortcuts**.
- `text` is the Unicode grapheme(s) the source produced, present only for
  printable keys — **authoritative for text fidelity** when layouts
  differ.
- `pressed` distinguishes press from release; `repeat` marks an
  OS-generated auto-repeat so key-state accounting is not fooled into
  double-counting a held key.

The source's **OS virtual-key code is deliberately not carried.** It is
redundant for our injection strategy and actively wrong to inject from
(it would import the source's layout), and putting a layout-specific code
on every keystroke is the coupling FR-4.1 exists to prevent. Keeping
`key` (physical) and `text` (produced) as separate slots is what "distin-
guishing physical key identity from produced text" requires; the OS
representation is the source's private business and stays there. This
refines the provisional `{ physical_key, os_key, text?, sequence }`
sketch in [PROTOCOL.md](../PROTOCOL.md) §6, which is updated to match.

### Injection: scan code primary, Unicode for layout mismatch

On Windows the destination injects via `SendInput`:

- **By scan code** (`KEYEVENTF_SCANCODE`), derived from the HID usage,
  for every key. The destination's own layout then resolves the
  character, so matching-layout typing is correct and shortcuts land on
  the right *physical* key. This is the Phase 4 path and it satisfies the
  exit criterion — normal typing and common shortcuts work.
- **By Unicode** (`KEYEVENTF_UNICODE`) from the `text` field as the
  documented path for *mismatched* layouts, where the physical key would
  produce the wrong character. Because `text` is already on the wire, this
  is a destination-side injection choice, not a protocol change — it can
  land after Phase 4 without a version bump. Phase 4 targets matching
  layouts; the protocol is ready for the rest, which is exactly what
  FR-4.1 means by "must not *permanently* require identical layouts."

### Modifiers are ordinary physical keys

Left/right Control, Shift, Alt, and GUI/Meta have distinct HID usages and
are represented as plain `KeyEvent`s. Modifier state is reconstructable
from the ordered stream of transitions, so **no separate modifier
bitmask travels** — a second source of truth that could disagree with the
transitions is worse than none. Held modifiers are tracked in key-state
exactly as pointer buttons are, and `ReleaseAllInput` (FR-4.4) synthesizes
their releases on every disconnect path — a stuck Ctrl after a drop is the
same release-blocking defect class the pointer work already guards.

### Ordering, not coalescing

Key transitions are ordered and lossless (FR-4.2): unlike pointer motion,
they are **never coalesced** — dropping or reordering a press/release
creates the stuck-key defect. They ride the existing `InputBatch` /
control-grant path, so keyboard adds **no new authorization surface**: a
peer's keystrokes are injected only while it holds the control grant (the
per-session complete mediation from Phase 3) and its per-peer `keyboard`
permission is set ([SECURITY.md](../SECURITY.md) §permissions, T8).

## Alternatives Considered

- **W3C UI Events `code` strings** ("KeyA", "ShiftLeft"). The same
  physical-key information in a bulkier, string-typed form that still
  needs a mapping table to inject. HID usage is the numeric canonical
  source those strings derive from — chosen for compactness and to avoid
  string handling on a hot path.
- **Windows scan codes as the wire identity.** Platform-specific, and
  the scan-code sets (Set 1 vs. the extended-key `E0` prefixes) are
  fiddly and Windows-flavoured. HID usage is the OS-neutral standard;
  scan codes are derived from it only at the Windows injection boundary.
- **Virtual-key (VK) codes as identity.** Layout-dependent *and*
  Windows-specific — disqualified twice over by FR-4.1.
- **Produced text only, no physical key.** Cannot express shortcuts
  (Ctrl+C needs the physical position, not the character) or non-text
  keys (arrows, F-keys, Esc produce no text). Text is necessary but not
  sufficient.
- **A separate modifier bitmask on each event.** Redundant with the
  ordered transitions and a divergence risk; rejected in favour of a
  single source of truth.

## Consequences

- Easier: layout independence becomes a *protocol* property from day one
  (the `text` field is carried) even while Phase 4 injects by scan code;
  shortcuts land by physical position; and key-state plus `ReleaseAllInput`
  are the same shape as the pointer-button machinery, so the core control
  engine barely grows.
- Harder: the Windows layer needs HID-usage ↔ scan-code mapping tables.
  They are well-known and bounded, but tedious and must be tested against
  real hardware for the extended keys (arrows, numpad, right-hand
  modifiers) where the `E0` prefix bites.
- Deferred, and documented as out of Phase 4 scope: Unicode injection for
  mismatched layouts (the `text` path), dead-key composition, and IME —
  each has real subtleties (dead keys and IME are stateful in ways a
  single keystroke does not capture) and none block matching-layout
  typing and shortcuts.
- Risk accepted: until the `text` injection path lands, two machines on
  *different* layouts will type the wrong characters (though shortcuts,
  arrows, and modifiers remain correct). This is a fidelity gap, not a
  stuck-key or security defect, and it closes without a wire change.
