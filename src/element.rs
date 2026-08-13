//! The five-cycle. One relation set, read three ways (combat, ecology, terrain).
//!
//! Index order is the generating ring and must never be renumbered:
//!
//! ```text
//!   generating (ring, energy flows):  Wood → Fire → Earth → Metal → Water → Wood
//!   overcoming (star, suppression):   Wood → Earth → Water → Fire → Metal → Wood
//! ```
//!
//! With indices `0..5` in ring order, every relation is arithmetic mod 5:
//! `generates = i+1`, `eats = i-1`, `suppresses = i+2`, `suppressed_by = i-2`.

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
#[repr(u8)]
pub enum Element {
    Wood = 0,
    Fire = 1,
    Earth = 2,
    Metal = 3,
    Water = 4,
}

impl Element {
    pub const COUNT: usize = 5;

    pub const ALL: [Element; 5] = [
        Element::Wood,
        Element::Fire,
        Element::Earth,
        Element::Metal,
        Element::Water,
    ];

    #[inline]
    pub const fn index(self) -> usize {
        self as usize
    }

    #[inline]
    pub const fn from_index(i: usize) -> Element {
        Element::ALL[i % Element::COUNT]
    }

    pub const fn name(self) -> &'static str {
        match self {
            Element::Wood => "Wood",
            Element::Fire => "Fire",
            Element::Earth => "Earth",
            Element::Metal => "Metal",
            Element::Water => "Water",
        }
    }

    /// Ring, forward: what this element feeds. Wood generates Fire.
    #[inline]
    pub const fn generates(self) -> Element {
        Element::from_index(self.index() + 1)
    }

    /// Ring, backward: what this element consumes. Fire eats Wood.
    /// Energy transfers along this edge.
    #[inline]
    pub const fn eats(self) -> Element {
        Element::from_index(self.index() + 4)
    }

    /// Star, forward: what this element nullifies. Fire suppresses Metal.
    /// No energy transfers along this edge.
    #[inline]
    pub const fn suppresses(self) -> Element {
        Element::from_index(self.index() + 2)
    }

    /// Star, backward: what nullifies this element. Fire is suppressed by Water.
    #[inline]
    pub const fn suppressed_by(self) -> Element {
        Element::from_index(self.index() + 3)
    }

    /// What eats this element. Fire is eaten by Earth.
    #[inline]
    pub const fn eaten_by(self) -> Element {
        self.generates()
    }

    /// Does `self` hold an advantage over `other` — on either edge?
    #[inline]
    pub fn beats(self, other: Element) -> bool {
        self.eats() == other || self.suppresses() == other
    }
}

/// A value per element. Used everywhere a per-race quantity is needed, and
/// preferred over a map because iteration order is then structural rather
/// than incidental — Invariant IV.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PerElement<T>(pub [T; Element::COUNT]);

impl<T> PerElement<T> {
    #[inline]
    pub fn get(&self, e: Element) -> &T {
        &self.0[e.index()]
    }

    #[inline]
    pub fn get_mut(&mut self, e: Element) -> &mut T {
        &mut self.0[e.index()]
    }

    /// Always yields elements in ring order.
    pub fn iter(&self) -> impl Iterator<Item = (Element, &T)> {
        Element::ALL.into_iter().zip(self.0.iter())
    }
}

impl<T: Copy> PerElement<T> {
    pub fn filled(v: T) -> PerElement<T> {
        PerElement([v; Element::COUNT])
    }
}

impl<T> core::ops::Index<Element> for PerElement<T> {
    type Output = T;
    #[inline]
    fn index(&self, e: Element) -> &T {
        &self.0[e.index()]
    }
}

impl<T> core::ops::IndexMut<Element> for PerElement<T> {
    #[inline]
    fn index_mut(&mut self, e: Element) -> &mut T {
        &mut self.0[e.index()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_matches_wu_xing() {
        assert_eq!(Element::Wood.generates(), Element::Fire);
        assert_eq!(Element::Fire.generates(), Element::Earth);
        assert_eq!(Element::Earth.generates(), Element::Metal);
        assert_eq!(Element::Metal.generates(), Element::Water);
        assert_eq!(Element::Water.generates(), Element::Wood);
    }

    #[test]
    fn star_matches_wu_xing() {
        assert_eq!(Element::Wood.suppresses(), Element::Earth);
        assert_eq!(Element::Earth.suppresses(), Element::Water);
        assert_eq!(Element::Water.suppresses(), Element::Fire);
        assert_eq!(Element::Fire.suppresses(), Element::Metal);
        assert_eq!(Element::Metal.suppresses(), Element::Wood);
    }

    #[test]
    fn eats_is_the_inverse_of_generates() {
        for e in Element::ALL {
            assert_eq!(e.generates().eats(), e);
            assert_eq!(e.eats().generates(), e);
        }
    }

    #[test]
    fn suppression_is_the_inverse_of_suppressed_by() {
        for e in Element::ALL {
            assert_eq!(e.suppresses().suppressed_by(), e);
            assert_eq!(e.suppressed_by().suppresses(), e);
        }
    }

    #[test]
    fn every_element_beats_exactly_two_and_loses_to_exactly_two() {
        for e in Element::ALL {
            let beats = Element::ALL.iter().filter(|o| e.beats(**o)).count();
            let lost = Element::ALL.iter().filter(|o| o.beats(e)).count();
            assert_eq!(beats, 2, "{} beats {} elements", e.name(), beats);
            assert_eq!(lost, 2, "{} loses to {} elements", e.name(), lost);
        }
    }

    #[test]
    fn nothing_beats_itself_and_the_two_edges_never_coincide() {
        for e in Element::ALL {
            assert!(!e.beats(e));
            assert_ne!(e.eats(), e.suppresses());
            assert_ne!(e.eats(), e);
            assert_ne!(e.suppresses(), e);
        }
    }

    #[test]
    fn no_pair_beats_each_other() {
        for a in Element::ALL {
            for b in Element::ALL {
                assert!(!(a.beats(b) && b.beats(a)), "{} and {}", a.name(), b.name());
            }
        }
    }

    #[test]
    fn per_element_iterates_in_ring_order() {
        let p = PerElement([10, 20, 30, 40, 50]);
        let seen: Vec<_> = p.iter().map(|(e, v)| (e, *v)).collect();
        assert_eq!(seen[0], (Element::Wood, 10));
        assert_eq!(seen[4], (Element::Water, 50));
    }
}
