//! Give `crossover.exe` its identity: the generated `BUILD_INFO` constant
//! that `crossover version` reports, plus — on Windows — the application icon
//! and a version resource carrying the same values.
//!
//! The identity resolution is shared with `crossover-svc` so the two binaries
//! of one install can never disagree about what they are; see
//! `apps/build_identity.rs` for the scheme and for why it is an included
//! source file rather than a crate.

include!("../build_identity.rs");

fn main() -> io::Result<()> {
    let identity = emit_build_identity()?;

    #[cfg(windows)]
    stamp_windows_resource(
        &identity,
        "Crossover keyboard, mouse, and clipboard sharing",
    )?;
    #[cfg(not(windows))]
    let _ = &identity;

    Ok(())
}
