//! ECP5 bitstreams: the configuration memory and block RAM contents as the
//! commands the device's configuration engine reads, byte for byte as
//! Project Trellis' `ecppack` writes them (and read back as `ecpunpack`
//! reads them).
//!
//! A bitstream is a header of comments, the preamble, then commands: reset
//! the CRC, check the device ID, set control register 0, write every frame
//! (last frame first, each followed by a CRC-16 and a dummy byte; or
//! compressed, a prefix code over a dictionary of frequent bytes), the
//! user code, each initialised block RAM's 2048 9-bit words, and DONE.

use std::collections::BTreeMap;

use crate::bits::Cram;
use crate::db::ChipInfo;

const PREAMBLE: [u8; 4] = [0xFF, 0xFF, 0xBD, 0xB3];
const CRC16_POLY: u16 = 0x8005;
/// Words of each block RAM.
pub const BRAM_WORDS: usize = 2048;

mod cmd {
    pub const SPI_MODE: u8 = 0b0111_1001;
    pub const JUMP: u8 = 0b0111_1110;
    pub const LSC_RESET_CRC: u8 = 0b0011_1011;
    pub const VERIFY_ID: u8 = 0b1110_0010;
    pub const LSC_WRITE_COMP_DIC: u8 = 0b0000_0010;
    pub const LSC_PROG_CNTRL0: u8 = 0b0010_0010;
    pub const LSC_INIT_ADDRESS: u8 = 0b0100_0110;
    pub const LSC_PROG_INCR_CMP: u8 = 0b1011_1000;
    pub const LSC_PROG_INCR_RTI: u8 = 0b1000_0010;
    pub const LSC_PROG_SED_CRC: u8 = 0b1010_0010;
    pub const ISC_PROGRAM_SECURITY: u8 = 0b1100_1110;
    pub const ISC_PROGRAM_USERCODE: u8 = 0b1100_0010;
    pub const LSC_EBR_ADDRESS: u8 = 0b1111_0110;
    pub const LSC_EBR_WRITE: u8 = 0b1011_0010;
    pub const ISC_PROGRAM_DONE: u8 = 0b0101_1110;
    pub const DUMMY: u8 = 0b1111_1111;
}

/// Frame options of ECP5 bitstreams: a CRC after each frame, one dummy
/// byte after each.
const CRC_META: u8 = 0x91;
const DUMMY_AFTER_PREAMBLE: usize = 4;
const SECURITY_SED_SPACE: usize = 12;

/// What goes in a bitstream besides the configuration memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// Comments in the header (`ecppack` copies the configuration's).
    pub comments: Vec<String>,
    pub usercode: u32,
    /// Control register 0 (`0x40000000`; its low bits set the master
    /// clock frequency of SPI boot).
    pub ctrl0: u32,
    pub compress: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { comments: Vec::new(), usercode: 0, ctrl0: 0x4000_0000, compress: false }
    }
}

/// A byte stream with the running CRC-16 of its configuration engine.
#[derive(Default)]
struct Writer {
    data: Vec<u8>,
    crc: u16,
}

fn crc_update(crc: &mut u16, val: u8) {
    for i in (0..8).rev() {
        let top = *crc >> 15;
        *crc = (*crc << 1) | ((val >> i) & 1) as u16;
        if top != 0 {
            *crc ^= CRC16_POLY;
        }
    }
}

fn crc_finish(mut crc: u16) -> u16 {
    for _ in 0..16 {
        let top = crc >> 15;
        crc <<= 1;
        if top != 0 {
            crc ^= CRC16_POLY;
        }
    }
    crc
}

impl Writer {
    fn byte(&mut self, b: u8) {
        self.data.push(b);
        crc_update(&mut self.crc, b);
    }

    fn bytes(&mut self, bs: &[u8]) {
        bs.iter().for_each(|&b| self.byte(b));
    }

    fn zeros(&mut self, n: usize) {
        (0..n).for_each(|_| self.byte(0));
    }

    /// Bytes the CRC does not cover.
    fn dummy(&mut self, n: usize) {
        self.data.extend(std::iter::repeat_n(0xFF, n));
    }

    fn u32(&mut self, v: u32) {
        self.bytes(&v.to_be_bytes());
    }

    fn crc(&mut self) {
        let c = crc_finish(self.crc);
        self.bytes(&c.to_be_bytes());
        self.crc = 0;
    }
}

/// The bytes of frame `idx`: bit `j` at bit `j + pad_after` counted from
/// the end of the frame.
fn frame_bytes(info: &ChipInfo, cram: &Cram, idx: usize, out: &mut [u8]) {
    out.fill(0);
    let n = out.len();
    for j in 0..info.bits_per_frame {
        if cram.get(idx, j) {
            let ofs = j + info.pad_bits_after_frame;
            out[n - 1 - ofs / 8] |= 1 << (ofs % 8);
        }
    }
}

fn bytes_per_frame(info: &ChipInfo) -> usize {
    (info.bits_per_frame + info.pad_bits_after_frame + info.pad_bits_before_frame) / 8
}

fn one_hot(b: u8) -> Option<u8> {
    (b.count_ones() == 1).then(|| b.trailing_zeros() as u8)
}

/// A `.bit` file of a configuration memory and block RAM contents.
pub fn write(
    info: &ChipInfo,
    cram: &Cram,
    bram: &BTreeMap<u16, Vec<u16>>,
    opts: &Options,
) -> Vec<u8> {
    let mut w = Writer::default();
    w.bytes(&PREAMBLE);
    w.dummy(DUMMY_AFTER_PREAMBLE);
    w.byte(cmd::LSC_RESET_CRC);
    w.zeros(3);
    w.crc = 0;
    w.byte(cmd::VERIFY_ID);
    w.zeros(3);
    w.u32(info.idcode);
    w.byte(cmd::LSC_PROG_CNTRL0);
    w.zeros(3);
    w.u32(opts.ctrl0);
    w.byte(cmd::LSC_INIT_ADDRESS);
    w.zeros(3);
    let bpf = bytes_per_frame(info);
    // Last frame first.
    let frames: Vec<Vec<u8>> = (0..info.frames)
        .map(|i| {
            let mut fb = vec![0; bpf];
            frame_bytes(info, cram, info.frames - 1 - i, &mut fb);
            fb
        })
        .collect();
    let count = info.frames as u16;
    if opts.compress {
        write_compressed(&mut w, &frames);
    } else {
        w.byte(cmd::LSC_PROG_INCR_RTI);
        w.byte(CRC_META);
        w.bytes(&count.to_be_bytes());
        for fb in &frames {
            w.bytes(fb);
            w.crc();
            w.byte(0xFF);
        }
    }
    w.dummy(SECURITY_SED_SPACE);
    w.byte(cmd::ISC_PROGRAM_USERCODE);
    w.byte(0x80);
    w.zeros(2);
    w.u32(opts.usercode);
    w.crc();
    for (&index, data) in bram {
        w.byte(cmd::LSC_EBR_ADDRESS);
        w.zeros(3);
        w.u32((index as u32) << 11);
        w.byte(cmd::LSC_EBR_WRITE);
        w.byte(0xD0);
        w.bytes(&((BRAM_WORDS >> 3) as u16).to_be_bytes());
        let word = |i: usize| data.get(i).copied().unwrap_or(0) & 0x1FF;
        // Eight 9-bit words in 9 bytes, most significant bit first.
        for a in (0..BRAM_WORDS).step_by(8) {
            let mut acc: u128 = 0;
            for k in 0..8 {
                acc = (acc << 9) | word(a + k) as u128;
            }
            let b = acc.to_be_bytes();
            w.bytes(&b[16 - 9..]);
        }
        w.crc();
    }
    w.byte(cmd::ISC_PROGRAM_DONE);
    w.zeros(3);
    w.dummy(4);
    let mut out = vec![0xFF, 0x00];
    for c in &opts.comments {
        out.extend_from_slice(c.as_bytes());
        out.push(0);
    }
    out.push(0xFF);
    out.extend(w.data);
    out
}

/// Frames in a prefix code: `0` a zero byte, `100xxx` a byte with only bit
/// `xxx` set, `101xxx` dictionary entry `xxx` (the eight most frequent
/// other bytes), `11xxxxxxxx` any byte; each frame padded at its start to
/// a multiple of 8 bytes and ending on a byte boundary.
fn write_compressed(w: &mut Writer, frames: &[Vec<u8>]) {
    let mut histogram = [0usize; 256];
    for &b in frames.iter().flatten() {
        histogram[b as usize] += 1;
    }
    // The most frequent first, the larger byte on a tie.
    let mut candidates: Vec<(usize, u8)> =
        (1..=255u8).filter(|&b| one_hot(b).is_none()).map(|b| (histogram[b as usize], b)).collect();
    candidates.sort_unstable_by(|a, b| b.cmp(a));
    let dict: Vec<u8> = candidates[..8].iter().map(|c| c.1).collect();
    w.byte(cmd::LSC_WRITE_COMP_DIC);
    w.zeros(3);
    for i in (0..8).rev() {
        w.byte(dict[i]);
    }
    w.byte(cmd::LSC_PROG_INCR_CMP);
    w.byte(CRC_META);
    w.bytes(&(frames.len() as u16).to_be_bytes());
    for fr in frames {
        let mut bits = BitWriter::default();
        if fr.len() % 8 != 0 {
            (0..8 - fr.len() % 8).for_each(|_| bits.add(0, 1));
        }
        for &b in fr {
            if b == 0 {
                bits.add(0, 1);
            } else if let Some(i) = one_hot(b) {
                bits.add(0b100, 3);
                bits.add(i as u32, 3);
            } else if let Some(j) = dict.iter().position(|&d| d == b) {
                bits.add(0b101, 3);
                bits.add(j as u32, 3);
            } else {
                bits.add(0b11, 2);
                bits.add(b as u32, 8);
            }
        }
        bits.flush();
        w.bytes(&bits.out);
        w.crc();
        w.byte(0xFF);
    }
}

#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    buf: u8,
    n: u32,
}

impl BitWriter {
    fn add(&mut self, x: u32, len: u32) {
        for i in (0..len).rev() {
            if x >> i & 1 != 0 {
                self.buf |= 1 << (7 - self.n);
            }
            self.n += 1;
            if self.n == 8 {
                self.flush();
            }
        }
    }

    fn flush(&mut self) {
        if self.n != 0 {
            self.out.push(self.buf);
            self.buf = 0;
            self.n = 0;
        }
    }
}

/// A bitstream read back.
#[derive(Clone, Debug)]
pub struct Parsed {
    pub info: ChipInfo,
    pub cram: Cram,
    pub bram: BTreeMap<u16, Vec<u16>>,
    pub comments: Vec<String>,
    pub usercode: u32,
    pub ctrl0: u32,
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    crc: u16,
}

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8, String> {
        let b = *self.data.get(self.pos).ok_or("bitstream ends unexpectedly")?;
        self.pos += 1;
        crc_update(&mut self.crc, b);
        Ok(b)
    }

    fn skip(&mut self, n: usize) -> Result<(), String> {
        for _ in 0..n {
            self.byte()?;
        }
        Ok(())
    }

    fn u32(&mut self) -> Result<u32, String> {
        let mut v = 0u32;
        for _ in 0..4 {
            v = (v << 8) | self.byte()? as u32;
        }
        Ok(v)
    }

    fn check_crc(&mut self) -> Result<(), String> {
        let expected = crc_finish(self.crc);
        let found = ((self.byte()? as u16) << 8) | self.byte()? as u16;
        if found != expected {
            return Err(format!(
                "CRC mismatch at byte {}: {found:#06x}, expected {expected:#06x}",
                self.pos - 2
            ));
        }
        self.crc = 0;
        Ok(())
    }
}

/// Reads a `.bit` file. `chip` gives a device's geometry from its ID code.
pub fn read(bytes: &[u8], chip: &dyn Fn(u32) -> Option<ChipInfo>) -> Result<Parsed, String> {
    let mut comments = Vec::new();
    let mut start = 0;
    if bytes.starts_with(&[0xFF, 0x00]) {
        let mut cur = Vec::new();
        let mut i = 2;
        loop {
            let c = *bytes.get(i).ok_or("bitstream header does not end")?;
            i += 1;
            match c {
                0xFF => break,
                0 => comments.push(String::from_utf8_lossy(&std::mem::take(&mut cur)).into_owned()),
                c => cur.push(c),
            }
        }
        start = i;
    }
    let pre = bytes[start..]
        .windows(4)
        .position(|w| w == PREAMBLE)
        .ok_or("no bitstream preamble found")?;
    let mut r = Reader { data: bytes, pos: start + pre + 4, crc: 0 };
    let mut found: Option<(ChipInfo, Cram)> = None;
    let (mut usercode, mut ctrl0) = (0, 0);
    let mut dict = [0u8; 16];
    let mut bram: BTreeMap<u16, Vec<u16>> = BTreeMap::new();
    let (mut ebr, mut ebr_addr) = (0u16, 0usize);
    while r.pos < bytes.len() {
        let op = bytes[r.pos];
        r.pos += 1;
        if op != cmd::DUMMY {
            crc_update(&mut r.crc, op);
        }
        match op {
            cmd::DUMMY => {}
            cmd::LSC_RESET_CRC => {
                r.skip(3)?;
                r.crc = 0;
            }
            cmd::VERIFY_ID => {
                r.skip(3)?;
                let id = r.u32()?;
                let info = chip(id).ok_or_else(|| format!("unknown device ID {id:#010x}"))?;
                let cram = Cram::new(info.frames, info.bits_per_frame);
                found = Some((info, cram));
            }
            cmd::LSC_PROG_CNTRL0 => {
                r.skip(3)?;
                ctrl0 = r.u32()?;
            }
            cmd::LSC_INIT_ADDRESS => r.skip(3)?,
            cmd::SPI_MODE => r.skip(3)?,
            cmd::JUMP => r.skip(7)?,
            cmd::LSC_PROG_SED_CRC => r.skip(7)?,
            cmd::ISC_PROGRAM_SECURITY => r.skip(3)?,
            cmd::ISC_PROGRAM_DONE => {
                let check = r.byte()? & 0x80 != 0;
                r.skip(2)?;
                if check {
                    r.check_crc()?;
                }
            }
            cmd::ISC_PROGRAM_USERCODE => {
                let check = r.byte()? & 0x80 != 0;
                r.skip(2)?;
                usercode = r.u32()?;
                if check {
                    r.check_crc()?;
                }
                r.crc = 0;
            }
            cmd::LSC_WRITE_COMP_DIC => {
                let check = r.byte()? & 0x80 != 0;
                r.skip(2)?;
                for i in (0..8).rev() {
                    dict[i] = 1 << i;
                    dict[8 + i] = r.byte()?;
                }
                if check {
                    r.check_crc()?;
                }
            }
            cmd::LSC_PROG_INCR_RTI | cmd::LSC_PROG_INCR_CMP => {
                let (info, cram) = found.as_mut().ok_or("frames before the device ID")?;
                let params = [r.byte()?, r.byte()?, r.byte()?];
                let check = params[0] & 0x80 != 0;
                let each = check && params[0] & 0x40 == 0;
                let dummies = (params[0] & 0x0F) as usize;
                let count = ((params[1] as usize) << 8) | params[2] as usize;
                let compressed = op == cmd::LSC_PROG_INCR_CMP;
                let mut bpf = bytes_per_frame(info);
                if compressed {
                    bpf += 7 - (bpf - 1) % 8;
                }
                let mut fb = vec![0u8; bpf];
                for i in 0..count {
                    let idx = info.frames.checked_sub(1 + i).ok_or("too many frames")?;
                    if compressed {
                        read_compressed(&mut r, &mut fb, &dict)?;
                    } else {
                        for b in fb.iter_mut() {
                            *b = r.byte()?;
                        }
                    }
                    for j in 0..info.bits_per_frame {
                        let ofs = j + info.pad_bits_after_frame;
                        cram.set(idx, j, fb[bpf - 1 - ofs / 8] >> (ofs % 8) & 1 != 0);
                    }
                    if each || (check && i == count - 1) {
                        r.check_crc()?;
                    }
                    r.skip(dummies)?;
                }
            }
            cmd::LSC_EBR_ADDRESS => {
                r.skip(3)?;
                let a = r.u32()?;
                ebr = ((a >> 11) & 0x3FF) as u16;
                ebr_addr = (a & 0x7FF) as usize;
                bram.entry(ebr).or_insert_with(|| vec![0; BRAM_WORDS]);
            }
            cmd::LSC_EBR_WRITE => {
                let params = [r.byte()?, r.byte()?, r.byte()?];
                let count = ((params[1] as usize) << 8) | params[2] as usize;
                for _ in 0..count {
                    if ebr_addr >= BRAM_WORDS {
                        ebr_addr = 0;
                        ebr += 1;
                    }
                    let mut acc: u128 = 0;
                    for _ in 0..9 {
                        acc = (acc << 8) | r.byte()? as u128;
                    }
                    let words = bram.entry(ebr).or_insert_with(|| vec![0; BRAM_WORDS]);
                    for k in 0..8 {
                        words[ebr_addr + k] = ((acc >> (9 * (7 - k))) & 0x1FF) as u16;
                    }
                    ebr_addr += 8;
                }
                if params[0] & 0x80 != 0 {
                    r.check_crc()?;
                }
            }
            op => {
                return Err(format!(
                    "unsupported bitstream command {op:#04x} at byte {}",
                    r.pos - 1
                ));
            }
        }
    }
    let (info, cram) = found.ok_or("no device ID in the bitstream")?;
    Ok(Parsed { info, cram, bram, comments, usercode, ctrl0 })
}

/// One frame's bytes from the prefix code of [`write_compressed`].
fn read_compressed(r: &mut Reader, out: &mut [u8], dict: &[u8; 16]) -> Result<(), String> {
    let mut acc: u32 = 0;
    let mut have = 0u32;
    let mut take = |r: &mut Reader, n: u32| -> Result<u32, String> {
        while have < n {
            acc = (acc << 8) | r.byte()? as u32;
            have += 8;
        }
        have -= n;
        Ok((acc >> have) & ((1 << n) - 1))
    };
    for b in out.iter_mut() {
        *b = if take(r, 1)? == 0 {
            0
        } else if take(r, 1)? == 1 {
            take(r, 8)? as u8
        } else {
            dict[take(r, 4)? as usize]
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> ChipInfo {
        ChipInfo {
            name: "test".into(),
            idcode: 0x4111_1043,
            frames: 6,
            bits_per_frame: 30,
            pad_bits_before_frame: 2,
            pad_bits_after_frame: 0,
            max_row: 1,
            max_col: 1,
            packages: Vec::new(),
        }
    }

    #[test]
    fn bitstreams_read_back() {
        let info = info();
        let mut cram = Cram::new(info.frames, info.bits_per_frame);
        for (f, b) in [(0, 0), (0, 29), (3, 7), (5, 12), (5, 13), (2, 1)] {
            cram.set(f, b, true);
        }
        let mut bram = BTreeMap::new();
        bram.insert(3, (0..BRAM_WORDS as u16).map(|i| i.wrapping_mul(37) & 0x1FF).collect());
        for compress in [false, true] {
            let opts = Options {
                comments: vec!["Part: test".into()],
                usercode: 7,
                compress,
                ..Options::default()
            };
            let bytes = write(&info, &cram, &bram, &opts);
            let p = read(&bytes, &|id| (id == info.idcode).then(|| info.clone())).unwrap();
            assert_eq!(p.cram, cram, "compress {compress}");
            assert_eq!(p.bram, bram);
            assert_eq!((p.usercode, p.ctrl0, p.comments), (7, 0x4000_0000, opts.comments.clone()));
        }
    }

    #[test]
    fn corruption_fails_the_crc() {
        let info = info();
        let cram = Cram::new(info.frames, info.bits_per_frame);
        let mut bytes = write(&info, &cram, &BTreeMap::new(), &Options::default());
        let frame_data = bytes.len() / 2;
        bytes[frame_data] ^= 0x10;
        let err = read(&bytes, &|_| Some(info.clone())).unwrap_err();
        assert!(err.contains("CRC") || err.contains("unsupported"), "{err}");
    }
}
