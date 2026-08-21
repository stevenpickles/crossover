//! Give `crossover-layout.exe` the same identity as the other two binaries.
//!
//! The editor ships beside `crossover.exe` and `crossover-svc.exe` and is
//! upgraded with them, so it carries the same stamped version resource — the
//! packaging script compares all three and rejects a build where one is stale
//! (`scripts/build.ps1`). It shares `apps/build_identity.rs` as an included
//! source file rather than a crate, which is what lets the editor report a
//! full build identity without adding an edge to the narrow dependency graph
//! ADR 0019 requires of it.

include!("../build_identity.rs");

fn main() -> io::Result<()> {
    let identity = emit_build_identity()?;

    #[cfg(windows)]
    stamp_windows_resource(&identity, "Crossover display layout editor")?;
    #[cfg(not(windows))]
    let _ = &identity;

    Ok(())
}
