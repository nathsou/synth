// Programming a board through its FTDI bridge in MPSSE mode: the iCE40
// boards by writing their SPI configuration flash (as iceprog does), the
// Artix-7 boards by loading the FPGA over JTAG (UG470, "JTAG
// configuration"). The protocols only need `write(bytes)` and `read(n)`,
// so they run the same on a WebUSB device (ftdi.js) and on the emulated
// board of the tests.

// MPSSE commands.
const SET_LOW = 0x80;
const GET_LOW = 0x81;
const LOOPBACK_OFF = 0x85;
const DIVISOR = 0x86;
const SEND_IMMEDIATE = 0x87;
const DIV5_OFF = 0x8a;
const THREE_PHASE_OFF = 0x8d;
const CLOCK_BYTES = 0x8f; // clocks without data, 8 per count
const ADAPTIVE_OFF = 0x97;
// Data: MSB first, out on the falling edge (SPI mode 0)...
const SPI_OUT = 0x11;
const SPI_INOUT = 0x31;
// ...and LSB first, out on the falling edge, in on the rising (JTAG).
const JTAG_OUT_BYTES = 0x19;
const JTAG_OUT_BITS = 0x1b;
const JTAG_INOUT_BYTES = 0x39;
const JTAG_INOUT_BITS = 0x3b;
const TMS_OUT = 0x4b;
const TMS_INOUT = 0x6b;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** MPSSE commands, sent in batches; a batch's reads come back together. */
export class Mpsse {
  constructor(io) {
    this.io = io;
    this.buf = [];
    this.expect = 0;
  }

  push(...bytes) {
    for (const b of bytes) this.buf.push(b);
  }

  pushData(data) {
    for (let i = 0; i < data.length; i++) this.buf.push(data[i]);
  }

  /** Sends the batch and returns what its commands read. */
  async flush() {
    if (this.expect) this.buf.push(SEND_IMMEDIATE);
    const n = this.expect;
    this.expect = 0;
    if (this.buf.length) await this.io.write(Uint8Array.from(this.buf));
    this.buf = [];
    return n ? this.io.read(n) : new Uint8Array(0);
  }

  /** Synchronizes with the engine and sets the clock to 60 MHz / (2 (div + 1)). */
  async init(div) {
    // A bad command is echoed as 0xFA and the command: proof the engine
    // listens, and that the read stream is aligned.
    this.push(0xaa);
    this.expect = 2;
    const r = await this.flush();
    if (r[0] !== 0xfa || r[1] !== 0xaa) throw new Error('the board\'s USB bridge is not answering in MPSSE mode');
    this.push(DIV5_OFF, ADAPTIVE_OFF, THREE_PHASE_OFF, LOOPBACK_OFF, DIVISOR, div & 0xff, div >> 8);
    await this.flush();
  }
}

// ----- iCE40: the SPI flash -----

// iceprog's wiring, shared by the Go Board and the Alchitry Cu.
const SCK = 0x01;
const MOSI = 0x02;
const CS = 0x10;
const CDONE = 0x40;
const CRESET = 0x80;
const SPI_DIR = SCK | MOSI | CS | CRESET;

class Flash {
  constructor(m) {
    this.m = m;
  }

  /** Sets chip select and the FPGA's reset (both active low). */
  pins(cs, creset) {
    this.m.push(SET_LOW, (cs ? CS : 0) | (creset ? CRESET : 0), SPI_DIR);
  }

  /** One transaction: sends `bytes`, then reads `n` bytes. */
  async txn(bytes, n = 0) {
    const m = this.m;
    this.pins(false, false);
    const len = bytes.length + n;
    m.push(n ? SPI_INOUT : SPI_OUT, (len - 1) & 0xff, (len - 1) >> 8);
    m.pushData(bytes);
    for (let i = 0; i < n; i++) m.push(0);
    m.expect += n ? len : 0;
    this.pins(true, false);
    if (!n) return null;
    const r = await m.flush();
    return r.subarray(r.length - n);
  }

  async wait(what) {
    for (let i = 0; i < 100000; i++) {
      const [s] = await this.txn([0x05], 1);
      if (!(s & 1)) return;
    }
    throw new Error(`the flash did not finish ${what}`);
  }

  async writeEnable() {
    await this.txn([0x06]);
  }

  async read(addr, n) {
    const out = new Uint8Array(n);
    for (let o = 0; o < n; o += 4096) {
      const k = Math.min(4096, n - o);
      const a = addr + o;
      out.set(await this.txn([0x03, a >> 16, (a >> 8) & 0xff, a & 0xff], k), o);
    }
    return out;
  }
}

/** Writes an iCE40 bitstream to the configuration flash and starts the FPGA. */
export async function programIce40(io, image, progress = () => {}) {
  const m = new Mpsse(io);
  await m.init(4); // 6 MHz
  const f = new Flash(m);
  // Hold the FPGA in reset, so that the flash is ours.
  f.pins(true, false);
  await m.flush();
  await sleep(100);
  // Leave a continuous read mode, then wake the flash up.
  await f.txn([0xff]);
  await f.txn([0xab]);
  const id = await f.txn([0x9f], 3);
  if ((id[0] === 0xff && id[1] === 0xff) || (id[0] === 0 && id[1] === 0)) {
    throw new Error('no flash answered: is this an iCE40 board (the Go Board or the Alchitry Cu)?');
  }
  const blocks = Math.ceil(image.length / 0x10000);
  for (let b = 0; b < blocks; b++) {
    progress('Erasing the flash', b / blocks);
    await f.writeEnable();
    await f.txn([0xd8, b, 0, 0]);
    await f.wait('erasing');
  }
  for (let a = 0; a < image.length; a += 256) {
    if (!(a & 0x1fff)) progress('Writing the flash', a / image.length);
    await f.writeEnable();
    await f.txn([0x02, a >> 16, (a >> 8) & 0xff, a & 0xff, ...image.subarray(a, a + 256)]);
    await f.wait('writing');
  }
  progress('Verifying the flash', 0);
  const back = await f.read(0, image.length);
  const bad = back.findIndex((v, i) => v !== image[i]);
  if (bad >= 0) throw new Error(`the flash reads back differently at address 0x${bad.toString(16)}`);
  // Power the flash down and let the FPGA load from it.
  await f.txn([0xb9]);
  f.pins(true, true);
  await m.flush();
  await sleep(250);
  m.push(GET_LOW);
  m.expect = 1;
  const [pins] = await m.flush();
  return { flashId: [...id].map((v) => v.toString(16).padStart(2, '0')).join(''), done: !!(pins & CDONE) };
}

// ----- Artix-7: JTAG -----

const IR_LEN = 6;
const IDCODE = 0x09;
const CFG_IN = 0x05;
const JPROGRAM = 0x0b;
const JSTART = 0x0c;
const BYPASS = 0x3f;
// What the IR captures (UG470): bit 4 INIT_COMPLETE, bit 5 DONE.
const IR_INIT = 0x10;
const IR_DONE = 0x20;

/** JTAG idle levels of the boards' bridges: the low byte's value and
 *  direction. TCK, TDI and TMS are outputs; Digilent's bridge also drives
 *  ADBUS5..7, which enable its JTAG buffers. */
const JTAG_PINS = {
  digilent: [0xe8, 0xeb],
  plain: [0x08, 0x0b],
};

/** Device IDs (the version nibble masked out). */
const IDCODES = {
  0x362e093: 'xc7a15t',
  0x362d093: 'xc7a35t',
  0x362c093: 'xc7a50t',
  0x3632093: 'xc7a75t',
  0x3631093: 'xc7a100t',
  0x3636093: 'xc7a200t',
};

class Jtag {
  constructor(m) {
    this.m = m;
  }

  /** Clocks up to 7 TMS bits (LSB first) with TDI held at `tdi`. */
  tms(bits, n, tdi = 0, read = false) {
    this.m.push(read ? TMS_INOUT : TMS_OUT, n - 1, (tdi << 7) | bits);
    if (read) this.m.expect += 1;
  }

  /** Test-Logic-Reset, then Run-Test/Idle. */
  reset() {
    this.tms(0b011111, 6);
  }

  /** Loads an instruction; with `read`, returns what the IR captured. */
  async ir(code, read = false) {
    this.tms(0b0011, 4); // RTI -> Shift-IR
    const m = this.m;
    m.push(read ? JTAG_INOUT_BITS : JTAG_OUT_BITS, IR_LEN - 2, code & ((1 << (IR_LEN - 1)) - 1));
    if (read) m.expect += 1;
    this.tms(0b1, 1, (code >> (IR_LEN - 1)) & 1, read); // last bit, to Exit1-IR
    this.tms(0b01, 2); // Update-IR -> RTI
    if (!read) return null;
    const r = await m.flush();
    const [lo, hi] = r.subarray(r.length - 2);
    return (lo >> (9 - IR_LEN)) | (((hi >> 7) & 1) << (IR_LEN - 1));
  }

  /** Reads 32 bits of DR. */
  async dr32() {
    const m = this.m;
    this.tms(0b001, 3);
    m.push(JTAG_INOUT_BYTES, 2, 0, 0, 0, 0);
    m.push(JTAG_INOUT_BITS, 6, 0);
    m.expect += 4;
    this.tms(0b1, 1, 0, true);
    this.tms(0b01, 2);
    const r = await m.flush();
    const b = r.subarray(r.length - 5);
    return (b[0] | (b[1] << 8) | (b[2] << 16) | ((b[3] >> 1) << 24) | (((b[4] >> 7) & 1) << 31)) >>> 0;
  }

  /** Clocks TCK in Run-Test/Idle. */
  idle(cycles) {
    for (let n = Math.ceil(cycles / 8); n > 0; n -= 0x10000) {
      const k = Math.min(n, 0x10000);
      this.m.push(CLOCK_BYTES, (k - 1) & 0xff, (k - 1) >> 8);
    }
  }
}

/** The parts of a Xilinx .bit file: the design and part names, and the
 *  configuration data. */
export function parseBit(bytes) {
  const v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  let p = 2 + v.getUint16(0);
  p += 2; // the length of the first key, 1
  const out = {};
  for (;;) {
    if (p >= bytes.length) throw new Error('not a .bit file');
    const key = String.fromCharCode(bytes[p++]);
    if (key === 'e') {
      const n = v.getUint32(p);
      out.data = bytes.subarray(p + 4, p + 4 + n);
      if (out.data.length !== n) throw new Error('the .bit file is truncated');
      return out;
    }
    const n = v.getUint16(p);
    const s = new TextDecoder().decode(bytes.subarray(p + 2, p + 2 + n - 1));
    if (key === 'a') out.design = s.split(';')[0];
    if (key === 'b') out.part = s;
    p += 2 + n;
  }
}

const REVERSE = Uint8Array.from({ length: 256 }, (_, b) => {
  let r = 0;
  for (let i = 0; i < 8; i++) r |= ((b >> i) & 1) << (7 - i);
  return r;
});

/** Loads an Artix-7 .bit file into the FPGA over JTAG. */
export async function programXc7(io, bit, board = '', progress = () => {}) {
  const { part, data } = parseBit(bit);
  const m = new Mpsse(io);
  await m.init(2); // 10 MHz
  const [value, dir] = /digilent|nexys|basys|arty|cmod/i.test(board) ? JTAG_PINS.digilent : JTAG_PINS.plain;
  m.push(SET_LOW, value, dir);
  const j = new Jtag(m);
  j.reset();
  const id = await j.dr32();
  const found = IDCODES[id & 0x0fffffff];
  const want = part?.match(/^xc7a\d+t/)?.[0];
  if (!found) {
    throw new Error(id === 0 || id === 0xffffffff
      ? 'no JTAG device answered: is this an Artix-7 board (the Alchitry Au or the Nexys A7)?'
      : `the JTAG device 0x${id.toString(16).padStart(8, '0')} is not a known Artix-7`);
  }
  if (want && found !== want) throw new Error(`the board has an ${found}, but the bitstream is for an ${want}`);
  // Clear the configuration, then wait for INIT to complete.
  await j.ir(JPROGRAM);
  j.idle(10000);
  await m.flush();
  await sleep(20);
  let ok = false;
  for (let i = 0; i < 200 && !ok; i++) {
    ok = !!((await j.ir(CFG_IN, true)) & IR_INIT);
    if (!ok) await sleep(5);
  }
  if (!ok) throw new Error('the FPGA did not finish clearing its configuration');
  // The configuration logic takes each byte most significant bit first;
  // MPSSE shifts least significant first.
  const rev = new Uint8Array(data.length);
  for (let i = 0; i < data.length; i++) rev[i] = REVERSE[data[i]];
  const chunk = 0x40000;
  j.tms(0b001, 3); // RTI -> Shift-DR, CFG_IN loaded
  for (let o = 0; o < rev.length; o += chunk) {
    progress('Loading the FPGA', o / rev.length);
    const end = Math.min(o + chunk, rev.length);
    const body = end === rev.length ? end - 1 : end;
    for (let p = o; p < body; p += 0x10000) {
      const k = Math.min(0x10000, body - p);
      m.push(JTAG_OUT_BYTES, (k - 1) & 0xff, (k - 1) >> 8);
      m.pushData(rev.subarray(p, p + k));
    }
    if (end === rev.length) {
      const last = rev[rev.length - 1];
      m.push(JTAG_OUT_BITS, 6, last & 0x7f);
      j.tms(0b1, 1, last >> 7);
      j.tms(0b01, 2);
    }
    await m.flush();
  }
  // Start up.
  await j.ir(JSTART);
  j.idle(2000);
  j.reset();
  await m.flush();
  const status = await j.ir(BYPASS, true);
  return { device: found, done: !!(status & IR_DONE) };
}

/** Programs the board with a flow result (`bin`, `family`). */
export async function programBoard(io, result, board, bytes, progress) {
  if (result.family === 'Artix-7') return programXc7(io, bytes, board, progress);
  return programIce40(io, bytes, progress);
}
