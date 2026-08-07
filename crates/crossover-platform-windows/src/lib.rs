//! Windows implementations of the `crossover-platform` traits.
//!
//! Currently an empty shell: Win32 implementations land behind
//! `#[cfg(windows)]` in later phases. The crate itself must compile on all
//! platforms so tri-OS CI can build the whole workspace
//! (docs/ARCHITECTURE.md §2, §4; platform risks in docs/SPECIFICATION.md §6).

/// One-line statement of this crate's responsibility.
pub const CRATE_PURPOSE: &str =
    "Win32 implementations of the crossover-platform traits (docs/ARCHITECTURE.md §4)";

#[cfg(test)]
mod tests {
    use super::CRATE_PURPOSE;

    #[test]
    fn crate_purpose_is_stated() {
        assert!(!CRATE_PURPOSE.is_empty());
    }
}
