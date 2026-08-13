//! 16.16 fixed-point scalar — the only numeric type permitted in simulation state.
//!
//! Invariant II. There is deliberately no `From<f32>`, no `From<f64>`, and no
//! arithmetic that touches a float. The single escape hatch is
//! [`Fx::to_f32_render`], whose name is the entire point: if you see it called
//! anywhere outside a renderer, that is the bug.
//!
//! Every operation saturates rather than wrapping or panicking. Saturation is
//! deterministic, total, and produces a visibly wrong number instead of a
//! silently wrong one.

use core::ops::{Add, AddAssign, Div, Mul, Neg, Sub, SubAssign};

pub const SHIFT: u32 = 16;
pub const ONE_RAW: i32 = 1 << SHIFT;

/// A 16.16 signed fixed-point number. Range approximately ±32768 with a
/// resolution of 1/65536.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Fx(i32);

impl Fx {
    pub const ZERO: Fx = Fx(0);
    pub const ONE: Fx = Fx(ONE_RAW);
    pub const HALF: Fx = Fx(ONE_RAW / 2);
    pub const MAX: Fx = Fx(i32::MAX);
    pub const MIN: Fx = Fx(i32::MIN);

    /// Exact whole number. Saturates outside the representable range.
    #[inline]
    pub const fn from_int(n: i32) -> Fx {
        match n.checked_mul(ONE_RAW) {
            Some(v) => Fx(v),
            None => {
                if n > 0 {
                    Fx(i32::MAX)
                } else {
                    Fx(i32::MIN)
                }
            }
        }
    }

    /// Exact rational. This is how you write a constant that would otherwise
    /// tempt you into a float literal: `Fx::ratio(3, 4)` rather than `0.75`.
    #[inline]
    pub const fn ratio(num: i32, den: i32) -> Fx {
        if den == 0 {
            return Fx(0);
        }
        Fx((((num as i64) << SHIFT) / (den as i64)) as i32)
    }

    #[inline]
    pub const fn from_raw(raw: i32) -> Fx {
        Fx(raw)
    }

    #[inline]
    pub const fn raw(self) -> i32 {
        self.0
    }

    /// Truncates toward negative infinity, so `-0.5` floors to `-1`.
    #[inline]
    pub const fn floor_int(self) -> i32 {
        self.0 >> SHIFT
    }

    #[inline]
    pub const fn round_int(self) -> i32 {
        (self.0 + (ONE_RAW / 2)) >> SHIFT
    }

    #[inline]
    pub const fn frac(self) -> Fx {
        Fx(self.0 & (ONE_RAW - 1))
    }

    #[inline]
    pub const fn abs(self) -> Fx {
        if self.0 < 0 {
            Fx(self.0.saturating_neg())
        } else {
            self
        }
    }

    #[inline]
    pub fn min(self, other: Fx) -> Fx {
        if self.0 < other.0 {
            self
        } else {
            other
        }
    }

    #[inline]
    pub fn max(self, other: Fx) -> Fx {
        if self.0 > other.0 {
            self
        } else {
            other
        }
    }

    #[inline]
    pub fn clamp(self, lo: Fx, hi: Fx) -> Fx {
        self.max(lo).min(hi)
    }

    #[inline]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub const fn signum(self) -> i32 {
        if self.0 > 0 {
            1
        } else if self.0 < 0 {
            -1
        } else {
            0
        }
    }

    /// Deterministic integer square root. Newton's method on integers — no
    /// float path, identical on every platform.
    pub fn sqrt(self) -> Fx {
        if self.0 <= 0 {
            return Fx::ZERO;
        }
        // sqrt of a 16.16 value: isqrt(raw << 16) lands back in 16.16.
        let scaled = (self.0 as u64) << SHIFT;
        Fx(isqrt_u64(scaled) as i32)
    }

    /// Renderer-only. Calling this inside `world`, `governor` or any other
    /// simulation module is a violation of Invariant II.
    #[inline]
    pub fn to_f32_render(self) -> f32 {
        #[allow(clippy::float_arithmetic)]
        {
            self.0 as f32 / ONE_RAW as f32
        }
    }
}

/// Bit-by-bit integer square root. Deterministic, branch-stable, no floats.
pub fn isqrt_u64(n: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    let mut rem: u64 = 0;
    let mut root: u64 = 0;
    // 32 iterations covers the full u64 domain.
    for i in (0..32).rev() {
        root <<= 1;
        rem = (rem << 2) | ((n >> (i * 2)) & 0x3);
        if root < rem {
            root += 1;
            rem -= root;
            root += 1;
        }
    }
    root >> 1
}

impl Add for Fx {
    type Output = Fx;
    #[inline]
    fn add(self, rhs: Fx) -> Fx {
        Fx(self.0.saturating_add(rhs.0))
    }
}

impl Sub for Fx {
    type Output = Fx;
    #[inline]
    fn sub(self, rhs: Fx) -> Fx {
        Fx(self.0.saturating_sub(rhs.0))
    }
}

impl Neg for Fx {
    type Output = Fx;
    #[inline]
    fn neg(self) -> Fx {
        Fx(self.0.saturating_neg())
    }
}

impl Mul for Fx {
    type Output = Fx;
    #[inline]
    fn mul(self, rhs: Fx) -> Fx {
        let wide = (self.0 as i64) * (rhs.0 as i64);
        Fx(saturate_i64(wide >> SHIFT))
    }
}

impl Mul<i32> for Fx {
    type Output = Fx;
    #[inline]
    fn mul(self, rhs: i32) -> Fx {
        Fx(saturate_i64((self.0 as i64) * (rhs as i64)))
    }
}

impl Div for Fx {
    type Output = Fx;
    #[inline]
    fn div(self, rhs: Fx) -> Fx {
        if rhs.0 == 0 {
            // Total rather than panicking: division by zero saturates with the
            // sign of the numerator. Deterministic on every platform.
            return match self.0.signum() {
                0 => Fx::ZERO,
                s if s > 0 => Fx::MAX,
                _ => Fx::MIN,
            };
        }
        let wide = ((self.0 as i64) << SHIFT) / (rhs.0 as i64);
        Fx(saturate_i64(wide))
    }
}

impl AddAssign for Fx {
    #[inline]
    fn add_assign(&mut self, rhs: Fx) {
        *self = *self + rhs;
    }
}

impl SubAssign for Fx {
    #[inline]
    fn sub_assign(&mut self, rhs: Fx) {
        *self = *self - rhs;
    }
}

#[inline]
fn saturate_i64(v: i64) -> i32 {
    if v > i32::MAX as i64 {
        i32::MAX
    } else if v < i32::MIN as i64 {
        i32::MIN
    } else {
        v as i32
    }
}

impl core::fmt::Debug for Fx {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Prints the exact rational, never a float round-trip.
        let whole = self.0 >> SHIFT;
        let frac = (self.0 & (ONE_RAW - 1)) as u64;
        let milli = (frac * 1000) >> SHIFT;
        write!(f, "{}.{:03}fx", whole, milli)
    }
}

/// A 2-D fixed-point vector. Positions and velocities only.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug, Hash)]
pub struct V2 {
    pub x: Fx,
    pub y: Fx,
}

impl V2 {
    pub const ZERO: V2 = V2 {
        x: Fx::ZERO,
        y: Fx::ZERO,
    };

    #[inline]
    pub const fn new(x: Fx, y: Fx) -> V2 {
        V2 { x, y }
    }

    #[inline]
    pub fn scale(self, s: Fx) -> V2 {
        V2::new(self.x * s, self.y * s)
    }

    /// Squared length in `Fx`. Convenient for coarse comparisons, but it
    /// truncates: squaring a small component loses most of its significant
    /// bits. Anything that needs precision should use [`V2::len`] or
    /// [`V2::normalized`], which work from the raw squares instead.
    #[inline]
    pub fn len_sq(self) -> Fx {
        self.x * self.x + self.y * self.y
    }

    /// Sum of the raw squares. Because a raw value is `v << 16`, this is
    /// `len² << 32` — so an integer sqrt of it lands back in 16.16 with no
    /// intermediate rounding at all.
    #[inline]
    fn len_sq_raw(self) -> u64 {
        let x = self.x.0 as i64;
        let y = self.y.0 as i64;
        ((x * x) as u64).saturating_add((y * y) as u64)
    }

    /// Exact to within one ulp, including for very short vectors.
    #[inline]
    pub fn len(self) -> Fx {
        Fx(saturate_i64(isqrt_u64(self.len_sq_raw()) as i64))
    }

    /// Unit vector, or zero if the input is zero.
    ///
    /// Divides at full precision rather than dividing two `Fx` values, because
    /// headings scale every movement in the simulation — a heading that is two
    /// percent long is an entity that moves two percent too fast, forever, and
    /// mobility is the parameter that decides whether five biomes coexist.
    pub fn normalized(self) -> V2 {
        let mut x = self.x.0 as i64;
        let mut y = self.y.0 as i64;
        if x == 0 && y == 0 {
            return V2::ZERO;
        }

        // Direction is scale-invariant, so scale the vector up until its
        // magnitude occupies ~30 bits before taking the root. Without this, a
        // vector only a few raw units long has too few significant bits left
        // after the sqrt to divide by — `(1, 1)` raw would normalise to `(1, 1)`.
        let m = x.unsigned_abs().max(y.unsigned_abs());
        let sh = m.leading_zeros().saturating_sub(33);
        x <<= sh;
        y <<= sh;

        let l = isqrt_u64(((x * x) as u64).saturating_add((y * y) as u64)) as i64;
        if l == 0 {
            return V2::ZERO;
        }
        V2::new(
            Fx(saturate_i64((x << SHIFT) / l)),
            Fx(saturate_i64((y << SHIFT) / l)),
        )
    }
}

impl Add for V2 {
    type Output = V2;
    #[inline]
    fn add(self, o: V2) -> V2 {
        V2::new(self.x + o.x, self.y + o.y)
    }
}

impl Sub for V2 {
    type Output = V2;
    #[inline]
    fn sub(self, o: V2) -> V2 {
        V2::new(self.x - o.x, self.y - o.y)
    }
}

impl Neg for V2 {
    type Output = V2;
    #[inline]
    fn neg(self) -> V2 {
        V2::new(-self.x, -self.y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_roundtrip() {
        for n in [-1000, -1, 0, 1, 7, 1000] {
            assert_eq!(Fx::from_int(n).floor_int(), n);
        }
    }

    #[test]
    fn ratio_is_exact_where_it_can_be() {
        assert_eq!(Fx::ratio(1, 2), Fx::HALF);
        assert_eq!(Fx::ratio(4, 2), Fx::from_int(2));
        assert_eq!(Fx::ratio(-1, 2), -Fx::HALF);
    }

    #[test]
    fn mul_div_roundtrip_within_one_ulp() {
        let a = Fx::ratio(355, 113);
        let b = Fx::ratio(7, 3);
        let back = (a * b) / b;
        assert!((back - a).abs().raw() <= 2, "got {:?} want {:?}", back, a);
    }

    #[test]
    fn div_by_zero_is_total_and_signed() {
        assert_eq!(Fx::from_int(5) / Fx::ZERO, Fx::MAX);
        assert_eq!(Fx::from_int(-5) / Fx::ZERO, Fx::MIN);
        assert_eq!(Fx::ZERO / Fx::ZERO, Fx::ZERO);
    }

    #[test]
    fn add_saturates_instead_of_wrapping() {
        assert_eq!(Fx::MAX + Fx::ONE, Fx::MAX);
        assert_eq!(Fx::MIN - Fx::ONE, Fx::MIN);
    }

    #[test]
    fn isqrt_matches_known_values() {
        assert_eq!(isqrt_u64(0), 0);
        assert_eq!(isqrt_u64(1), 1);
        assert_eq!(isqrt_u64(4), 2);
        assert_eq!(isqrt_u64(99), 9);
        assert_eq!(isqrt_u64(100), 10);
        assert_eq!(isqrt_u64(u32::MAX as u64), 65535);
    }

    #[test]
    fn sqrt_is_close() {
        let r = Fx::from_int(144).sqrt();
        assert!((r - Fx::from_int(12)).abs().raw() <= 2, "{:?}", r);
        let r2 = Fx::from_int(2).sqrt();
        // 1.41421... in 16.16 is 92681
        assert!((r2.raw() - 92681).abs() <= 2, "{:?}", r2);
    }

    #[test]
    fn normalized_is_unit_length() {
        let v = V2::new(Fx::from_int(3), Fx::from_int(4));
        assert_eq!(v.len(), Fx::from_int(5));
        let n = v.normalized();
        assert!((n.len() - Fx::ONE).abs().raw() <= 4, "{:?}", n.len());
    }

    #[test]
    fn normalized_zero_is_zero() {
        assert_eq!(V2::ZERO.normalized(), V2::ZERO);
    }

    #[test]
    fn normalized_is_accurate_for_very_short_vectors() {
        // The case that the naive two-step version gets wrong: components
        // small enough that squaring them in 16.16 discards their precision.
        for raw in [1i32, 3, 17, 128, 655, 4096] {
            let v = V2::new(Fx::from_raw(raw), Fx::from_raw(raw / 2 + 1));
            let n = v.normalized();
            assert!(
                (n.len() - Fx::ONE).abs().raw() <= 4,
                "raw {} normalised to length {:?}",
                raw,
                n.len()
            );
        }
    }

    #[test]
    fn len_is_exact_for_pythagorean_triples() {
        for (a, b, c) in [(3, 4, 5), (5, 12, 13), (8, 15, 17), (20, 21, 29)] {
            let v = V2::new(Fx::from_int(a), Fx::from_int(b));
            assert_eq!(v.len(), Fx::from_int(c), "{}-{}-{}", a, b, c);
        }
    }
}
