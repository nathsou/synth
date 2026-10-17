//! Four-state logic values and arbitrary-width four-state bit vectors with
//! Verilog semantics, used for constant evaluation and constant folding.
//!
//! Bits are stored as two planes: `val` and `unk`. A bit is `0` (unk=0,
//! val=0), `1` (unk=0, val=1), `x` (unk=1, val=0) or `z` (unk=1, val=1).
//! Arithmetic with any unknown operand bit yields all-`x`, as in Verilog.

use std::cmp::Ordering;
use std::fmt;

/// A single four-state bit.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Logic {
    Zero,
    One,
    X,
    Z,
}

impl Logic {
    pub fn from_bool(b: bool) -> Logic {
        if b { Logic::One } else { Logic::Zero }
    }

    pub fn from_char(c: char) -> Option<Logic> {
        match c {
            '0' => Some(Logic::Zero),
            '1' => Some(Logic::One),
            'x' | 'X' => Some(Logic::X),
            'z' | 'Z' | '?' => Some(Logic::Z),
            _ => None,
        }
    }

    pub fn to_char(self) -> char {
        match self {
            Logic::Zero => '0',
            Logic::One => '1',
            Logic::X => 'x',
            Logic::Z => 'z',
        }
    }

    pub fn is_known(self) -> bool {
        matches!(self, Logic::Zero | Logic::One)
    }

    pub fn and(self, o: Logic) -> Logic {
        match (self, o) {
            (Logic::Zero, _) | (_, Logic::Zero) => Logic::Zero,
            (Logic::One, Logic::One) => Logic::One,
            _ => Logic::X,
        }
    }

    pub fn or(self, o: Logic) -> Logic {
        match (self, o) {
            (Logic::One, _) | (_, Logic::One) => Logic::One,
            (Logic::Zero, Logic::Zero) => Logic::Zero,
            _ => Logic::X,
        }
    }

    pub fn xor(self, o: Logic) -> Logic {
        match (self, o) {
            (Logic::Zero, Logic::Zero) | (Logic::One, Logic::One) => Logic::Zero,
            (Logic::Zero, Logic::One) | (Logic::One, Logic::Zero) => Logic::One,
            _ => Logic::X,
        }
    }
}

impl std::ops::Not for Logic {
    type Output = Logic;
    fn not(self) -> Logic {
        match self {
            Logic::Zero => Logic::One,
            Logic::One => Logic::Zero,
            _ => Logic::X,
        }
    }
}

/// An arbitrary-width four-state bit vector, least significant bit first.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct LogicVec {
    width: usize,
    val: Vec<u64>,
    unk: Vec<u64>,
}

fn words(width: usize) -> usize {
    width.div_ceil(64).max(1)
}

impl LogicVec {
    /// All-zero vector.
    pub fn zeros(width: usize) -> Self {
        LogicVec { width, val: vec![0; words(width)], unk: vec![0; words(width)] }
    }

    pub fn ones(width: usize) -> Self {
        let mut v = Self::zeros(width);
        for w in &mut v.val {
            *w = u64::MAX;
        }
        v.mask();
        v
    }

    /// All-x vector.
    pub fn xs(width: usize) -> Self {
        let mut v = Self::zeros(width);
        for w in &mut v.unk {
            *w = u64::MAX;
        }
        v.mask();
        v
    }

    /// All-z vector.
    pub fn zs(width: usize) -> Self {
        let mut v = Self::xs(width);
        v.val.clone_from(&v.unk);
        v
    }

    pub fn filled(width: usize, b: Logic) -> Self {
        match b {
            Logic::Zero => Self::zeros(width),
            Logic::One => Self::ones(width),
            Logic::X => Self::xs(width),
            Logic::Z => Self::zs(width),
        }
    }

    pub fn from_u64(v: u64, width: usize) -> Self {
        let mut r = Self::zeros(width);
        r.val[0] = v;
        r.mask();
        r
    }

    /// Two's complement of `v`, sign-extended to `width`.
    pub fn from_i64(v: i64, width: usize) -> Self {
        let mut r = Self::zeros(width);
        for i in 0..r.val.len() {
            r.val[i] = if i == 0 {
                v as u64
            } else if v < 0 {
                u64::MAX
            } else {
                0
            };
        }
        r.mask();
        r
    }

    pub fn from_bool(b: bool) -> Self {
        Self::from_u64(b as u64, 1)
    }

    pub fn from_bits(bits: &[Logic]) -> Self {
        let mut r = Self::zeros(bits.len());
        for (i, &b) in bits.iter().enumerate() {
            r.set(i, b);
        }
        r
    }

    /// Parses digits in base 2, 8, 16 (with x/z/? digits) or 10 (no x/z),
    /// most significant first, into a vector of `width` bits. Underscores
    /// are ignored. Returns `None` on an invalid digit.
    pub fn parse_radix(digits: &str, radix: u32, width: usize) -> Option<Self> {
        let digits: Vec<char> = digits.chars().filter(|&c| c != '_').collect();
        if digits.is_empty() {
            return None;
        }
        if radix == 10 {
            if digits.len() == 1 && matches!(digits[0], 'x' | 'X') {
                return Some(Self::xs(width));
            }
            if digits.len() == 1 && matches!(digits[0], 'z' | 'Z' | '?') {
                return Some(Self::zs(width));
            }
            let mut r = Self::zeros(width.max(64));
            let ten = Self::from_u64(10, r.width);
            for c in digits {
                let d = c.to_digit(10)?;
                r = r.mul(&ten).add(&Self::from_u64(d as u64, r.width));
            }
            return Some(r.resize(width, false));
        }
        let bits_per = match radix {
            2 => 1,
            8 => 3,
            16 => 4,
            _ => return None,
        };
        let mut bits: Vec<Logic> = Vec::with_capacity(digits.len() * bits_per);
        for &c in digits.iter().rev() {
            let b = match c {
                'x' | 'X' => [Logic::X; 4],
                'z' | 'Z' | '?' => [Logic::Z; 4],
                _ => {
                    let d = c.to_digit(radix)?;
                    [0, 1, 2, 3].map(|i| Logic::from_bool(d >> i & 1 != 0))
                }
            };
            bits.extend_from_slice(&b[..bits_per]);
        }
        // Verilog: if the value is narrower than the size, extend with 0,
        // or with x/z when the most significant digit is x/z.
        let msb = bits.last().copied().unwrap_or(Logic::Zero);
        let ext = if msb.is_known() { Logic::Zero } else { msb };
        bits.resize(width.max(bits.len()), ext);
        bits.truncate(width);
        Some(Self::from_bits(&bits))
    }

    fn mask(&mut self) {
        let rem = self.width % 64;
        let n = words(self.width);
        if self.width == 0 {
            self.val[0] = 0;
            self.unk[0] = 0;
        } else if rem != 0 {
            self.val[n - 1] &= (1u64 << rem) - 1;
            self.unk[n - 1] &= (1u64 << rem) - 1;
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    #[inline]
    pub fn get(&self, i: usize) -> Logic {
        debug_assert!(i < self.width);
        let (w, b) = (i / 64, i % 64);
        match (self.unk[w] >> b & 1, self.val[w] >> b & 1) {
            (0, 0) => Logic::Zero,
            (0, _) => Logic::One,
            (_, 0) => Logic::X,
            _ => Logic::Z,
        }
    }

    #[inline]
    pub fn set(&mut self, i: usize, v: Logic) {
        debug_assert!(i < self.width);
        let (w, b) = (i / 64, i % 64);
        let (u, x) = match v {
            Logic::Zero => (0, 0),
            Logic::One => (0, 1),
            Logic::X => (1, 0),
            Logic::Z => (1, 1),
        };
        self.unk[w] = (self.unk[w] & !(1 << b)) | (u << b);
        self.val[w] = (self.val[w] & !(1 << b)) | (x << b);
    }

    pub fn bits(&self) -> impl DoubleEndedIterator<Item = Logic> + ExactSizeIterator + '_ {
        (0..self.width).map(|i| self.get(i))
    }

    pub fn to_bits(&self) -> Vec<Logic> {
        self.bits().collect()
    }

    /// True when no bit is x or z.
    pub fn is_known(&self) -> bool {
        self.unk.iter().all(|&w| w == 0)
    }

    pub fn is_zero(&self) -> bool {
        self.is_known() && self.val.iter().all(|&w| w == 0)
    }

    /// The value as unsigned, if known and it fits in 64 bits.
    pub fn to_u64(&self) -> Option<u64> {
        if !self.is_known() || self.val[1..].iter().any(|&w| w != 0) {
            return None;
        }
        Some(self.val[0])
    }

    /// The value interpreted as signed (two's complement), if known and in range.
    pub fn to_i64(&self, signed: bool) -> Option<i64> {
        if !self.is_known() {
            return None;
        }
        if !signed || self.width == 0 || self.get(self.width - 1) == Logic::Zero {
            return self.to_u64().and_then(|v| i64::try_from(v).ok());
        }
        // Negative: -(~x + 1)
        let mag = self.neg();
        let m = mag.to_u64()?;
        if m <= i64::MAX as u64 + 1 { Some((m as i64).wrapping_neg()) } else { None }
    }

    /// Truthiness: `Some(true)` if any bit is 1, `Some(false)` if all are 0,
    /// `None` (x) otherwise.
    pub fn truthy(&self) -> Option<bool> {
        let any_one = self.val.iter().zip(&self.unk).any(|(&v, &u)| v & !u != 0);
        if any_one {
            Some(true)
        } else if self.is_known() {
            Some(false)
        } else {
            None
        }
    }

    /// Resizes to `width` bits, sign-extending if `signed`.
    pub fn resize(&self, width: usize, signed: bool) -> Self {
        let ext = if signed && self.width > 0 { self.get(self.width - 1) } else { Logic::Zero };
        let mut r = Self::zeros(width);
        let n = self.width.min(width);
        // Copy whole words, then fix the tail.
        let full = n / 64;
        r.val[..full].copy_from_slice(&self.val[..full]);
        r.unk[..full].copy_from_slice(&self.unk[..full]);
        for i in full * 64..n {
            r.set(i, self.get(i));
        }
        if ext != Logic::Zero {
            for i in n..width {
                r.set(i, ext);
            }
        }
        r
    }

    /// Bits `lo..lo + width`; positions outside the vector read as x.
    pub fn slice(&self, lo: isize, width: usize) -> Self {
        let mut r = Self::zeros(width);
        for i in 0..width {
            let j = lo + i as isize;
            let b =
                if j >= 0 && (j as usize) < self.width { self.get(j as usize) } else { Logic::X };
            r.set(i, b);
        }
        r
    }

    /// Concatenation: `parts[0]` is the least significant part.
    pub fn concat(parts: &[&LogicVec]) -> Self {
        let width = parts.iter().map(|p| p.width).sum();
        let mut r = Self::zeros(width);
        let mut pos = 0;
        for p in parts {
            for i in 0..p.width {
                r.set(pos + i, p.get(i));
            }
            pos += p.width;
        }
        r
    }

    pub fn repeat(&self, n: usize) -> Self {
        let parts: Vec<&LogicVec> = std::iter::repeat_n(self, n).collect();
        Self::concat(&parts)
    }

    fn map2(&self, o: &Self, f: impl Fn(Logic, Logic) -> Logic) -> Self {
        debug_assert_eq!(self.width, o.width);
        let mut r = Self::zeros(self.width);
        for i in 0..self.width {
            r.set(i, f(self.get(i), o.get(i)));
        }
        r
    }

    /// Bitwise complement.
    pub fn inv(&self) -> Self {
        let mut r = Self::zeros(self.width);
        for i in 0..self.width {
            r.set(i, !self.get(i));
        }
        r
    }

    pub fn and(&self, o: &Self) -> Self {
        self.map2(o, Logic::and)
    }

    pub fn or(&self, o: &Self) -> Self {
        self.map2(o, Logic::or)
    }

    pub fn xor(&self, o: &Self) -> Self {
        self.map2(o, Logic::xor)
    }

    pub fn xnor(&self, o: &Self) -> Self {
        self.map2(o, |a, b| !a.xor(b))
    }

    pub fn reduce_and(&self) -> Logic {
        self.bits().fold(Logic::One, Logic::and)
    }

    pub fn reduce_or(&self) -> Logic {
        self.bits().fold(Logic::Zero, Logic::or)
    }

    pub fn reduce_xor(&self) -> Logic {
        self.bits().fold(Logic::Zero, Logic::xor)
    }

    // ---- arithmetic (operands must have equal widths; any x gives all-x) ----

    pub fn add(&self, o: &Self) -> Self {
        debug_assert_eq!(self.width, o.width);
        if !self.is_known() || !o.is_known() {
            return Self::xs(self.width);
        }
        let mut r = Self::zeros(self.width);
        let mut carry = 0u64;
        for i in 0..r.val.len() {
            let (s1, c1) = self.val[i].overflowing_add(o.val[i]);
            let (s2, c2) = s1.overflowing_add(carry);
            r.val[i] = s2;
            carry = (c1 || c2) as u64;
        }
        r.mask();
        r
    }

    pub fn neg(&self) -> Self {
        if !self.is_known() {
            return Self::xs(self.width);
        }
        self.inv().add(&Self::from_u64(1, self.width))
    }

    pub fn sub(&self, o: &Self) -> Self {
        if !self.is_known() || !o.is_known() {
            return Self::xs(self.width);
        }
        self.add(&o.neg())
    }

    pub fn mul(&self, o: &Self) -> Self {
        debug_assert_eq!(self.width, o.width);
        if !self.is_known() || !o.is_known() {
            return Self::xs(self.width);
        }
        let n = self.val.len();
        let mut acc = vec![0u64; n];
        for i in 0..n {
            let mut carry = 0u128;
            for j in 0..n - i {
                let cur = acc[i + j] as u128 + self.val[i] as u128 * o.val[j] as u128 + carry;
                acc[i + j] = cur as u64;
                carry = cur >> 64;
            }
        }
        let mut r = LogicVec { width: self.width, val: acc, unk: vec![0; n] };
        r.mask();
        r
    }

    fn ucmp(&self, o: &Self) -> Ordering {
        for i in (0..self.val.len()).rev() {
            match self.val[i].cmp(&o.val[i]) {
                Ordering::Equal => {}
                ord => return ord,
            }
        }
        Ordering::Equal
    }

    /// Unsigned division and remainder; `None` on division by zero.
    fn udivrem(&self, o: &Self) -> Option<(Self, Self)> {
        if o.is_zero() {
            return None;
        }
        if let (Some(a), Some(b)) = (self.to_u64(), o.to_u64()) {
            return Some((Self::from_u64(a / b, self.width), Self::from_u64(a % b, self.width)));
        }
        let mut q = Self::zeros(self.width);
        let mut r = Self::zeros(self.width);
        for i in (0..self.width).rev() {
            r = r.shl(1);
            r.set(0, self.get(i));
            if r.ucmp(o) != Ordering::Less {
                r = r.sub(o);
                q.set(i, Logic::One);
            }
        }
        Some((q, r))
    }

    fn is_negative(&self) -> bool {
        self.width > 0 && self.get(self.width - 1) == Logic::One
    }

    /// Division (truncating toward zero); x on unknown operand or division by zero.
    pub fn div(&self, o: &Self, signed: bool) -> Self {
        if !self.is_known() || !o.is_known() {
            return Self::xs(self.width);
        }
        let (na, nb) = (signed && self.is_negative(), signed && o.is_negative());
        let a = if na { self.neg() } else { self.clone() };
        let b = if nb { o.neg() } else { o.clone() };
        match a.udivrem(&b) {
            None => Self::xs(self.width),
            Some((q, _)) => {
                if na != nb {
                    q.neg()
                } else {
                    q
                }
            }
        }
    }

    /// Remainder with the sign of the dividend.
    pub fn rem(&self, o: &Self, signed: bool) -> Self {
        if !self.is_known() || !o.is_known() {
            return Self::xs(self.width);
        }
        let (na, nb) = (signed && self.is_negative(), signed && o.is_negative());
        let a = if na { self.neg() } else { self.clone() };
        let b = if nb { o.neg() } else { o.clone() };
        match a.udivrem(&b) {
            None => Self::xs(self.width),
            Some((_, r)) => {
                if na {
                    r.neg()
                } else {
                    r
                }
            }
        }
    }

    /// Exponentiation `self ** o` (Verilog rules for negative/zero bases).
    pub fn pow(&self, o: &Self, signed_base: bool, signed_exp: bool) -> Self {
        if !self.is_known() || !o.is_known() {
            return Self::xs(self.width);
        }
        let w = self.width;
        if signed_exp && o.is_negative() {
            // Negative exponent: 1 for base 1, ±1 for base -1, x for 0, else 0.
            let one = Self::from_u64(1, w);
            if *self == one {
                return one;
            }
            if signed_base && *self == Self::ones(w) {
                return if o.get(0) == Logic::One { Self::ones(w) } else { one };
            }
            if self.is_zero() {
                return Self::xs(w);
            }
            return Self::zeros(w);
        }
        let mut result = Self::from_u64(1, w);
        let mut base = self.clone();
        for i in 0..o.width {
            if o.get(i) == Logic::One {
                result = result.mul(&base);
            }
            base = base.mul(&base);
            if i > 64 && base.is_zero() {
                break;
            }
        }
        result
    }

    /// Logical shift left by `n` bits.
    pub fn shl(&self, n: usize) -> Self {
        let mut r = Self::zeros(self.width);
        for i in n..self.width {
            r.set(i, self.get(i - n));
        }
        r
    }

    /// Shift right by `n`, filling with the sign bit if `arith`.
    pub fn shr(&self, n: usize, arith: bool) -> Self {
        let fill = if arith && self.width > 0 { self.get(self.width - 1) } else { Logic::Zero };
        let mut r = Self::zeros(self.width);
        for i in 0..self.width {
            r.set(i, if i + n < self.width { self.get(i + n) } else { fill });
        }
        r
    }

    /// `==`: 1/0, or x if an unknown bit could change the result.
    pub fn logic_eq(&self, o: &Self) -> Logic {
        let mut unknown = false;
        for i in 0..self.width {
            let (a, b) = (self.get(i), o.get(i));
            if a.is_known() && b.is_known() {
                if a != b {
                    return Logic::Zero;
                }
            } else {
                unknown = true;
            }
        }
        if unknown { Logic::X } else { Logic::One }
    }

    /// `===`: exact match including x and z.
    pub fn case_eq(&self, o: &Self) -> bool {
        self == o
    }

    /// Ordering comparison; `None` when either operand has unknown bits.
    pub fn cmp(&self, o: &Self, signed: bool) -> Option<Ordering> {
        if !self.is_known() || !o.is_known() {
            return None;
        }
        if signed {
            match (self.is_negative(), o.is_negative()) {
                (true, false) => return Some(Ordering::Less),
                (false, true) => return Some(Ordering::Greater),
                _ => {}
            }
        }
        Some(self.ucmp(o))
    }

    /// Ceiling log2 of an unsigned value, as `$clog2` defines it.
    pub fn clog2(&self) -> Option<u64> {
        if !self.is_known() {
            return None;
        }
        if self.is_zero() {
            return Some(0);
        }
        // Number of bits needed to represent (v - 1).
        let m = self.sub(&Self::from_u64(1, self.width));
        Some((0..m.width).rev().find(|&i| m.get(i) == Logic::One).map_or(0, |i| i as u64 + 1))
    }

    /// Renders as `<width>'b<bits>` (most significant first).
    pub fn to_bin_string(&self) -> String {
        self.bits().rev().map(Logic::to_char).collect()
    }
}

impl fmt::Debug for LogicVec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}'b{}", self.width, self.to_bin_string())
    }
}

impl fmt::Display for LogicVec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.to_u64() {
            Some(v) if self.width <= 64 => write!(f, "{}'d{v}", self.width),
            _ => write!(f, "{}'b{}", self.width, self.to_bin_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(x: u64, w: usize) -> LogicVec {
        LogicVec::from_u64(x, w)
    }

    #[test]
    fn parse() {
        assert_eq!(LogicVec::parse_radix("1010", 2, 8).unwrap().to_u64(), Some(10));
        assert_eq!(LogicVec::parse_radix("ff", 16, 4).unwrap().to_u64(), Some(15));
        assert_eq!(LogicVec::parse_radix("123_456", 10, 32).unwrap().to_u64(), Some(123456));
        let x = LogicVec::parse_radix("x1", 16, 8).unwrap();
        assert_eq!(x.to_bin_string(), "xxxx0001");
        let z = LogicVec::parse_radix("z", 2, 4).unwrap();
        assert_eq!(z.to_bin_string(), "zzzz");
        assert!(LogicVec::parse_radix("12", 2, 4).is_none());
        let big = LogicVec::parse_radix("18446744073709551616", 10, 72).unwrap();
        assert_eq!(big.get(64), Logic::One);
    }

    #[test]
    fn arithmetic() {
        assert_eq!(v(200, 8).add(&v(100, 8)).to_u64(), Some(44));
        assert_eq!(v(3, 8).sub(&v(5, 8)).to_u64(), Some(254));
        assert_eq!(v(3, 8).sub(&v(5, 8)).to_i64(true), Some(-2));
        assert_eq!(v(20, 8).mul(&v(13, 8)).to_u64(), Some(4));
        let a = LogicVec::from_i64(-7, 8);
        let b = v(2, 8);
        assert_eq!(a.div(&b, true).to_i64(true), Some(-3));
        assert_eq!(a.rem(&b, true).to_i64(true), Some(-1));
        assert_eq!(v(7, 8).div(&v(0, 8), false), LogicVec::xs(8));
        assert_eq!(v(3, 8).pow(&v(4, 8), false, false).to_u64(), Some(81));
        assert_eq!(v(1, 70).shl(65).get(65), Logic::One);
        let w = LogicVec::from_u64(u64::MAX, 128).mul(&LogicVec::from_u64(u64::MAX, 128));
        assert_eq!(w.slice(64, 64).to_u64(), Some(u64::MAX - 1));
        assert_eq!(w.slice(0, 64).to_u64(), Some(1));
        let big_a = LogicVec::from_u64(1, 100).shl(80);
        let (q, r) = big_a.udivrem(&LogicVec::from_u64(3, 100)).unwrap();
        assert_eq!(q.mul(&LogicVec::from_u64(3, 100)).add(&r), big_a);
    }

    #[test]
    fn unknowns_and_compare() {
        let x = LogicVec::parse_radix("1x", 2, 2).unwrap();
        assert_eq!(x.add(&v(1, 2)), LogicVec::xs(2));
        assert_eq!(x.logic_eq(&v(0, 2)), Logic::Zero);
        assert_eq!(x.logic_eq(&v(2, 2)), Logic::X);
        assert_eq!(x.truthy(), Some(true));
        assert_eq!(LogicVec::from_i64(-1, 4).cmp(&v(1, 4), true), Some(Ordering::Less));
        assert_eq!(LogicVec::from_i64(-1, 4).cmp(&v(1, 4), false), Some(Ordering::Greater));
        assert_eq!(v(9, 8).clog2(), Some(4));
        assert_eq!(v(8, 8).clog2(), Some(3));
        assert_eq!(v(1, 8).clog2(), Some(0));
        assert_eq!(LogicVec::from_i64(-3, 4).resize(8, true).to_i64(true), Some(-3));
        assert_eq!(v(0b1011, 4).shr(1, true).to_bin_string(), "1101");
    }
}
