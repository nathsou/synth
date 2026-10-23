// An FTDI FT2232H/FT232H over WebUSB, in MPSSE mode: the USB bridge of the
// playground's boards, which drives their configuration flash (iCE40) or
// JTAG port (Artix-7). The chip's first channel (interface A) is used: that
// is where every supported board wires the FPGA, the second being the UART.
//
// WebUSB needs a Chromium-based browser, and the operating system must let
// it claim the interface: on Linux a udev rule giving access to the device
// (the ftdi_sio driver is detached from that interface while programming),
// on Windows the WinUSB driver for interface 0, on macOS no other driver
// bound to it.

const FTDI_VID = 0x0403;
// FT2232H/D (0x6010), FT4232H (0x6011), FT232H (0x6014).
const FTDI_PIDS = [0x6010, 0x6011, 0x6014];

const SIO_RESET = 0x00;
const SIO_SET_LATENCY_TIMER = 0x09;
const SIO_SET_BITMODE = 0x0b;
const RESET_SIO = 0;
const PURGE_RX = 1;
const PURGE_TX = 2;
const BITMODE_RESET = 0x00;
const BITMODE_MPSSE = 0x02;

/** Whether this browser can program boards. */
export const supported = () => typeof navigator !== 'undefined' && !!navigator.usb;

/** Asks the user for a board and opens it. */
export async function requestFtdi() {
  if (!supported()) throw new Error('programming a board needs WebUSB: use Chrome, Edge or another Chromium-based browser');
  let dev;
  try {
    dev = await navigator.usb.requestDevice({ filters: FTDI_PIDS.map((productId) => ({ vendorId: FTDI_VID, productId })) });
  } catch (e) {
    if (e.name === 'NotFoundError') throw new Error('no board chosen');
    throw e;
  }
  return Ftdi.open(dev);
}

export class Ftdi {
  constructor(dev, iface, inEp, outEp, packet) {
    this.dev = dev;
    this.iface = iface;
    this.inEp = inEp;
    this.outEp = outEp;
    this.packet = packet;
    this.pending = [];
  }

  static async open(dev) {
    await dev.open();
    if (dev.configuration === null) await dev.selectConfiguration(1);
    const intf = dev.configuration.interfaces.find((i) => i.interfaceNumber === 0);
    if (!intf) throw new Error('the USB device has no interface A');
    const eps = intf.alternate.endpoints;
    const inEp = eps.find((e) => e.direction === 'in' && e.type === 'bulk');
    const outEp = eps.find((e) => e.direction === 'out' && e.type === 'bulk');
    try {
      await dev.claimInterface(0);
    } catch (e) {
      await dev.close().catch(() => {});
      throw new Error(`cannot claim the board's USB interface (${e.message}). ${claimHint()}`);
    }
    const f = new Ftdi(dev, 0, inEp.endpointNumber, outEp.endpointNumber, inEp.packetSize || 512);
    await f.control(SIO_RESET, RESET_SIO);
    await f.control(SIO_SET_LATENCY_TIMER, 2);
    await f.control(SIO_SET_BITMODE, BITMODE_RESET << 8);
    await f.control(SIO_SET_BITMODE, BITMODE_MPSSE << 8);
    await f.control(SIO_RESET, PURGE_RX);
    await f.control(SIO_RESET, PURGE_TX);
    return f;
  }

  get name() {
    return [this.dev.manufacturerName, this.dev.productName].filter(Boolean).join(' ') || 'FTDI device';
  }

  control(request, value) {
    // wIndex: the channel, 1 for A.
    return this.dev.controlTransferOut({ requestType: 'vendor', recipient: 'device', request, value, index: this.iface + 1 });
  }

  async write(bytes) {
    for (let i = 0; i < bytes.length; i += 1 << 16) {
      const r = await this.dev.transferOut(this.outEp, bytes.subarray(i, i + (1 << 16)));
      if (r.status !== 'ok') throw new Error(`USB write failed: ${r.status}`);
    }
  }

  /** Reads exactly `n` bytes. Each USB packet the chip sends starts with two
   *  modem status bytes, which are dropped. */
  async read(n) {
    const out = new Uint8Array(n);
    let got = 0;
    let idle = 0;
    while (got < n) {
      const want = Math.ceil((n - got) / (this.packet - 2)) * this.packet;
      const r = await this.dev.transferIn(this.inEp, Math.min(want, 1 << 16));
      if (r.status !== 'ok') throw new Error(`USB read failed: ${r.status}`);
      const d = new Uint8Array(r.data.buffer, r.data.byteOffset, r.data.byteLength);
      let any = false;
      for (let p = 0; p < d.length; p += this.packet) {
        const chunk = d.subarray(p + 2, Math.min(p + this.packet, d.length));
        const take = Math.min(chunk.length, n - got);
        out.set(chunk.subarray(0, take), got);
        got += take;
        any ||= take > 0;
      }
      if (!any && ++idle > 200) throw new Error('the board stopped answering');
    }
    return out;
  }

  async close() {
    try {
      await this.control(SIO_SET_BITMODE, BITMODE_RESET << 8);
      await this.dev.releaseInterface(this.iface);
    } finally {
      await this.dev.close().catch(() => {});
    }
  }
}

function claimHint() {
  const ua = navigator.userAgent;
  if (/Windows/.test(ua)) return 'On Windows, install the WinUSB driver for interface 0 of the board (for example with Zadig).';
  if (/Mac OS/.test(ua)) return 'On macOS, no other program or driver may hold the board.';
  return 'On Linux, give your user access to the board with a udev rule, for example '
    + '`SUBSYSTEM=="usb", ATTRS{idVendor}=="0403", MODE="0666"` in /etc/udev/rules.d/53-ftdi.rules, '
    + 'then unplug and plug the board again; close any program using it.';
}
