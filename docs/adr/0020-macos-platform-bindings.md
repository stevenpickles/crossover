# 0020. macOS platform bindings: one `objc2` family, in its own crate

Status: Accepted
Date: 2026-09-28

## Context

Phase 9.1 ports Crossover to macOS ([ROADMAP.md](../ROADMAP.md)), validated
on Apple Silicon under the current macOS. Every platform trait in
`crossover-platform` — `SecureStorage`, `ClipboardProvider`, `InputCapture`,
`InputInjector`, `DisplayInfo`, `CursorMask`, `ServiceManager`, and the
file-paste traits — needs an implementation written against Apple's
frameworks: Security (Keychain), AppKit (`NSPasteboard`), CoreGraphics
(event taps, event posting, displays, the cursor), and Foundation beneath
all of them.

Rust reaches those frameworks through binding crates, and the choice is a
core library decision ([adr/README.md](README.md)) for three reasons:

- **It is the whole `unsafe` surface of the port.** The Windows crate is the
  workspace's designated home for FFI, and every block in it carries a
  SAFETY comment and runs on Windows CI. The macOS crate inherits that
  discipline, and the binding library decides how much `unsafe` there is to
  discipline.
- **It lands in a tree that already has one.** The layout editor's toolkit
  ([ADR 0019](0019-layout-editor-toolkit.md)) links winit, and on macOS
  winit links the `objc2` family: `Cargo.lock` already holds `objc2` 0.6
  with `objc2-foundation`, `objc2-app-kit`, `objc2-core-foundation` and
  `objc2-core-graphics` at 0.3.2 — alongside an older 0.5 / 0.2 generation
  and `core-foundation` 0.9 that other editor dependencies still pull. A
  choice that ignores what is already there adds a *third* binding stack to
  audit.
- **The two binary-identity risks start here.** Accessibility trust (M-1)
  and Keychain access control (M-8) are both recorded against the signed
  identity of the binary, so how the port is built and signed decides
  whether a rebuild or an upgrade silently loses input or the device
  identity ([platform-risks-macos.md](../platform-risks-macos.md)).

## Decision

**The macOS port is written against the `objc2` family — `objc2` 0.6 and the
0.3 generation of its per-framework crates — in a new crate,
`crossover-platform-macos`, which is the port's only home for `unsafe`.**

1. **One family, the generation already in the tree.** `objc2` for the
   runtime, and `objc2-foundation`, `objc2-app-kit`,
   `objc2-core-foundation`, `objc2-core-graphics` and **`objc2-security`**
   for the frameworks, each with only the features a slice uses. Matching
   the generation winit already resolves means the port adds framework
   crates to a family that is already audited and built, not a second
   family beside it.
2. **The Keychain through `objc2-security` too.** `SecItemAdd` /
   `SecItemCopyMatching` / `SecItemUpdate` / `SecItemDelete` on a
   generic-password item, called directly. It is lower level than the
   alternative below and costs a small, contained amount of `unsafe`; it
   keeps the whole port on one CoreFoundation binding.
3. **A crate of its own, shaped like the Windows one.** Everything behind
   `#[cfg(target_os = "macos")]`, so on Windows and Linux the crate
   compiles as an empty shell and tri-OS CI keeps building the whole
   workspace. Dependencies are target-gated in the manifest, so nothing
   Apple-specific enters another OS's dependency graph. The applications
   depend on it for every target, as they already do on
   `crossover-platform-windows`.
4. **The same FFI discipline.** Every `unsafe` block carries a SAFETY
   comment; the crate's tests run on the `macos-latest` CI runners, which
   are Apple Silicon; and anything CI cannot reach — a permission prompt,
   the real pasteboard under a logged-in user, a moved cursor — gets a
   manual hardware check, as the Windows port's probes do.
5. **Platform APIs before new crates.** A slice reaches for the framework
   through `objc2` before it reaches for a convenience crate, for the same
   audit-surface reason [ADR 0016](0016-image-interchange-format.md)'s
   amendment prefers ImageIO to an image crate on macOS.

The crate is created now with nothing in it; each slice adds the framework
crates it uses when it uses them, so the manifest never names a dependency
no code exercises.

## Alternatives Considered

**`security-framework` for the Keychain.** The most widely used safe
wrapper for Security.framework, with a high-level generic-password API
that would make the Keychain slice mostly safe code. Rejected on balance:
its 3.x line brings `core-foundation` 0.10, a second CoreFoundation binding
next to `objc2-core-foundation`, for the benefit of hiding four FFI calls
that are simple to wrap and test once. It stays the fallback if the direct
calls prove harder to get right than expected — a swap that would need only
an addendum here, not a new decision.

**The older `cocoa` / `core-foundation` / `core-graphics` crates.**
Long-established, and `core-foundation` 0.9 is in the tree already. Rejected
because the `cocoa` crate is deprecated in favour of `objc2`, its API is
largely `unsafe` with untyped `id` receivers, and building a new port on a
line that is being retired means porting it again.

**Hand-written `extern` declarations with no binding crate.** The smallest
dependency graph, and plausible for CoreGraphics' C functions. Rejected
because AppKit's pasteboard is Objective-C: calling it without a runtime
binding means hand-rolling message sends and reference counting, which is
exactly the class of memory-safety bug `objc2`'s typed messages and
retained pointers exist to prevent.

**Putting the port inside `crossover-platform`.** Rejected for the same
reason the Windows code has a crate of its own: the platform boundary is a
compile-time firewall ([ARCHITECTURE.md](../ARCHITECTURE.md) §3), and a
trait crate that pulled in AppKit on macOS would no longer be the neutral
contract every backend implements.

## Consequences

**The port starts with an empty crate and a known-good dependency family.**
Slice 1 (Keychain identity) is the first to add framework crates, and each
later slice adds only its own.

**Code signing becomes a requirement, not a packaging nicety.** M-1 and M-8
both bind a grant to the binary's signed identity, and an unsigned build
changes identity with every rebuild. The Keychain slice verifies on the
lab Mac what an ad-hoc signature with a stable identifier preserves across
a rebuild, and the packaging slice (unattended operation) decides how
release builds are signed. Until then, a developer re-granting
Accessibility after a rebuild is expected, not a defect.

**A permission the user has not granted must be visible.** Input without
Accessibility trust fails silently on macOS (M-1). The input slice owes an
explicit "input unavailable: Accessibility permission missing" state that
reaches the user and the log, in the same spirit as NFR-3.

**Static analysis has to follow the FFI to macOS.** CodeQL's Rust analysis
(`.github/workflows/codeql.yml`) builds on a Windows runner, because the
Windows crate's FFI is why that job exists; off macOS this crate compiles
empty, so its FFI would never be analysed. The first slice to add `unsafe`
here also adds a `macos-latest` Rust entry to that workflow.

**Two `objc2` generations stay in the tree** until the editor's dependencies
move past 0.5 / 0.2 themselves. The port adds nothing to the older one.

**Everything behind the traits stays OS-neutral.** Core, protocol and
security crates gain no macOS code; the engine sees a macOS clipboard
exactly as it sees a Windows one, which is what makes macOS ↔ Linux the
proof the protocol is the contract (Phase 9 exit criteria).
