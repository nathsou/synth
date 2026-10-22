//! Series-7 configuration frames and packet stream (UG470).
//!
//! Frame ECC and startup sequencing were cross-checked against Project X-Ray
//! (ISC); see THIRD_PARTY.md. Streams are big-endian, uncompressed and unencrypted.
use crate::device::Part;
use std::collections::BTreeMap;

pub const WORDS: usize = 101;
pub type Frame = [u32; WORDS];
pub type Frames = BTreeMap<u32, Frame>;
const NOP: u32 = 0x20000000;

pub fn blank(part: &Part) -> Frames {
    part.frames.iter().map(|&a| (a, [0; WORDS])).collect()
}
/// Extended Hamming code in word 50, bits 12:0. Existing ECC is ignored.
pub fn ecc(frame: &Frame) -> u32 {
    let mut result = 0u32;
    for (word, &data) in frame.iter().enumerate() {
        let mut data = if word == 50 { data & !0x1fff } else { data };
        let base = (word as u32) * 32
            + if word > 37 {
                0x1360
            } else if word > 6 {
                0x1340
            } else {
                0x1320
            };
        while data != 0 {
            let bit = data.trailing_zeros();
            result ^= base + bit;
            data &= data - 1;
        }
    }
    result ^ (((result & 0xfff).count_ones() & 1) << 12)
}
/// ICAP CRC32C over the 32 data bits followed by five register address bits.
pub fn crc_word(mut crc: u32, register: u8, data: u32) -> u32 {
    let mut bits = (u64::from(register & 31) << 32) | u64::from(data);
    for _ in 0..37 {
        let xor = (crc ^ bits as u32) & 1;
        crc >>= 1;
        if xor != 0 {
            crc ^= 0x82f63b78;
        }
        bits >>= 1;
    }
    crc
}
fn row(a: u32) -> u32 {
    a & !0x1ffff
}

/// Matches X-Ray's startup sequence; `check_crc` adds CRC checks before startup
/// and DESYNC. ECC is always recalculated. All device frames must be supplied.
pub fn encode(part: &Part, frames: &Frames, check_crc: bool) -> Result<Vec<u8>, String> {
    if !frames.keys().copied().eq(part.frames.iter().copied()) {
        return Err("configuration must contain exactly the part's frame addresses".into());
    }
    let mut payload = Vec::new();
    for (i, &a) in part.frames.iter().enumerate() {
        let mut f = frames[&a];
        f[50] = (f[50] & !0x1fff) | ecc(&f);
        payload.extend(f);
        if part.frames.get(i + 1).is_none_or(|&b| row(a) != row(b)) {
            payload.extend([0; WORDS * 2]);
        }
    }
    if payload.len() >= 1 << 27 {
        return Err("FDRI payload too large".into());
    }
    let mut w = Writer { words: vec![u32::MAX; 8], crc: 0 };
    w.words.extend([0xbb, 0x11220044, u32::MAX, u32::MAX, 0xaa995566]);
    w.nops(1);
    w.reg(17, 0);
    w.reg(16, 0);
    w.reg(4, 0);
    w.nops(1);
    w.reg(4, 7);
    w.nops(2);
    w.reg(19, 0);
    w.reg(9, 0x02003fe5);
    w.reg(14, 0);
    w.reg(12, part.idcode);
    w.reg(4, 9);
    w.nops(1);
    w.reg(6, 0x401);
    w.reg(5, 0x501);
    w.reg(6, 0);
    w.reg(24, 0);
    w.nops(8);
    w.reg(1, part.frames[0]);
    w.reg(4, 1);
    w.nops(1);
    w.words.extend([0x30004000, 0x50000000 | payload.len() as u32]);
    for &v in &payload {
        w.crc = crc_word(w.crc, 2, v);
    }
    w.words.extend(payload);
    if check_crc {
        w.reg(0, w.crc);
    }
    w.reg(4, 7);
    w.nops(2);
    w.reg(4, 10);
    w.nops(1);
    w.reg(4, 3);
    w.nops(100);
    w.reg(4, 5);
    w.nops(1);
    w.reg(1, 0x03be0000);
    w.reg(6, 0x501);
    w.reg(5, 0x501);
    if check_crc {
        w.reg(0, w.crc);
    }
    w.reg(4, 7);
    w.nops(2);
    w.reg(4, 13);
    w.nops(400);
    Ok(w.words.into_iter().flat_map(u32::to_be_bytes).collect())
}
struct Writer {
    words: Vec<u32>,
    crc: u32,
}
impl Writer {
    fn nops(&mut self, n: usize) {
        self.words.extend(std::iter::repeat_n(NOP, n));
    }
    fn reg(&mut self, r: u8, v: u32) {
        self.words.extend([0x30000001 | (u32::from(r) << 13), v]);
        self.crc = if r == 4 && v == 7 || r == 0 { 0 } else { crc_word(self.crc, r, v) };
    }
}

/// Read full-device, noncompressed Series-7 streams, validating IDCODE, frame
/// boundaries, ECC and every CRC packet. Rejects partial/multiple FDRI writes.
pub fn decode(part: &Part, bytes: &[u8]) -> Result<Frames, String> {
    let sync =
        bytes.windows(4).position(|w| w == [0xaa, 0x99, 0x55, 0x66]).ok_or("missing sync word")?;
    let bytes = &bytes[sync + 4..];
    if !bytes.len().is_multiple_of(4) {
        return Err("truncated configuration word".into());
    }
    let words: Vec<u32> = bytes.as_chunks::<4>().0.iter().map(|b| u32::from_be_bytes(*b)).collect();
    let (mut i, mut reg, mut crc, mut far) = (0usize, None, 0u32, None);
    let (mut id, mut started, mut desync) = (false, false, false);
    let mut result = None;
    while i < words.len() {
        let h = words[i];
        i += 1;
        if h == NOP || h == u32::MAX {
            continue;
        }
        if desync {
            return Err("packet after DESYNC".into());
        }
        let (r, n) = match h >> 29 {
            1 => {
                if (h >> 27) & 3 != 2 || h & 0x1800 != 0 {
                    return Err(format!("unsupported type-1 header {h:08x}"));
                }
                let r = ((h >> 13) & 0x3fff) as u16;
                if r > 31 {
                    return Err("unsupported configuration register".into());
                }
                reg = Some(r as u8);
                (r as u8, (h & 0x7ff) as usize)
            }
            2 => {
                if (h >> 27) & 3 != 2 {
                    return Err("unsupported type-2 opcode".into());
                }
                (reg.ok_or("type-2 without type-1")?, (h & 0x07ffffff) as usize)
            }
            _ => return Err(format!("invalid packet header {h:08x}")),
        };
        let end =
            i.checked_add(n).filter(|&e| e <= words.len()).ok_or("truncated packet payload")?;
        let data = &words[i..end];
        i = end;
        if r == 2 && n > 0 {
            if !id || far != part.frames.first().copied() || result.is_some() {
                return Err("unsupported partial or repeated frame write".into());
            }
            let mut frames = Frames::new();
            let mut p = 0;
            for (j, &a) in part.frames.iter().enumerate() {
                let f: Frame = data.get(p..p + WORDS).ok_or("truncated frame")?.try_into().unwrap();
                p += WORDS;
                if f[50] & 0x1fff != ecc(&f) {
                    return Err(format!("ECC mismatch in frame {a:08x}"));
                }
                frames.insert(a, f);
                if part.frames.get(j + 1).is_none_or(|&b| row(a) != row(b)) {
                    if data.get(p..p + 2 * WORDS).is_none_or(|v| v.iter().any(|&w| w != 0)) {
                        return Err("invalid row padding".into());
                    }
                    p += 2 * WORDS;
                }
            }
            if p != data.len() {
                return Err("extra FDRI data".into());
            }
            result = Some(frames);
        }
        for &v in data {
            if r == 0 {
                if v != crc {
                    return Err(format!("CRC mismatch: {v:08x}, expected {crc:08x}"));
                }
                crc = 0;
                continue;
            }
            if r == 12 {
                if v != part.idcode {
                    return Err("bitstream IDCODE does not match part".into());
                }
                id = true;
            }
            if r == 1 {
                far = Some(v);
            }
            if r == 4 {
                if v == 7 {
                    crc = 0;
                    continue;
                }
                if v == 5 {
                    started = true;
                }
                if v == 13 {
                    desync = true;
                }
            }
            crc = crc_word(crc, r, v);
        }
    }
    if !started || !desync {
        return Err("incomplete startup sequence".into());
    }
    result.ok_or_else(|| "no frame data".into())
}

/// Deterministic Xilinx .bit container; dates do not vary between builds.
pub fn bit_file(part: &Part, design: &str, raw: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = vec![0, 9, 0x0f, 0xf0, 0x0f, 0xf0, 0x0f, 0xf0, 0x0f, 0xf0, 0, 0, 1];
    for (tag, s) in
        [(b'a', design), (b'b', part.name.as_str()), (b'c', "1970/01/01"), (b'd', "00:00:00")]
    {
        let n = u16::try_from(s.len() + 1).map_err(|_| "bit header field too long")?;
        if s.contains('\0') {
            return Err("NUL in bit header".into());
        }
        out.push(tag);
        out.extend(n.to_be_bytes());
        out.extend(s.as_bytes());
        out.push(0);
    }
    out.push(b'e');
    out.extend(u32::try_from(raw.len()).map_err(|_| "bitstream too large")?.to_be_bytes());
    out.extend(raw);
    Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independent_crc_vectors() {
        // Project X-Ray xc7series/crc_test.cc vectors (ISC; THIRD_PARTY.md).
        assert_eq!(crc_word(0, 0, 0), 0);
        assert_eq!(crc_word(0, 16, 0), 0x82f63b78);
        assert_eq!(crc_word(0, 31, u32::MAX), 0xbf86d4df);
        assert_eq!(crc_word(u32::MAX, 0, 0), 0xc631e365);
    }
    #[test]
    fn integrity_and_boundaries() {
        let part = Part {
            name: "test".into(),
            idcode: 0x12345678,
            frames: vec![0, 1, 0x80, 0x20000, 0x400000, 0x800000],
        };
        let mut f = blank(&part);
        for (&a, v) in &mut f {
            for (i, w) in v.iter_mut().enumerate() {
                *w = a.wrapping_mul(1234567) ^ (i as u32).wrapping_mul(7654321);
            }
            v[50] = (v[50] & !0x1fff) | ecc(v);
        }
        for crc in [false, true] {
            let b = encode(&part, &f, crc).unwrap();
            assert_eq!(decode(&part, &b).unwrap(), f);
            assert_eq!(decode(&part, &bit_file(&part, "test", &b).unwrap()).unwrap(), f);
            assert!(decode(&part, &b[..b.len() - 1]).is_err());
        }
        let mut b = encode(&part, &f, true).unwrap();
        let pos = b.windows(4).position(|v| v == 0x30004000u32.to_be_bytes()).unwrap();
        b[pos + 12] ^= 1;
        assert!(decode(&part, &b).unwrap_err().contains("ECC"));
        let mut b = encode(&part, &f, true).unwrap();
        let pos = b.windows(4).position(|v| v == 0x30012001u32.to_be_bytes()).unwrap();
        b[pos + 5] ^= 1;
        assert!(decode(&part, &b).unwrap_err().contains("CRC"));
    }
}
