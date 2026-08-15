//! Give `crossover-svc.exe` the same identity as `crossover.exe`.
//!
//! The service binary is the one most likely to be inspected without being
//! run — it sits in `%ProgramFiles%\Crossover` and is started by the SCM — so
//! carrying a version resource matters more here than anywhere else. It shares
//! `apps/build_identity.rs` with the application, which is a source include
//! rather than a crate precisely so that the `LocalSystem` binary's dependency
//! graph stays as narrow as ADR 0011 requires.

include!("../build_identity.rs");

fn main() -> io::Result<()> {
    let identity = emit_build_identity()?;

    #[cfg(windows)]
    stamp_windows_resource(&identity, "Crossover background service")?;
    #[cfg(not(windows))]
    let _ = &identity;

    Ok(())
}
