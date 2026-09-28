//! macOS implementations of the `crossover-platform` traits (Phase 9.1,
//! [ADR 0020](../../../docs/adr/0020-macos-platform-bindings.md)).
//!
//! Written against the `objc2` family. Every implementation lives behind
//! `#[cfg(target_os = "macos")]`; on other targets the crate compiles as an
//! empty shell so tri-OS CI can build the whole workspace, exactly as
//! `crossover-platform-windows` does off Windows (docs/ARCHITECTURE.md §3,
//! §4).
//!
//! This crate is the port's designated home for `unsafe` (framework FFI).
//! It holds none yet: the first slice to call a framework relaxes the
//! workspace's `unsafe_code = "forbid"` here, and from then every unsafe
//! block carries a SAFETY comment and is exercised by tests on the
//! `macos-latest` CI runners (NFR-6, docs/TESTING.md §1.6). What CI cannot
//! reach — a permission prompt, the real pasteboard under a logged-in user,
//! a moved cursor — is covered by manual checks on the lab Mac.
//!
//! The risks each implementation must answer are catalogued, before any of
//! it exists, in docs/platform-risks-macos.md (M-1..M-10).
