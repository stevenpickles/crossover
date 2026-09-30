# 0023. macOS file paste: a spooled file offered by URL, the same guardrails

Status: Proposed
Date: 2026-09-29

## Context

Files and folders travel between two Windows machines through
[ADR 0015](0015-spooled-virtual-file-paste.md): a peer's file is received
into a private spool, verified, and offered to Explorer as a **virtual file
list** — `CFSTR_FILEDESCRIPTORW` plus `CFSTR_FILECONTENTS` behind an
`IDataObject` Crossover owns — so the bytes reach a user-visible folder
only when the user pastes, and the shell performs that write.
[SECURITY.md](../SECURITY.md) §7 holds the invariants (F1–F16), and every
one of them is about the *receiving* side's decisions, not the mechanism.

Phase 9 requires full parity, so Phase 9.1's sixth slice brings files to
the Mac, and the mechanism does not port: macOS has no `IDataObject`. It
has two candidates:

- **A file URL.** The pasteboard carries `public.file-url` items, and
  Finder's Paste copies the files they name into the folder the user is
  in. That is exactly how a Finder Copy followed by Paste works, so any
  destination that takes a Finder copy takes this.
- **A file promise.** `NSFilePromiseProvider` defers writing a file until a
  destination asks for it, naming the folder — the closest thing to a
  virtual file. It is built for drag and drop; whether Finder honours a
  promise *pasted* rather than dropped is not documented well enough to
  design on.

The sending half is easier: a Finder copy puts `public.file-url` items on
the pasteboard, which read as the selection's paths, exactly the
observation `CF_HDROP` gives on Windows. Packing a selection into one blob
(ADR 0015's walk, caps, refusals and write-only archive) is plain
filesystem and zip work, and is platform code today only by where it
lives.

## Decision

**A received file is offered on the macOS pasteboard as a `public.file-url`
naming a read-only file inside the spool, whose name is the validated name.
Every ADR 0015 guardrail carries over unchanged; only the last step — how a
verified entry is offered for paste — differs.**

1. **Spool layout.** A verified entry is placed at
   `<spool>/<entry-id>/<validated-name>`, read-only, where `<spool>` is a
   private directory under the user's Application Support folder. The
   directory per entry exists so the leaf can carry the name Finder will
   give the pasted copy without ever colliding (F5). The name is F4's
   validated name, never a peer string that has not passed it.
2. **Offer = a URL on the pasteboard.** One `public.file-url` item for the
   entry. Finder's Paste copies it out of the spool into the user's
   folder; the spool entry is never moved, so a paste can happen twice
   and a second paste elsewhere still works, as on Windows.
3. **Lifetime is ADR 0015's rule.** The entry lives while the pasteboard
   still holds our item — judged by `changeCount` against the value our
   own write produced, the macOS form of F13's ownership check — and is
   collected when the pasteboard moves on, with the 24-hour age backstop
   (F12).
4. **Loop prevention is F13's third layer, unchanged.** A local copy whose
   URLs resolve inside the spool root is never staged for sending, so our
   own offer — a file URL, which the sender side *would* otherwise read —
   can never travel back.
5. **Origin marking: quarantine.** The spooled file carries a
   `com.apple.quarantine` attribute naming Crossover, which Finder's copy
   preserves. It is the macOS counterpart of ADR 0015's zone marking —
   stating where the bytes came from — and it asks Gatekeeper to check an
   application or script before first launch, while an ordinary document
   opens as usual. Offered for the maintainer's decision, because ADR
   0015's own zone choice was changed on exactly this question (it chose
   "Local intranet" to avoid a prompt on every document); the macOS
   attribute prompts only for executables, which is why quarantine rather
   than nothing is proposed.
6. **The sender reuses ADR 0015's builder.** The selection walk, the caps
   judged during it, the reparse-point / symlink refusal of the whole item,
   and the write-only archive move out of the Windows crate into
   platform-neutral code both ports call, rather than being written twice.
7. **Clipboard history**: macOS has no system clipboard history or cloud
   clipboard equivalent to F16's concern at the time of writing, so there
   is nothing to exclude; if one is found on the lab Mac, F16's rule
   applies.

## Alternatives Considered

**File promises.** The spool path would never appear on the pasteboard,
and the destination would get a file written on demand — the closest match
to Windows. Not chosen *yet*: its behaviour on paste (as opposed to drop)
is the one thing here that must be seen before it is designed on. If the
lab Mac shows Finder honours a pasted promise, this ADR should be amended
to prefer it; nothing in 1–6 besides point 2 changes.

**Materialize into a user-visible folder on receipt** (a Downloads-style
drop folder). ADR 0015 rejected this on user experience, and F3/F9's
"nothing reaches a user-visible location without the user's gesture"
rejects it on security grounds. Unchanged here.

**Expose the spool path as-is**, without the per-entry directory. The leaf
would have to be the entry id, and Finder would paste a file named by a
UUID. Rejected: the validated name is the whole point of F4's care.

## Consequences

**The spool path becomes visible** in the URL a paste target sees, which it
never is on Windows. It names a private, Crossover-owned directory (F15)
and discloses nothing a local process running as the user could not
already list, so it is recorded, not defended.

**F14's "rendering serves registered spool content and nothing else"
becomes a filesystem property** rather than a data-object one: the URL
names exactly one registered entry's file, read-only, and nothing else in
the spool is referenced.

**The file builder moves crates**, which is an internal refactor of code
that is already tested; its tests move with it and run on every OS.

**To verify on the lab Mac before this is accepted:** that Finder's Paste
copies a `public.file-url` from a private directory with its name, and
preserves the quarantine attribute; what a pasted quarantined document and
application each do on first open; whether Finder honours a pasted file
promise (which would change point 2); and that Mail, Messages and a Save
dialog accept the pasted item as they accept a Finder copy.
