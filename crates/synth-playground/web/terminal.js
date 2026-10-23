// A small serial terminal: the bytes a design sends over its UART, shown as
// a screen of text. It knows enough of VT100 for the examples: carriage
// return, line feed, backspace, cursor home (`ESC [ H`, which starts a new
// frame of a design that redraws in place), cursor position, and clearing
// the screen or a line. Other escape sequences are ignored.

export class Terminal {
  constructor(cols = 80, rows = 24) {
    this.cols = cols;
    this.rows = rows;
    this.frames = 0;
    this.reset();
  }

  reset() {
    this.lines = Array.from({ length: this.rows }, () => []);
    this.x = 0;
    this.y = 0;
    this.esc = null;
  }

  /** Writes bytes (numbers) to the screen. */
  write(bytes) {
    for (const b of bytes) this.byte(b);
  }

  byte(b) {
    if (this.esc !== null) {
      this.esc += String.fromCharCode(b);
      // A CSI sequence ends with a letter; a lone ESC with any byte.
      if (this.esc === '[' || (this.esc.startsWith('[') && !/[@-~]$/.test(this.esc))) return;
      this.control(this.esc);
      this.esc = null;
      return;
    }
    if (b === 0x1b) this.esc = '';
    else if (b === 0x0d) this.x = 0;
    else if (b === 0x0a) this.newline();
    else if (b === 0x08) this.x = Math.max(0, this.x - 1);
    else if (b >= 0x20 && b < 0x7f) this.put(String.fromCharCode(b));
  }

  control(seq) {
    const m = /^\[([\d;]*)([A-Za-z])$/.exec(seq);
    if (!m) return;
    const args = m[1].split(';').map((a) => parseInt(a, 10));
    switch (m[2]) {
      case 'H':
      case 'f': {
        const row = (args[0] || 1) - 1;
        const col = (args[1] || 1) - 1;
        if (row === 0 && col === 0) this.frames++;
        this.y = Math.min(row, this.rows - 1);
        this.x = Math.min(col, this.cols - 1);
        break;
      }
      case 'J':
        // 2 (and 3): the whole screen; none or 0: from the cursor on.
        if (args[0] === 2 || args[0] === 3) this.lines = this.lines.map(() => []);
        else if (!(args[0] >= 1)) {
          this.lines[this.y].length = Math.min(this.lines[this.y].length, this.x);
          for (let y = this.y + 1; y < this.rows; y++) this.lines[y] = [];
        }
        break;
      case 'K':
        this.lines[this.y].length = Math.min(this.lines[this.y].length, this.x);
        break;
      default:
    }
  }

  put(c) {
    if (this.x >= this.cols) {
      this.x = 0;
      this.newline();
    }
    const line = this.lines[this.y];
    while (line.length < this.x) line.push(' ');
    line[this.x++] = c;
  }

  newline() {
    if (this.y < this.rows - 1) this.y++;
    else {
      this.lines.shift();
      this.lines.push([]);
    }
  }

  /** The screen as text, without trailing blank lines. */
  text() {
    const lines = this.lines.map((l) => l.join('').trimEnd());
    while (lines.length && !lines[lines.length - 1]) lines.pop();
    return lines.join('\n');
  }
}
