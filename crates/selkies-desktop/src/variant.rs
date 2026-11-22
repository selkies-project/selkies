//! D-Bus variants read leniently: a value a peer sends inside a variant is
//! taken at the type the shell expects when the variant's signature is that
//! type's, and skipped unread otherwise.
//!
//! The specifications the shell serves fix each value's type, but senders
//! get them wrong often enough that a dictionary decoded as one typed
//! structure would fail whole over one stray entry.  Reading each entry
//! through [`Lenient`] keeps the entries that are right, and reads them
//! straight into their Rust types rather than through generic variants, so
//! an image's bytes arrive as one buffer rather than a variant per byte.

use std::marker::PhantomData;

use zbus::export::serde::de::{self, Deserialize, Deserializer, IgnoredAny, SeqAccess, Visitor};
use zbus::{Signature, Type};

/// A variant's value when it has the type `T`, else `None`.
#[derive(Debug)]
pub struct Lenient<T>(pub Option<T>);

impl<T> Default for Lenient<T> {
    fn default() -> Lenient<T> {
        Lenient(None)
    }
}

impl<T> Type for Lenient<T> {
    const SIGNATURE: &'static Signature = &Signature::Variant;
}

impl<'de, T: Deserialize<'de> + Type> Deserialize<'de> for Lenient<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Lenient<T>, D::Error> {
        deserializer.deserialize_struct("Variant", &["signature", "value"], LenientVisitor(PhantomData))
    }
}

/// Reads a variant's signature, then its value as `T` when the signature is
/// `T`'s and skips it otherwise.
struct LenientVisitor<T>(PhantomData<T>);

impl<'de, T: Deserialize<'de> + Type> Visitor<'de> for LenientVisitor<T> {
    type Value = Lenient<T>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a variant")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Lenient<T>, A::Error> {
        let signature: Signature = seq
            .next_element()?
            .ok_or_else(|| de::Error::invalid_length(0, &self))?;
        if &signature == T::SIGNATURE {
            return Ok(Lenient(seq.next_element()?));
        }
        seq.next_element::<IgnoredAny>()?;
        Ok(Lenient(None))
    }
}
