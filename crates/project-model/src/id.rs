//! Reading numeric ids that JSON may have written as text.
//!
//! JSON object keys are always strings, so a table keyed by id is written with
//! `"3"` where the id is `3`. serde_json converts back when it reads a key
//! directly, but not once the value has passed through serde's own buffer —
//! which is what happens inside an internally tagged enum like
//! [`Content`](crate::Content), or after a migration has worked on the JSON.
//! Ids read through this accept both forms, so where a table sits in the file
//! never decides whether the file can be opened.

/// Implements `Deserialize` for a `u32` newtype, from a number or from the
/// digits of one.
macro_rules! deserialize_as_key {
    ($name:ident) => {
        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct Visitor;

                impl serde::de::Visitor<'_> for Visitor {
                    type Value = u32;

                    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        f.write_str("an id: a whole number, or its digits as text")
                    }

                    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<u32, E> {
                        u32::try_from(v).map_err(|_| E::custom(format!("id {v} is too large")))
                    }

                    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<u32, E> {
                        u32::try_from(v).map_err(|_| E::custom(format!("id {v} is out of range")))
                    }

                    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<u32, E> {
                        v.parse().map_err(|_| E::custom(format!("{v:?} is not an id")))
                    }
                }

                deserializer.deserialize_any(Visitor).map($name)
            }
        }
    };
}

pub(crate) use deserialize_as_key;
