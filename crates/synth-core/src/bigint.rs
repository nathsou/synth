//! Arbitrary-precision signed integers for compile-time arithmetic.
//!
//! Small and simple rather than fast: frontends use them for constant
//! evaluation and value ranges, where operands rarely exceed a few hundred
//! bits.

use std::cmp::Ordering;
use std::fmt;
use std::ops::{Add, Mul, Neg, Sub};

use crate::{Logic, LogicVec};

/// A signed integer of any size (sign and magnitude, 32-bit limbs, least
/// significant first, no trailing zero limbs).
#[derive(Clone, PartialEq, Eq, Hash, Default)]
pub struct BigInt {
    neg: bool,
    mag: Vec<u32>,
}

fn trim(v: &mut Vec<u32>) {
    while v.last() == Some(&0) {
        v.pop();
    }
}

fn cmp_mag(a: &[u32], b: &[u32]) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.iter().rev().cmp(b.iter().rev()))
}

fn add_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len().max(b.len()) + 1);
    let mut carry = 0u64;
    for i in 0..a.len().max(b.len()) {
        let s = *a.get(i).unwrap_or(&0) as u64 + *b.get(i).unwrap_or(&0) as u64 + carry;
        out.push(s as u32);
        carry = s >> 32;
    }
    if carry > 0 {
        out.push(carry as u32);
    }
    out
}

/// `a - b` for `a >= b`.
fn sub_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i64;
    for (i, &x) in a.iter().enumerate() {
        let mut d = x as i64 - *b.get(i).unwrap_or(&0) as i64 - borrow;
        borrow = 0;
        if d < 0 {
            d += 1 << 32;
            borrow = 1;
        }
        out.push(d as u32);
    }
    trim(&mut out);
    out
}

fn mul_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let mut out = vec![0u32; a.len() + b.len()];
    for (i, &x) in a.iter().enumerate() {
        let mut carry = 0u64;
        for (j, &y) in b.iter().enumerate() {
            let t = out[i + j] as u64 + x as u64 * y as u64 + carry;
            out[i + j] = t as u32;
            carry = t >> 32;
        }
        out[i + b.len()] = carry as u32;
    }
    trim(&mut out);
    out
}

fn bit_len_mag(a: &[u32]) -> u64 {
    match a.last() {
        None => 0,
        Some(&top) => (a.len() as u64 - 1) * 32 + (32 - top.leading_zeros() as u64),
    }
}

fn shl_mag(a: &[u32], n: u64) -> Vec<u32> {
    if a.is_empty() {
        return Vec::new();
    }
    let (limbs, bits) = ((n / 32) as usize, (n % 32) as u32);
    let mut out = vec![0u32; limbs];
    let mut carry = 0u32;
    for &x in a {
        if bits == 0 {
            out.push(x);
        } else {
            out.push((x << bits) | carry);
            carry = x >> (32 - bits);
        }
    }
    if carry > 0 {
        out.push(carry);
    }
    out
}

/// Magnitude division by binary long division: `(a / b, a % b)`.
fn divrem_mag(a: &[u32], b: &[u32]) -> (Vec<u32>, Vec<u32>) {
    assert!(!b.is_empty(), "division by zero");
    if cmp_mag(a, b) == Ordering::Less {
        return (Vec::new(), a.to_vec());
    }
    if b.len() == 1 {
        let d = b[0] as u64;
        let mut q = vec![0u32; a.len()];
        let mut r = 0u64;
        for i in (0..a.len()).rev() {
            let cur = (r << 32) | a[i] as u64;
            q[i] = (cur / d) as u32;
            r = cur % d;
        }
        trim(&mut q);
        let mut rv = vec![r as u32];
        trim(&mut rv);
        return (q, rv);
    }
    let n = bit_len_mag(a);
    let mut q = vec![0u32; a.len()];
    let mut r: Vec<u32> = Vec::new();
    for i in (0..n).rev() {
        r = shl_mag(&r, 1);
        if (a[(i / 32) as usize] >> (i % 32)) & 1 == 1 {
            if r.is_empty() {
                r.push(1);
            } else {
                r[0] |= 1;
            }
        }
        if cmp_mag(&r, b) != Ordering::Less {
            r = sub_mag(&r, b);
            q[(i / 32) as usize] |= 1 << (i % 32);
        }
    }
    trim(&mut q);
    (q, r)
}

impl BigInt {
    pub fn zero() -> BigInt {
        BigInt::default()
    }

    pub fn from_i64(v: i64) -> BigInt {
        BigInt::from_i128(v as i128)
    }

    pub fn from_u64(v: u64) -> BigInt {
        BigInt::from_i128(v as i128)
    }

    pub fn from_i128(v: i128) -> BigInt {
        let neg = v < 0;
        let mut m = v.unsigned_abs();
        let mut mag = Vec::new();
        while m > 0 {
            mag.push(m as u32);
            m >>= 32;
        }
        BigInt { neg, mag }
    }

    /// `2^n`.
    pub fn pow2(n: u64) -> BigInt {
        BigInt { neg: false, mag: shl_mag(&[1], n) }
    }

    /// Parses digits (underscores allowed) in the given radix (2 to 16).
    pub fn parse(digits: &str, radix: u32) -> Option<BigInt> {
        let mut v = BigInt::zero();
        let r = BigInt::from_u64(radix as u64);
        let mut any = false;
        for c in digits.chars() {
            if c == '_' {
                continue;
            }
            let d = c.to_digit(radix)?;
            v = &(&v * &r) + &BigInt::from_u64(d as u64);
            any = true;
        }
        any.then_some(v)
    }

    pub fn is_zero(&self) -> bool {
        self.mag.is_empty()
    }

    pub fn is_negative(&self) -> bool {
        self.neg
    }

    pub fn abs(&self) -> BigInt {
        BigInt { neg: false, mag: self.mag.clone() }
    }

    /// Number of bits in the magnitude (0 for zero).
    pub fn bit_len(&self) -> u64 {
        bit_len_mag(&self.mag)
    }

    fn make(neg: bool, mut mag: Vec<u32>) -> BigInt {
        trim(&mut mag);
        let neg = neg && !mag.is_empty();
        BigInt { neg, mag }
    }

    /// Truncating division and remainder (the remainder has the sign of
    /// `self`), or `None` when dividing by zero.
    pub fn div_rem(&self, d: &BigInt) -> Option<(BigInt, BigInt)> {
        if d.is_zero() {
            return None;
        }
        let (q, r) = divrem_mag(&self.mag, &d.mag);
        Some((BigInt::make(self.neg != d.neg, q), BigInt::make(self.neg, r)))
    }

    /// Floor division (rounds toward negative infinity), or `None` for zero.
    pub fn div_floor(&self, d: &BigInt) -> Option<BigInt> {
        let (q, r) = self.div_rem(d)?;
        Some(if !r.is_zero() && (r.neg != d.neg) { &q - &BigInt::from_i64(1) } else { q })
    }

    /// `self * 2^n`.
    pub fn shl(&self, n: u64) -> BigInt {
        BigInt::make(self.neg, shl_mag(&self.mag, n))
    }

    /// `self ^ e`.
    pub fn pow(&self, e: u64) -> BigInt {
        let mut out = BigInt::from_i64(1);
        let mut base = self.clone();
        let mut e = e;
        while e > 0 {
            if e & 1 == 1 {
                out = &out * &base;
            }
            base = &base * &base;
            e >>= 1;
        }
        out
    }

    pub fn to_i64(&self) -> Option<i64> {
        if self.mag.len() > 2 {
            return None;
        }
        let m = self.mag.iter().rev().fold(0u128, |a, &x| (a << 32) | x as u128);
        let v = if self.neg { -(m as i128) } else { m as i128 };
        i64::try_from(v).ok()
    }

    pub fn to_u64(&self) -> Option<u64> {
        if self.neg { None } else { self.to_i64().and_then(|v| u64::try_from(v).ok()) }
    }

    /// Bit `i` of the two's complement representation.
    pub fn bit(&self, i: u64) -> bool {
        let get =
            |m: &[u32], i: u64| m.get((i / 32) as usize).is_some_and(|&x| (x >> (i % 32)) & 1 == 1);
        if !self.neg {
            return get(&self.mag, i);
        }
        // -m = !(m - 1)
        let m1 = sub_mag(&self.mag, &[1]);
        !get(&m1, i)
    }

    /// The low `width` bits of the two's complement representation.
    pub fn to_logic_vec(&self, width: usize) -> LogicVec {
        let bits: Vec<Logic> = (0..width as u64).map(|i| Logic::from_bool(self.bit(i))).collect();
        LogicVec::from_bits(&bits)
    }

    /// The value of fully known bits, read as unsigned or two's complement.
    pub fn from_logic_vec(v: &LogicVec, signed: bool) -> Option<BigInt> {
        if !v.is_known() {
            return None;
        }
        let w = v.width();
        let mut out = BigInt::zero();
        for i in (0..w).rev() {
            out = out.shl(1);
            if v.get(i) == Logic::One {
                out = &out + &BigInt::from_i64(1);
            }
        }
        if signed && w > 0 && v.get(w - 1) == Logic::One {
            out = &out - &BigInt::pow2(w as u64);
        }
        Some(out)
    }
}

impl Ord for BigInt {
    fn cmp(&self, o: &BigInt) -> Ordering {
        match (self.neg, o.neg) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => cmp_mag(&self.mag, &o.mag),
            (true, true) => cmp_mag(&o.mag, &self.mag),
        }
    }
}

impl PartialOrd for BigInt {
    fn partial_cmp(&self, o: &BigInt) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl Add for &BigInt {
    type Output = BigInt;
    fn add(self, o: &BigInt) -> BigInt {
        if self.neg == o.neg {
            return BigInt::make(self.neg, add_mag(&self.mag, &o.mag));
        }
        match cmp_mag(&self.mag, &o.mag) {
            Ordering::Less => BigInt::make(o.neg, sub_mag(&o.mag, &self.mag)),
            _ => BigInt::make(self.neg, sub_mag(&self.mag, &o.mag)),
        }
    }
}

impl Sub for &BigInt {
    type Output = BigInt;
    fn sub(self, o: &BigInt) -> BigInt {
        self + &(-o)
    }
}

impl Mul for &BigInt {
    type Output = BigInt;
    fn mul(self, o: &BigInt) -> BigInt {
        BigInt::make(self.neg != o.neg, mul_mag(&self.mag, &o.mag))
    }
}

impl Neg for &BigInt {
    type Output = BigInt;
    fn neg(self) -> BigInt {
        BigInt::make(!self.neg, self.mag.clone())
    }
}

impl From<i64> for BigInt {
    fn from(v: i64) -> BigInt {
        BigInt::from_i64(v)
    }
}

impl fmt::Display for BigInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_zero() {
            return f.write_str("0");
        }
        let mut digits = Vec::new();
        let mut m = self.mag.clone();
        let ten9 = [1_000_000_000u32];
        while !m.is_empty() {
            let (q, r) = divrem_mag(&m, &ten9);
            digits.push(r.first().copied().unwrap_or(0));
            m = q;
        }
        if self.neg {
            f.write_str("-")?;
        }
        write!(f, "{}", digits.last().unwrap())?;
        for d in digits.iter().rev().skip(1) {
            write!(f, "{d:09}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for BigInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(v: i128) -> BigInt {
        BigInt::from_i128(v)
    }

    #[test]
    fn arithmetic() {
        assert_eq!(&b(5) + &b(-7), b(-2));
        assert_eq!(&b(-5) - &b(-7), b(2));
        assert_eq!(&b(1 << 40) * &b(-(1 << 30)), b(-(1 << 70)));
        assert_eq!(b(-7).div_rem(&b(2)), Some((b(-3), b(-1))));
        assert_eq!(b(-7).div_floor(&b(2)), Some(b(-4)));
        let big = BigInt::pow2(200);
        let (q, r) = (&big + &b(5)).div_rem(&BigInt::pow2(100)).unwrap();
        assert_eq!(q, BigInt::pow2(100));
        assert_eq!(r, b(5));
        assert_eq!(BigInt::pow2(64).to_string(), "18446744073709551616");
        assert_eq!(b(-1234567890123).to_string(), "-1234567890123");
        assert_eq!(BigInt::parse("dead_beef", 16), Some(b(0xdead_beef)));
        assert_eq!(b(3).pow(5), b(243));
        assert!(b(-3) < b(2) && b(-3) > b(-4));
        assert_eq!(BigInt::pow2(8).bit_len(), 9);
    }

    #[test]
    fn twos_complement() {
        let v = b(-2).to_logic_vec(4);
        assert_eq!(v.to_u64(), Some(0b1110));
        assert_eq!(BigInt::from_logic_vec(&v, true), Some(b(-2)));
        assert_eq!(BigInt::from_logic_vec(&v, false), Some(b(14)));
    }
}
