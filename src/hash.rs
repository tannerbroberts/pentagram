//! Deterministic state hashing — the instrument every other guarantee is read
//! through.
//!
//! FNV-1a 64. Chosen because it is order-sensitive (so it catches an iteration
//! order bug rather than hiding one), trivially portable, and has no
//! platform-dependent behaviour whatsoever. It is not cryptographic and does
//! not need to be — an adversary is not the threat model here, a mismatched
//! float is.

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

#[derive(Clone, Copy, Debug)]
pub struct Hasher(u64);

impl Default for Hasher {
    fn default() -> Self {
        Hasher::new()
    }
}

impl Hasher {
    #[inline]
    pub const fn new() -> Hasher {
        Hasher(FNV_OFFSET)
    }

    #[inline]
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.0 ^= v as u64;
        self.0 = self.0.wrapping_mul(FNV_PRIME);
        self
    }

    #[inline]
    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        for x in b {
            self.u8(*x);
        }
        self
    }

    #[inline]
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.bytes(&v.to_le_bytes())
    }

    #[inline]
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.bytes(&v.to_le_bytes())
    }

    #[inline]
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.bytes(&v.to_le_bytes())
    }

    #[inline]
    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.bytes(&v.to_le_bytes())
    }

    #[inline]
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(v as u8)
    }

    #[inline]
    pub fn finish(&self) -> u64 {
        self.0
    }
}

/// Anything that contributes to the canonical state hash.
pub trait Hashable {
    fn hash_into(&self, h: &mut Hasher);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_hash_is_the_offset_basis() {
        assert_eq!(Hasher::new().finish(), FNV_OFFSET);
    }

    #[test]
    fn is_order_sensitive() {
        // This is the property that makes it catch iteration-order bugs.
        let mut a = Hasher::new();
        a.u32(1).u32(2);
        let mut b = Hasher::new();
        b.u32(2).u32(1);
        assert_ne!(a.finish(), b.finish());
    }

    #[test]
    fn distinguishes_adjacent_values() {
        let mut a = Hasher::new();
        a.i32(0);
        let mut b = Hasher::new();
        b.i32(1);
        assert_ne!(a.finish(), b.finish());
    }

    #[test]
    fn matches_published_fnv1a_vectors() {
        // Pinned against the reference implementation rather than against
        // ourselves. If this ever fails, the hashing scheme changed and every
        // stored replay hash is now meaningless.
        let mut a = Hasher::new();
        a.bytes(b"a");
        assert_eq!(a.finish(), 0xaf63_dc4c_8601_ec8c);

        let mut b = Hasher::new();
        b.bytes(b"foobar");
        assert_eq!(b.finish(), 0x85944171f73967e8);
    }
}
