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

    /// Animal-vs-Plant diet: same element, no ring offset. "Eats" is
    /// animal-only behavior -- an Animal of element X eats the Kind-sibling
    /// Plant of the same element X's product, not a ring-adjacent element.
    #[inline]
    pub const fn eats_plant(self) -> Element {
        self
    }

    /// Animal-vs-Animal predation: the original ring-backward relation,
    /// unchanged. Kept under its own name because grazing (eats_plant) and
    /// hunting no longer share one relation.
    #[inline]
    pub const fn eats_animal(self) -> Element {
        self.eats()
    }

    /// What a race draws down from terrain to sustain itself -- one
    /// ring-step removed from what it deposits (mathematically identical to
    /// eats_animal, kept as its own name because this is terrain
    /// consumption, not predation). Read by apply_conversion (every race)
    /// and phase_flora's rooting gate (Plants only).
    #[inline]
    pub const fn habitat(self) -> Element {
        self.eats()
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
