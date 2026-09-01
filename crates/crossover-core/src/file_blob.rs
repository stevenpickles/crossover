//! The name a built blob travels under (ADR 0015, "Sender side").
//!
//! The sender's builder is a platform trait
//! (`crossover_platform::FileBlobBuilder`), and a platform crate carries
//! no dependencies by design (docs/ARCHITECTURE.md §4), so it cannot ask
//! whether the name it derived from the filesystem is a *conforming wire
//! name*. That question has exactly one answer in this project —
//! `crossover_protocol::validate_file_name`, the check a peer's name is
//! held to before it may reach a shell — and duplicating it at the
//! platform boundary would mean two validators for the one string of a
//! file transfer that a shell ever sees.
//!
//! So the builder reports *where* the name came from and this module,
//! which can name both crates, applies ADR 0015's two different answers:
//!
//! - a name the user chose — a single file's own name, or a single
//!   folder's — is **refused** when it does not conform. Reject, never
//!   repair: a substituted name is a name the user did not pick, on an
//!   item the receiving side cannot inspect before it arrives.
//! - a name *derived* from a multi-entry selection's parent folder falls
//!   back to [`FALLBACK_ARCHIVE_NAME`]. There is nothing of the user's
//!   intent to preserve — they never named the parent for this purpose —
//!   so a generic name is honest rather than a guess.
//!
//! Nothing here touches a filesystem or a blob's bytes: it is a decision
//! about a string, testable without either.

use crossover_platform::BlobNaming;
use crossover_protocol::{FileNameError, validate_file_name};

/// What a packed selection is called when nothing better can be derived
/// (ADR 0015).
///
/// Deliberately a conforming name by construction — it is asserted in
/// this module's tests, because a fallback that itself failed validation
/// would turn a recoverable naming problem into a refused transfer at the
/// one point where there is nothing left to fall back to.
pub const FALLBACK_ARCHIVE_NAME: &str = "files.zip";

/// The name a blob should travel under, or why it cannot travel at all.
///
/// `proposed` is the raw name the builder derived from the local
/// filesystem and `naming` is where it came from. The returned name has
/// passed the same validation a peer's name passes at decode, so it is
/// safe to put in a `FileDescriptor` — which validates it again, as the
/// receiving side will a third time before it reaches a shell.
///
/// # Errors
///
/// [`FileNameError`] when the selection named itself and that name does
/// not conform. A derived name never fails: it falls back.
pub fn wire_file_name(proposed: &str, naming: BlobNaming) -> Result<String, FileNameError> {
    match validate_file_name(proposed) {
        Ok(()) => Ok(proposed.to_owned()),
        Err(error) => match naming {
            BlobNaming::Own => Err(error),
            BlobNaming::Derived => Ok(FALLBACK_ARCHIVE_NAME.to_owned()),
        },
    }
}

#[cfg(test)]
mod tests {
    use crossover_platform::BlobNaming;
    use crossover_protocol::{FileNameError, validate_file_name};

    use super::{FALLBACK_ARCHIVE_NAME, wire_file_name};

    #[test]
    fn a_conforming_name_travels_unchanged() {
        for (proposed, naming) in [
            ("quarterly.pdf", BlobNaming::Own),
            ("designs.zip", BlobNaming::Own),
            ("invoices.zip", BlobNaming::Derived),
        ] {
            assert_eq!(wire_file_name(proposed, naming).unwrap(), proposed);
        }
    }

    #[test]
    fn a_name_the_user_chose_is_refused_rather_than_repaired() {
        // The bidi override ADR 0015 calls out by name: a shell renders
        // `invoice\u{202E}gnp.exe` as `invoiceexe.png`. Nothing here
        // strips it — we do not guess what the user meant, and a repaired
        // name is one they never chose.
        let hostile = "invoice\u{202e}gnp.exe";
        assert!(matches!(
            wire_file_name(hostile, BlobNaming::Own),
            Err(FileNameError::FormatCharacter { .. })
        ));
        // Over-long in bytes, and a reserved device name: both refused
        // for a name the selection gave itself.
        assert!(wire_file_name(&"x".repeat(300), BlobNaming::Own).is_err());
        assert!(wire_file_name("NUL.zip", BlobNaming::Own).is_err());
    }

    #[test]
    fn a_derived_name_falls_back_instead_of_refusing() {
        // The parent folder is not something the user named for this
        // purpose, so there is no intent to preserve — and refusing the
        // whole transfer over the name of a folder nobody mentioned would
        // be a puzzling failure.
        for proposed in ["", "NUL.zip", "invoice\u{202e}gnp.zip", &"x".repeat(300)] {
            assert_eq!(
                wire_file_name(proposed, BlobNaming::Derived).unwrap(),
                FALLBACK_ARCHIVE_NAME
            );
        }
    }

    #[test]
    fn the_fallback_is_itself_a_conforming_name() {
        // The one name with nothing to fall back to.
        assert!(validate_file_name(FALLBACK_ARCHIVE_NAME).is_ok());
    }
}
