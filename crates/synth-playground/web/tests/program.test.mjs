// The board programmers (program.js) against an emulated FTDI bridge: an
// MPSSE engine wired to a SPI flash and an iCE40's reset (the Go Board, the
// Alchitry Cu), or to the JTAG port of an Artix-7 (the Alchitry Au, the
// Nexys A7).

import test from 'node:test';
import assert from 'node:assert/strict';
import { programIce40, programXc7, parseBit } from '../program.js';

/** An MPSSE engine: commands in, read bytes out. */
class Bridge {
  constructor() {
    this.low = 0;
    this.out = [];
  }

  async write(bytes) {
    let p = 0;
    const len16 = () => {
      const n = bytes[p] | (bytes[p + 1] << 8);
      p += 2;
      return n + 1;
    };
    while (p < bytes.length) {
      const c = bytes[p++];
      switch (c) {
        case 0x8a: case 0x97: case 0x8d: case 0x85: case 0x87: break;
        case 0x86: p += 2; break;
        case 0x80: this.setLow(bytes[p]); p += 2; break;
        case 0x81: this.out.push(this.readLow()); break;
        case 0x11: case 0x31: {
          const n = len16();
          for (let i = 0; i < n; i++) {
            const r = this.spi(bytes[p++]);
            if (c === 0x31) this.out.push(r);
          }
          break;
        }
        case 0x19: case 0x39: {
          const n = len16();
          for (let i = 0; i < n; i++) {
            const b = bytes[p++];
            let r = 0;
            for (let k = 0; k < 8; k++) r |= this.clock(this.tmsPin, (b >> k) & 1) << k;
            if (c === 0x39) this.out.push(r);
          }
          break;
        }
        case 0x1b: case 0x3b: {
          const n = bytes[p++] + 1;
          const b = bytes[p++];
          let r = 0;
          for (let k = 0; k < n; k++) r |= this.clock(this.tmsPin, (b >> k) & 1) << (8 - n + k);
          if (c === 0x3b) this.out.push(r);
          break;
        }
        case 0x4b: case 0x6b: {
          const n = bytes[p++] + 1;
          const b = bytes[p++];
          const tdi = b >> 7;
          let r = 0;
          for (let k = 0; k < n; k++) {
            this.tmsPin = (b >> k) & 1;
            r |= this.clock(this.tmsPin, tdi) << (8 - n + k);
          }
          if (c === 0x6b) this.out.push(r);
          break;
        }
        case 0x8f: {
          const n = len16() * 8;
          for (let i = 0; i < n; i++) this.clock(this.tmsPin, 0);
          break;
        }
        case 0xaa: this.out.push(0xfa, 0xaa); break;
        default: throw new Error(`unexpected MPSSE command 0x${c.toString(16)}`);
      }
    }
  }

  async read(n) {
    assert.equal(this.out.length, n, 'the reads expected match the reads made');
    return Uint8Array.from(this.out.splice(0, n));
  }

  setLow(v) { this.low = v; }
  readLow() { return this.low; }
  spi() { throw new Error('no SPI device'); }
  clock() { throw new Error('no JTAG device'); }
}

/** A W25Q32 flash behind iceprog's wiring, and the FPGA it configures. */
class FlashBoard extends Bridge {
  constructor() {
    super();
    this.mem = new Uint8Array(1 << 22).fill(0x5a);
    this.cmd = null;
    this.wel = false;
    this.busy = 0;
    this.conflicts = 0;
    this.cdone = false;
  }

  setLow(v) {
    const cs = !!(v & 0x10);
    const creset = !!(v & 0x80);
    if (this.cmd === null && !cs) this.cmd = [];
    else if (this.cmd !== null && cs) {
      this.execute(this.cmd);
      this.cmd = null;
    }
    // Leaving reset, the FPGA loads from the flash.
    if (creset && !(this.low & 0x80)) this.cdone = this.mem[0] === 0x7e && this.mem[1] === 0xaa;
    if (!creset) this.cdone = false;
    this.low = v;
  }

  readLow() { return (this.low & ~0x40) | (this.cdone ? 0x40 : 0); }

  spi(b) {
    if (this.cmd === null) return 0xff;
    // The FPGA must be held in reset while the flash is used.
    if (this.low & 0x80) this.conflicts++;
    const i = this.cmd.length;
    this.cmd.push(b);
    const c = this.cmd[0];
    if (c === 0x9f && i >= 1) return [0xef, 0x40, 0x16][i - 1] ?? 0;
    if (c === 0x05 && i >= 1) return this.busy > 0 ? (this.busy--, 1) : 0;
    if (c === 0x03 && i >= 4) return this.mem[this.addr() + i - 4];
    return 0xff;
  }

  addr() { return (this.cmd[1] << 16) | (this.cmd[2] << 8) | this.cmd[3]; }

  execute(cmd) {
    if (cmd[0] === 0x06) this.wel = true;
    if (cmd[0] === 0xd8) {
      assert.ok(this.wel, 'erase after write enable');
      const a = this.addr() & ~0xffff;
      this.mem.fill(0xff, a, a + 0x10000);
      this.wel = false;
      this.busy = 3;
    }
    if (cmd[0] === 0x02) {
      assert.ok(this.wel, 'program after write enable');
      const a = this.addr();
      assert.ok(cmd.length - 4 <= 256 && (a & 0xff) + cmd.length - 4 <= 256, 'within one page');
      for (let i = 4; i < cmd.length; i++) this.mem[a + i - 4] &= cmd[i];
      this.wel = false;
      this.busy = 2;
    }
  }
}

/** The JTAG port of a 7-series FPGA. */
class JtagBoard extends Bridge {
  constructor(idcode, expected) {
    super();
    this.idcode = idcode;
    this.expected = expected;
    this.state = 'TLR';
    this.tmsPin = 1;
    this.ir = 0x09;
    this.irShift = 0;
    this.dr = 0;
    this.cfg = [];
    this.init = true;
    this.done = false;
    this.idleClocks = 0;
  }

  static NEXT = {
    TLR: ['RTI', 'TLR'], RTI: ['RTI', 'SelDR'], SelDR: ['CapDR', 'SelIR'], CapDR: ['ShDR', 'Ex1DR'],
    ShDR: ['ShDR', 'Ex1DR'], Ex1DR: ['PauseDR', 'UpdDR'], PauseDR: ['PauseDR', 'Ex2DR'], Ex2DR: ['ShDR', 'UpdDR'],
    UpdDR: ['RTI', 'SelDR'], SelIR: ['CapIR', 'TLR'], CapIR: ['ShIR', 'Ex1IR'], ShIR: ['ShIR', 'Ex1IR'],
    Ex1IR: ['PauseIR', 'UpdIR'], PauseIR: ['PauseIR', 'Ex2IR'], Ex2IR: ['ShIR', 'UpdIR'], UpdIR: ['RTI', 'SelDR'],
  };

  /** One TCK rising edge: returns TDO as it was before the edge. */
  clock(tms, tdi) {
    let tdo = 0;
    if (this.state === 'ShIR') {
      tdo = this.irShift & 1;
      this.irShift = (this.irShift >> 1) | (tdi << 5);
    } else if (this.state === 'ShDR') {
      if (this.ir === 0x05) this.cfg.push(tdi);
      else {
        tdo = this.dr & 1;
        this.dr = (this.dr >>> 1) | (tdi << 31);
      }
    } else if (this.state === 'RTI') {
      this.idleClocks++;
      if (this.ir === 0x0b && this.idleClocks >= 1000) this.init = true;
      if (this.ir === 0x0c && this.idleClocks >= 2000) this.done = this.configured();
    }
    this.state = JtagBoard.NEXT[this.state][tms];
    if (this.state === 'TLR') this.ir = 0x09;
    if (this.state === 'CapIR') this.irShift = 0b01 | (this.init ? 0x10 : 0) | (this.done ? 0x20 : 0);
    if (this.state === 'CapDR') this.dr = this.ir === 0x09 ? this.idcode : 0;
    if (this.state === 'UpdIR') {
      this.ir = this.irShift;
      this.idleClocks = 0;
      if (this.ir === 0x0b) {
        this.init = false;
        this.done = false;
        this.cfg = [];
      }
    }
    return tdo;
  }

  /** Whether CFG_IN received the configuration, each byte MSB first. */
  configured() {
    if (this.cfg.length !== this.expected.length * 8) return false;
    return this.expected.every((b, i) => {
      let v = 0;
      for (let k = 0; k < 8; k++) v = (v << 1) | this.cfg[i * 8 + k];
      return v === b;
    });
  }
}

/** A .bit file as arch-xc7 writes it. */
function bitFile(design, part, data) {
  const out = [0, 9, 0x0f, 0xf0, 0x0f, 0xf0, 0x0f, 0xf0, 0x0f, 0xf0, 0, 0, 1];
  for (const [tag, s] of [['a', design], ['b', part], ['c', '1970/01/01'], ['d', '00:00:00']]) {
    out.push(tag.charCodeAt(0), (s.length + 1) >> 8, (s.length + 1) & 0xff, ...Buffer.from(s), 0);
  }
  const n = data.length;
  out.push(0x65, n >>> 24, (n >> 16) & 0xff, (n >> 8) & 0xff, n & 0xff);
  const bit = new Uint8Array(out.length + n);
  bit.set(out);
  bit.set(data, out.length);
  return bit;
}

function random(n, seed) {
  let x = seed;
  return Uint8Array.from({ length: n }, () => ((x = (x * 1103515245 + 12345) >>> 0) >>> 16) & 0xff);
}

test('program: an iCE40 bitstream is written to the flash, and the FPGA starts', async () => {
  const image = random(70_000, 1);
  image.set([0x7e, 0xaa, 0x99, 0x7e]);
  const board = new FlashBoard();
  const stages = new Set();
  const r = await programIce40(board, image, (s) => stages.add(s));
  assert.deepEqual(board.mem.subarray(0, image.length), image);
  // The rest of the erased blocks is blank.
  assert.ok(board.mem.subarray(image.length, 0x20000).every((b) => b === 0xff));
  assert.equal(board.mem[0x20000], 0x5a);
  assert.equal(board.conflicts, 0, 'the FPGA was in reset while the flash was used');
  assert.deepEqual(r, { flashId: 'ef4016', done: true });
  assert.deepEqual([...stages], ['Erasing the flash', 'Writing the flash', 'Verifying the flash']);
});

test('program: a board without a flash is reported', async () => {
  const board = new FlashBoard();
  board.spi = () => 0xff;
  await assert.rejects(programIce40(board, random(100, 2)), /no flash answered/);
});

test('program: .bit files are parsed', () => {
  const data = random(1000, 3);
  const b = parseBit(bitFile('top;UserID=0XFFFFFFFF', 'xc7a35tftg256-1', data));
  assert.equal(b.design, 'top');
  assert.equal(b.part, 'xc7a35tftg256-1');
  assert.deepEqual(b.data, data);
  assert.throws(() => parseBit(bitFile('top', 'xc7a35t', data).subarray(0, 500)), /truncated/);
});

test('program: an Artix-7 is configured over JTAG', async () => {
  const data = random(300_000, 4);
  for (const [board, idcode] of [['Nexys A7', 0x13631093], ['Alchitry Au', 0x0362d093]]) {
    const part = idcode === 0x13631093 ? 'xc7a100tcsg324-1' : 'xc7a35tftg256-1';
    const b = new JtagBoard(idcode, data);
    const r = await programXc7(b, bitFile('top', part, data), board);
    assert.deepEqual(r, { device: part.slice(0, part.indexOf('t') + 1), done: true }, board);
    // Digilent's bridge enables its JTAG buffers with the upper pins.
    assert.equal(b.low, board === 'Nexys A7' ? 0xe8 : 0x08);
  }
});

test('program: a bitstream for another part is refused', async () => {
  const data = random(1000, 5);
  const b = new JtagBoard(0x0362d093, data);
  await assert.rejects(programXc7(b, bitFile('top', 'xc7a100tcsg324-1', data), 'Nexys A7'),
    /the board has an xc7a35t, but the bitstream is for an xc7a100t/);
  assert.equal(b.cfg.length, 0);
});
