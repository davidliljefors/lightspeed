//! XXH3-64 helpers. Non-cryptographic: used for cache keys and change
//! detection, never for security.

use xxhash_rust::xxh3::{Xxh3, xxh3_64};

#[inline]
pub fn hash_bytes(bytes: &[u8]) -> u64 {
    xxh3_64(bytes)
}

/// Streaming hasher that length-prefixes each field, so `["ab","c"]` and
/// `["a","bc"]` hash differently.
#[derive(Default)]
pub struct FieldHasher(Xxh3);

impl FieldHasher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn field(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.update(&(bytes.len() as u64).to_le_bytes());
        self.0.update(bytes);
        self
    }

    pub fn str(&mut self, s: &str) -> &mut Self {
        self.field(s.as_bytes())
    }

    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.update(&v.to_le_bytes());
        self
    }

    pub fn finish(&self) -> u64 {
        self.0.digest()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_sensitive() {
        assert_eq!(hash_bytes(b"abc"), hash_bytes(b"abc"));
        assert_ne!(hash_bytes(b"abc"), hash_bytes(b"abd"));
        assert_ne!(hash_bytes(b""), hash_bytes(b"\0"));
    }

    #[test]
    fn fields_are_delimited() {
        let h = |a: &[&str]| {
            let mut h = FieldHasher::new();
            a.iter().for_each(|s| {
                h.str(s);
            });
            h.finish()
        };
        assert_ne!(h(&["ab", "c"]), h(&["a", "bc"]));
        assert_ne!(h(&["a"]), h(&["a", ""]));
        assert_eq!(h(&["x", "y"]), h(&["x", "y"]));
    }
}
