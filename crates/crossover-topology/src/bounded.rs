//! A bounded-sequence `serde` deserializer, shared by every decoder in the
//! workspace that must refuse an over-long list **during** decoding rather
//! than after materializing it.
//!
//! Not feature-gated: [`crate::state`] (behind the `config` feature) and
//! `crossover-protocol`'s wire messages (`MonitorTopology`, `LayoutSync`)
//! both need this, and neither should carry the other's dependency to get
//! it — hence its own module, in the default graph.

use serde::de::{Error as _, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

/// Deserialize a sequence, refusing the element past `N` rather than
/// building the list and measuring it afterwards.
///
/// Two properties matter and neither comes free from `Vec`'s own impl. The
/// refusal happens **during** the decode, so an over-long list is never
/// materialized; and the initial capacity comes from `min(size_hint, N)`,
/// so a format that reports a huge length cannot make us reserve it. Both
/// are the standard treatment for a count that came from outside
/// (docs/PROTOCOL.md §6.2, CLAUDE.md's "bound everything influenced by
/// network input; validate lengths before allocating").
///
/// # Errors
///
/// Whatever `D::Error` the underlying deserializer produces for a
/// malformed sequence, plus this function's own `A::Error::custom` for the
/// element past `N` — the caller (a `#[serde(deserialize_with = "...")]`
/// field) turns either into its normal decode failure.
pub fn bounded_seq<'de, D, T, const N: usize>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Bounded<T, const N: usize>(core::marker::PhantomData<T>);

    impl<'de, T: Deserialize<'de>, const N: usize> Visitor<'de> for Bounded<T, N> {
        type Value = Vec<T>;

        fn expecting(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(formatter, "at most {N} elements")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(N));
            while let Some(item) = seq.next_element()? {
                if items.len() == N {
                    return Err(A::Error::custom(format!("more than {N} elements")));
                }
                items.push(item);
            }
            Ok(items)
        }
    }

    deserializer.deserialize_seq(Bounded::<T, N>(core::marker::PhantomData))
}
