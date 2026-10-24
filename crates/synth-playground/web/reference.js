// The Weft reference: a summary of the language (docs/WEFT.md is the
// specification), its builtins and its standard library, shown next to the
// editor. Sections are written in a small markdown: `### Title {#id}` starts
// an entry that Jump to and Ctrl+click on a builtin can open.

import { h, esc, icon, toast } from './util.js';
import { highlightHtml } from './highlight.js';

const SPEC = 'https://github.com/nathsou/synth/blob/main/docs/WEFT.md';

export const SECTIONS = [
  {
    id: 'basics', title: 'Basics', blurb: 'a first module',
    md: `
A design is a set of \`.weft\` files sharing one namespace: items can be used before they are defined and in any file. The top module is the one no other module instantiates; its ports are the pins, named in the constraints file.

\`\`\`weft
/// Blinks an LED at about 1.5 Hz with a 25 MHz clock.
module top(in clk: clock, out led: bool) {
    reg count: uint<24> = 0;
    count := count +% 1;
    led = count[23];
}
\`\`\`

\`in clk: clock\` is the only clock, so it is the domain of every register. \`count\` is a register reset to 0; \`:=\` sets its next value. \`led\` is driven combinationally with \`=\`. The sum \`count + 1\` would need 25 bits, so wrapping addition \`+%\` keeps it in 24.

### Comments and names {#lexical}

- \`// line\`, \`/* block */\` (not nested), and \`///\` doc comments, shown when hovering the item.
- Identifiers are ASCII: \`[A-Za-z_][A-Za-z0-9_]*\`. By convention \`snake_case\` for values, modules and functions, \`CamelCase\` for structs, enums and variants, \`UPPER_CASE\` for constants and generics.
- There is no shadowing: declaring a name that is already visible is an error. Names starting with \`_\` are exempt from the unused-name warnings.

### Literals {#literals}

| Literal | Example |
|---|---|
| Decimal | \`42\`, \`1_000_000\` |
| Hexadecimal, binary, octal | \`0x2A\`, \`0b10_1010\`, \`0o52\` |
| Bit pattern (in \`match\` only) | \`0b1??0\` |
| Boolean | \`true\`, \`false\` |
| String (generic arguments and file names) | \`"rom.hex"\` |

Integer literals have no type of their own: they take the type the context expects. Binary and hexadecimal literals also have a *natural width*, the number of digits times 1 or 4, used where a width is needed (\`concat(a, 0b00)\`).

### Formatting {#formatting}

The format button (Shift+Alt+F) indents by four spaces per bracket and aligns the names of ports written one per line. A module header longer than 100 characters is broken into one port, and if needed one generic parameter, per line.
`,
  },
  {
    id: 'items', title: 'Items', blurb: 'module · fn · struct',
    md: `
| Item | Form |
|---|---|
| Module | \`module name<generics>(ports) { body }\` |
| Extern module | \`extern module name<generics>(ports);\` |
| Function | \`fn name<generics>(params) -> Type { body }\` |
| Struct | \`struct Name { field: Type, … }\` |
| Enum | \`enum Name { A, B }\` or \`enum Name: uint<N> { A = 1, … }\` |
| Constant | \`const NAME: Type = expr;\` |
| Static assertion | \`assert(cond, "message");\` |

Doc comments (\`///\`) and attributes may precede any item or declaration.

### Attributes {#attributes}

| Attribute | On | Effect |
|---|---|---|
| \`#[keep]\` | \`reg\`, \`wire\`, \`inst\` | Not removed by optimization |
| \`#[async_reg]\` | \`reg\` | A synchronizer stage: kept, not merged into shift registers, placed close together |
| \`#[attr("name", value)]\` | declarations | Passed to the RTL as an attribute |
| \`#[attr("frame", "WxH")]\` | \`reg\` | The simulation shows the register as a W×H image |

Attributes never change behaviour.
`,
  },
  {
    id: 'types', title: 'Types', blurb: 'uint · sint · index',
    md: `
| Type | Values | Width |
|---|---|---|
| \`bool\` | \`false\`, \`true\` | 1 |
| \`uint<N>\` | 0 … 2^N − 1 | N |
| \`sint<N>\` | −2^(N−1) … 2^(N−1) − 1 | N |
| \`index<N>\` | 0 … N − 1 | max(1, clog2(N)) |
| \`[T; N]\` | N elements of T | N × width(T) |
| struct, enum | see below | |

\`bool\` is not an integer: conditions need \`bool\`, and there is no implicit conversion between the two. \`index<N>\` is the type of a valid index into anything with N elements, and wrapping arithmetic on it wraps modulo N, which makes counters of any period.

### Integer ranges {#ranges}

Every integer value has a statically known range, which is its type: \`x + 1\` with \`x: uint<8>\` is \`1..=256\`, stored as a \`uint<9>\`. Diagnostics write such ranges \`int<lo..=hi>\`; the form cannot be written in source. Inside a branch guarded by a comparison with a constant, a name's range is narrowed:

\`\`\`weft
let pixel = if addr < 4800 { fb[addr] } else { 0 };
if level < 255 { level := level + 1; }     // fits uint<8> here
\`\`\`

### Structs {#structs}

\`\`\`weft
struct Pixel { r: uint<5>, g: uint<6>, b: uint<5> }
let p = Pixel { r: 0, g: 63, b: 0 };       // every field is given
\`\`\`

The first field holds the most significant bits: \`p.bits() == concat(p.r, p.g, p.b)\`. Structs compare with \`==\` and \`!=\`.

### Enums {#enums}

\`\`\`weft
enum State { Idle, Busy, Done }                 // encoding chosen by the compiler
enum Opcode: uint<7> { Load = 0x03, Store = 0x23 }
reg state: State = .Idle;                       // \`.Idle\` where the type is known
\`\`\`

Enums support \`==\`, \`!=\` and \`match\`. Only enums with an explicit encoding have \`.bits()\`; there is no conversion from integers to enums.

### Arrays {#arrays}

\`[a, b, c]\` or \`[v; N]\`. Element 0 holds the least significant bits. Arrays support \`==\`, \`!=\`, constant and runtime indexing, and assignment of single elements (\`regs[rd] := wdata;\`).

### Compile-time types {#compile-time-types}

\`int\` (arbitrary precision: generics, constants, loop variables), \`bool\`, \`string\` (generic arguments, file names) and \`type\` (type generics) exist only at compile time.
`,
  },
  {
    id: 'operators', title: 'Operators', blurb: 'precedence · arithmetic',
    md: `
From highest to lowest precedence:

| Operators | Notes |
|---|---|
| \`.field\` \`.method()\` \`[i]\` \`[hi:lo]\` \`[i +: W]\` calls | |
| \`**\` | compile-time \`int\` only, right associative |
| unary \`-\` \`!\` \`~\` | |
| \`*\` \`*%\` \`/\` \`%\` | |
| \`+\` \`-\` \`+%\` \`-%\` | |
| \`<<\` \`>>\` | |
| \`&\`, then \`^\`, then \`|\` | bind tighter than comparisons |
| \`==\` \`!=\` \`<\` \`<=\` \`>\` \`>=\` | do not chain |
| \`&&\`, then \`||\` | |

Mixing \`&&\` and \`||\`, or two of \`&\`, \`|\`, \`^\`, needs parentheses. There is no \`?:\`: use an \`if\` expression. Inside \`<…>\`, comparisons and shifts are parenthesized: \`uint<(W >> 1)>\`.

### Exact arithmetic {#exact}

\`+\`, \`-\`, \`*\`, unary \`-\`, \`/ c\` and \`% c\` compute the mathematical result, whatever the operands' signedness, and their type is its exact range. Division and remainder need a positive constant divisor.

| With \`x: uint<8>\`, \`s: sint<8>\` | Range | Stored as |
|---|---|---|
| \`x + 1\` | \`1..=256\` | \`uint<9>\` |
| \`x - 1\` | \`-1..=254\` | \`sint<9>\` |
| \`x + s\` | \`-128..=382\` | \`sint<10>\` |
| \`x / 10\` | \`0..=25\` | \`uint<5>\` |

### Wrapping arithmetic {#wrapping}

\`a +% b\`, \`a -% b\`, \`a *% b\` wrap to the declared type of an operand (both, if both have one, must be the same); the other operand must fit it. On \`index<N>\`, \`+%\` and \`-%\` wrap modulo N.

\`\`\`weft
reg phase: index<217> = 0;
phase := phase +% 1;            // 0, 1, …, 216, 0, …
\`\`\`

### Bitwise operators and shifts {#bitwise}

- \`&\`, \`|\`, \`^\`: two \`bool\`s, or two unsigned values of the same width (a constant takes the other's width). Use \`.bits()\` for \`sint\`.
- \`~a\` on unsigned integers, \`!a\` on \`bool\`.
- \`a << n\`, \`a >> n\` keep \`a\`'s width (bits shifted out are lost); \`>>\` fills with the sign bit for signed values. \`n\` is a constant or an unsigned value. To scale exactly, use \`*\` or \`concat(x, 0b00)\`.
`,
  },
  {
    id: 'conversions', title: 'Conversions', blurb: 'bits · slices · methods',
    md: `
An integer converts implicitly to an integer type that holds its whole range, at assignments, connections, arguments, results and branches. Nothing else converts implicitly, and nothing converts with loss.

| Form | Meaning |
|---|---|
| \`T(x)\` | Lossless conversion to integer type T: an error if \`x\` may not fit |
| \`x.trunc(N)\` | The low N bits: \`uint<N>\`, or \`sint<N>\` for signed values |
| \`x.bits()\` | The bits as \`uint<width>\` (not for enums without an encoding) |
| \`x.as_sint()\` | \`uint<N>\` reinterpreted as \`sint<N>\` |
| \`from_bits(x)\` | A value of the expected type rebuilt from its bits |

### Indexing and slicing {#indexing}

| Form | Result |
|---|---|
| \`x[i]\` | bit i of an integer, as \`bool\` |
| \`a[i]\` | element i of an array or memory |
| \`x[hi:lo]\` | \`uint<hi-lo+1>\`, constant bounds, written high to low |
| \`x[i +: W]\` | \`uint<W>\`: bits i … i+W−1 |

Every index is checked at compile time: a runtime index's range must lie within the valid positions, so \`fb[addr]\` needs \`addr: index<4800>\` (or a narrowing \`if\`) for a 4800-word memory.

### Methods {#methods}

| Method | Result |
|---|---|
| \`x.any()\`, \`x.all()\` | \`bool\`: some bit set, all bits set |
| \`x.parity()\` | \`bool\`: XOR of all bits |
| \`x.reverse()\` | bit-reversed, same type |
| \`x.replicate(k)\` | \`uint<k × width>\` |
| \`x.rotate_left(n)\`, \`x.rotate_right(n)\` | same type |
| \`x.width\`, \`T.width\` | the width, as an \`int\` |
`,
  },
  {
    id: 'control', title: 'Conditionals', blurb: 'if · match · undef',
    md: `
\`if c { a } else { b }\` is an expression when both branches are given; \`else if\` chains. Integer branches join into the union of their ranges. A branch may start with \`let\`s.

\`\`\`weft
state := if ok { .Done } else { .Idle };
\`\`\`

### match {#match}

\`\`\`weft
tx = match state {
    .Idle | .Stop => true,
    .Start => false,
    .Data => shift[0],
};
\`\`\`

| Pattern | Matches |
|---|---|
| \`5\`, \`-1\`, \`0x2A\` | that value |
| \`0..=9\` | an inclusive range |
| \`0b1??0\` | a bit pattern of the scrutinee's width |
| \`.Idle\`, \`State.Idle\` | an enum variant |
| \`true\`, \`false\` | a \`bool\` |
| \`p | q\` | either pattern |
| \`_\` | anything |

A \`match\` must be exhaustive, and an arm that can never match is an error. As an expression, arms are separated by commas; as a statement, each arm is a statement or a block, without commas.

### undef {#undef}

\`undef\` is an explicit don't-care of the expected type, which the optimizer may choose freely: \`data = match sel { 0 => a, 1 => b, _ => undef };\`. It is not allowed in reset values, initial contents, constants or generic arguments.
`,
  },
  {
    id: 'modules', title: 'Modules', blurb: 'ports · wires · statements',
    md: `
### Ports {#ports}

\`\`\`text
in    name: Type [@domain] [= default],
out   name: Type [@domain],
inout name: Type,
domain name,
default domain name,
\`\`\`

\`in\` ports are read-only; a constant default lets instances leave them unconnected. \`out\` ports are driven like wires and can be read. \`inout\` ports exist only on the top module and extern modules. \`in clk: clock\` is a clock input, which also defines a domain \`clk\`. A trailing comma is allowed.

### Declarations {#declarations}

| Declaration | Meaning |
|---|---|
| \`const NAME[: T] = expr;\` | a compile-time constant |
| \`let name[: T] [@d] = expr;\` | a named combinational value |
| \`wire name: T [@d];\` | a combinational signal driven by \`=\` |
| \`reg name: T [@d] = expr;\` | a register with a reset value |
| \`reg name: T [@d] init expr;\` | a register with only a power-on value |
| \`mem name: [T; N] [@d] [init expr];\` | a memory |
| \`inst name = module<args>(conns) [@d];\` | an instance |
| \`[default] domain name { … }\` | a clock domain |

Module-level declarations are visible in the whole body, before and after them.

### Statements {#statements}

| Statement | Meaning |
|---|---|
| \`target = expr;\` | drive a wire, \`out\` port or instance input |
| \`target := expr;\` | set a register's next value, or write a memory |
| \`if c { … } else { … }\` | conditional statements |
| \`match x { p => stmt … }\` | conditional statements, without commas |
| \`for i in a..b { … }\` | unrolled at compile time (\`..=\` is inclusive) |
| \`assert(cond, "message");\` | compile-time assertion |

Targets may have fields, indices and slices: \`pix.r\`, \`regs[i]\`, \`word[7:0]\`, \`inst.port\`.

### Signals are not variables {#signals}

A body describes hardware: reading a wire gives its value in this cycle wherever the read is, and reading a register gives its current state. When several assignments to a target apply, the last one wins, which is how defaults are written:

\`\`\`weft
ready = false;                      // default
if state == .Wait && valid {
    ready = true;
}
\`\`\`

Every bit of every wire, \`out\` port and instance input must be assigned on every path (the error names the path). Registers keep their value on paths without \`:=\`. Combinational cycles are errors. For sequential computations, use a function with \`var\`s.
`,
  },
  {
    id: 'registers', title: 'Registers & memories', blurb: 'reg · mem',
    md: `
\`\`\`weft
reg state: State = .Idle;              // reset value, also the power-on value
reg shift: uint<8> init 0;             // power-on value only: not reset
reg acc: sint<16> @dsp = 0;            // in domain \`dsp\`
if adc_valid { sample := adc_data; }   // conditions become clock enables
\`\`\`

Whether reset is synchronous, asynchronous or absent is a property of the domain. While it is asserted, registers with a reset value take it; registers with \`init\` are not affected.

### Memories {#memories}

\`\`\`weft
mem ram: [uint<32>; 1024] init hex_file("firmware.hex");
mem fb: [uint<4>; 4800];                   // starts at 0

if we { ram[waddr] := wdata; }             // a write port
reg q: uint<32> init 0;
q := ram[raddr];                           // registered read: block RAM
\`\`\`

A read gives the contents before this cycle's writes (read-first). A read used directly in logic is combinational and needs LUT RAM or flip-flops. Writing part of a word (\`ram[a][7:0] := …\`) uses per-bit write enables. Memories have no reset. A memory read in another clock domain goes through \`unsafe_cross\`, as a registered read.
`,
  },
  {
    id: 'domains', title: 'Clock domains', blurb: 'domain · @ · crossings',
    md: `
A domain is a clock edge and an optional reset. Every value has a timing tag: static (constants), a clock, or async. A value may reach a register, memory or port of a clock only if it is static or of that clock; the compiler names the source and the path otherwise.

\`\`\`weft
domain sys { clock: clk, reset: rst }               // synchronous reset
domain sys { clock: clk, async_reset: rst }         // asynchronous reset
domain spi { clock: sck, edge: falling }            // falling edge, no reset
\`\`\`

A module with no clock ports and no domain parameters has one implicit domain parameter: it runs in whatever domain its parent gives it, which is how most modules are written. \`default.clock\` and \`default.reset\` refer to it.

### The default domain {#default-domain}

Declarations and ports without \`@\` belong to the default domain: the implicit one, the only one, or the one declared \`default\`. In a module without one, ports, registers and memories need \`@d\`. \`@async\` marks a port that may carry any tag. The top module's non-clock inputs are \`@async\` unless annotated.

### Crossing domains {#crossings}

| Crossing | Use |
|---|---|
| one bit, level | \`sync(x)\` |
| one bit, pulse | \`rising(sync(x))\` |
| a reset from a button or another domain | \`reset_sync(x)\` |
| quasi-static configuration, dual-clock memory reads | \`unsafe_cross(x)\` |

\`sync\` takes only \`bool\`: synchronizing the bits of a bus separately is the bug it prevents.

### A PLL clock {#pll}

\`\`\`weft
module top<CLK_HZ: int = 12_000_000>(in clk: clock, out led: bool) {
    inst pll = synth_pll<IN_HZ: CLK_HZ, OUT_HZ: 40_000_000>(clk_in: clk);
    domain raw { clock: pll.clk_out }
    let rst = reset_sync(!pll.locked) @raw;
    default domain sys { clock: pll.clk_out, reset: rst }

    reg count: uint<26> = 0;
    count := count +% 1;
    led = count[25];
}
\`\`\`

The playground sets \`CLK_HZ\` to the board's clock frequency.
`,
  },
  {
    id: 'instances', title: 'Instances', blurb: 'inst · extern',
    md: `
\`\`\`weft
inst tx = uart_tx(tick: baud.tick, byte: data);
inst q = fifo<T: uint<8>, DEPTH: 64>(push: p, data: d, pop: r);
inst u = uart_tx(tick: t, start: s, byte: b) @sys;
tx.start = go;                        // inputs can also be driven by assignments
led = tx.busy;                        // outputs are read as \`inst.port\`
\`\`\`

Connections and generic arguments are by name (a single generic may be given positionally: \`divider<25_000_000>()\`). Every input without a default must be driven. A child's domain parameters are connected like ports, or get the parent's default domain, or the one after \`@\`.

### Module calls {#module-calls}

A module with exactly one output can be instantiated in a \`let\`, as a call that gives its output. A module with one input may take it positionally. Module calls are allowed only in module-level \`let\`s, never inside a runtime \`if\` or \`match\`.

\`\`\`weft
let go = rising(sync(btn));
let por = power_on_reset<CYCLES: 16>() @clk;
\`\`\`

### Instances in loops {#loops}

Declarations in a \`for\` body exist once per iteration; results leave through module-level signals:

\`\`\`weft
wire out: [uint<8>; 4];
for i in 0..4 {
    inst lane = scrambler(data: lanes[i]);
    out[i] = lane.q;
}
\`\`\`

### Extern modules {#extern}

\`\`\`weft
extern module servant<memfile: string, memsize: int>(
    domain { clock: wb_clk, reset: wb_rst },
    out q: bool,
);
\`\`\`

An extern module declares a device primitive or a Verilog module given with the design (a \`.v\` file in the playground). Types must match the Verilog widths; generics become Verilog parameters; \`domain { clock: c, reset: r }\` binds the implicit domain to ports.
`,
  },
  {
    id: 'functions', title: 'Functions & generics', blurb: 'fn · generics · const',
    md: `
\`\`\`weft
fn count_ones<N: int>(x: uint<N>) -> uint<bits_for(N)> {
    var n: uint<bits_for(N)> = 0;
    for i in 0..N {
        if x[i] { n = n +% 1; }
    }
    n
}
\`\`\`

Functions are pure combinational logic, inlined at every call: statements, then a final expression. \`var\` declares a mutable local, read in program order. Generics such as \`N\` are inferred from argument types. Recursion is allowed (depth up to 256). A call with constant arguments is evaluated at compile time, which computes tables (\`mem rom: [uint<16>; 256] init squares();\`).

### Generic parameters {#generics}

\`\`\`weft
module fifo<T: type, DEPTH: int = 16>(in push: bool, in data: T, out head: T) { … }
\`\`\`

Generics are \`int\`, \`bool\`, \`string\` or \`type\`, with optional defaults; arguments are compile-time constants given by name. Generic items are checked for each set of arguments they are used with; items whose generics all have defaults are also checked on their own.

### Constant expressions {#constants}

Literals, \`const\`s, generics, loop variables, \`.width\`, \`**\` and the builtins \`clog2\`, \`bits_for\`, \`min\` and \`max\`.

### Compile-time if and for {#generate}

An \`if\` on a constant elaborates only the taken branch, which may declare signals (Verilog's \`generate if\`). \`for i in a..b\` unrolls, with \`i\` an \`int\` in each iteration.

\`\`\`weft
if WITH_PARITY {
    reg p: bool = false;
    p := data.parity();
    parity_out = p;
} else {
    parity_out = false;
}
\`\`\`
`,
  },
  {
    id: 'builtins', title: 'Builtins', blurb: 'concat · clog2 · min',
    md: `
These names are predeclared and cannot be redefined.

### concat {#concat}

\`\`\`weft
fn concat(a, b, …) -> uint<sum of widths>
\`\`\`

Joins \`bool\`, \`uint\` and \`sint\` values, the first argument most significant. Every argument needs a width: binary and hexadecimal literals have theirs, other constants need a conversion (\`uint<3>(5)\`).

\`\`\`weft
led = concat(beat[24], 0b00, !q);       // uint<4>
let word = concat(hi, lo);              // uint<16> from two uint<8>
\`\`\`

### from_bits {#from_bits}

\`\`\`weft
fn from_bits(x: uint<N>) -> T
\`\`\`

Rebuilds a value of the expected type \`T\` from a \`uint\` of exactly its width (or a constant that fits it). For \`bool\`, integers except \`index\`, and structs and arrays of these; not for enums.

\`\`\`weft
let p: Pixel = from_bits(word);
\`\`\`

### unsafe_cross {#unsafe_cross}

\`\`\`weft
fn unsafe_cross(x: T) -> T
\`\`\`

Returns \`x\` with a static timing tag, accepted by every clock domain. The expression inside is not tag-checked: for values that are stable when sampled, and dual-clock memory reads. Searching for \`unsafe_cross\` finds every unchecked crossing.

\`\`\`weft
px_color := unsafe_cross(fb[px_addr]);     // dual-clock block RAM
\`\`\`

### hex_file, bin_file {#hex_file}

\`\`\`weft
hex_file(path: string)
bin_file(path: string)
\`\`\`

Memory contents from a file in \`$readmemh\` (\`hex_file\`) or \`$readmemb\` (\`bin_file\`) format, relative to the source file. Only as a memory's \`init\`: \`mem rom: [uint<8>; 256] init hex_file("rom.hex");\`.

### clog2 {#clog2}

\`\`\`weft
fn clog2(n: int) -> int
\`\`\`

The smallest k with 2^k ≥ n: \`clog2(1) == 0\`, \`clog2(256) == 8\`, \`clog2(257) == 9\`.

### bits_for {#bits_for}

\`\`\`weft
fn bits_for(v: int) -> int
\`\`\`

The width of the \`uint\` needed to hold v, at least 1: \`bits_for(255) == 8\`, \`bits_for(256) == 9\`.

\`\`\`weft
reg count: uint<bits_for(CYCLES)> init 0;
\`\`\`

### min, max {#min}

\`\`\`weft
fn min(a: int, b: int) -> int
fn max(a: int, b: int) -> int
\`\`\`

The smaller or larger of two compile-time integers.

\`\`\`weft
module top<CLK_HZ: int = 12_000_000, SYS_HZ: int = min(CLK_HZ, 25_000_000)>(…)
\`\`\`

### assert {#assert}

\`\`\`weft
assert(cond: bool, message: string);
\`\`\`

Fails the compilation with \`message\` unless the compile-time condition holds. A statement, in a module or function body or at the top level.

\`\`\`weft
assert(DEPTH >= 2 && DEPTH <= 4096, "fifo depth must be in 2..=4096");
\`\`\`
`,
  },
  {
    id: 'std', title: 'Standard library', blurb: 'sync · rising · divider',
    md: `
Written in Weft and always available. The modules with one output can be called in a \`let\`.

### sync {#sync}

\`\`\`weft
module sync<STAGES: int = 2>(in x: bool @async, out y: bool)
\`\`\`

Two-flip-flop synchronizer for a single asynchronous bit. The output follows the input after STAGES or STAGES + 1 clock cycles.

\`\`\`weft
let rx = sync(rx_pin);
\`\`\`

### reset_sync {#reset_sync}

\`\`\`weft
module reset_sync<STAGES: int = 2>(in x: bool @async, out rst: bool)
\`\`\`

A reset that asserts as soon as \`x\` is high and releases synchronously, STAGES cycles after \`x\` falls.

\`\`\`weft
let rst = reset_sync(btn) @clk;
default domain sys { clock: clk, async_reset: rst }
\`\`\`

### power_on_reset {#power_on_reset}

\`\`\`weft
module power_on_reset<CYCLES: int = 16>(out rst: bool)
\`\`\`

High for the first CYCLES cycles after configuration.

\`\`\`weft
let por = power_on_reset<CYCLES: 16>() @clk;
default domain sys { clock: clk, reset: por }
\`\`\`

### rising, falling {#rising}

\`\`\`weft
module rising(in x: bool, out y: bool)
module falling(in x: bool, out y: bool)
\`\`\`

A one-cycle pulse when \`x\` goes from low to high (\`rising\`) or from high to low (\`falling\`). The input must be in the module's domain: \`rising(sync(btn))\` for a button.

### divider {#divider}

\`\`\`weft
module divider<N: int>(out tick: bool)
\`\`\`

Pulses \`tick\` once every N cycles.

\`\`\`weft
inst baud = divider<N: 217>();         // 25 MHz / 115200
if baud.tick { … }
\`\`\`

### synth_pll {#synth_pll}

\`\`\`weft
extern module synth_pll<IN_HZ: int, OUT_HZ: int>(
    in  clk_in: clock,
    out clk_out: clock,
    out locked: bool @async,
);
\`\`\`

A clock of OUT_HZ made from \`clk_in\` (IN_HZ) by the device's PLL (\`SB_PLL40_CORE\` on iCE40, \`PLLE2_ADV\` on Artix-7), or \`clk_in\` itself when the two are equal. The output frequency must be one the PLL makes to within 0.2%. \`locked\` rises once the clock is stable: hold the design in reset until then (see [A PLL clock](#ref-pll)).

### count_ones {#count_ones}

\`\`\`weft
fn count_ones<N: int>(x: uint<N>) -> uint<bits_for(N)>
\`\`\`

The number of set bits.

### onehot_to_index {#onehot_to_index}

\`\`\`weft
fn onehot_to_index<N: int>(x: uint<N>) -> index<N>
\`\`\`

The position of the set bit of a one-hot value (the highest one if several are set, 0 if none).

### priority {#priority}

\`\`\`weft
fn priority<N: int>(x: uint<N>) -> index<N>
\`\`\`

The position of the lowest set bit (0 if none).
`,
  },
  {
    id: 'checks', title: 'Checks', blurb: 'errors · warnings',
    md: `
**Errors** stop the compilation: type errors and implicit narrowing; out-of-range constants and indices; incomplete assignments, and \`=\` on a register or \`:=\` on a wire; combinational cycles; non-exhaustive or unreachable \`match\` arms; domain mixing and unsynchronized asynchronous values; missing connections; shadowing; failed \`assert\`s. Each comes with a suggestion: \`+%\` or \`.trunc()\` for widths, \`sync()\` for asynchronous inputs, the path an assignment misses, the variants a \`match\` misses.

**Warnings**: comparisons always true or false given the ranges; dead assignments and \`r := r;\`; \`_\` arms that hide enum variants; unused registers, wires, \`let\`s, memories and inputs; registers never assigned; memories never written and not initialized; combinational reads of large memories, which cannot be block RAM.

The full specification, with the reasons behind the rules, is [docs/WEFT.md](${SPEC}).
`,
  },
];

/** The id of an entry's anchor in the page. */
const anchor = (id) => `ref-${id}`;

const slug = (s) => s.toLowerCase().replace(/<[^>]*>|`/g, '').replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '');

/** Every section and `###` entry: [{id, title, section}]. */
export const ENTRIES = SECTIONS.flatMap((s) => [
  { id: s.id, title: s.title, section: s.title, blurb: s.blurb },
  ...[...s.md.matchAll(/^### (.+?)(?: \{#([\w-]+)\})?$/gm)].map((m) => ({
    id: m[2] || slug(m[1]), title: m[1].replace(/`/g, ''), section: s.title,
  })),
]);

/** Names that open an entry other than their own (`max` is with `min`). */
const ALIASES = { max: 'min', bin_file: 'hex_file', falling: 'rising' };

/** The entry documenting a builtin or standard library name, if any. */
export function entryFor(name) {
  const id = ALIASES[name] || name;
  return ['builtins', 'std'].some((s) => SECTIONS.find((x) => x.id === s).md.includes(`{#${id}}`)) ? id : null;
}

function inline(s) {
  return esc(s)
    .replace(/`([^`]+)`/g, '<code>$1</code>')
    .replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>')
    .replace(/\[([^\]]+)\]\(([^)]+)\)/g, (_, t, href) => href.startsWith('#')
      ? `<a href="${href}" data-ref="${href.slice(1 + 'ref-'.length)}">${t}</a>`
      : `<a href="${href}" target="_blank" rel="noopener">${t}</a>`);
}

/** The cells of a table row; `|` inside backquotes is text. */
function cells(line) {
  const out = [];
  let cur = '', code = false;
  for (const c of line.trim().replace(/^\||\|$/g, '')) {
    if (c === '`') code = !code;
    if (c === '|' && !code) {
      out.push(cur.trim());
      cur = '';
    } else cur += c;
  }
  out.push(cur.trim());
  return out;
}

/** Renders the reference's markdown: `###` entries, paragraphs, lists,
 *  tables and code blocks. */
export function renderDoc(md) {
  const lines = md.trim().split('\n');
  let html = '';
  let i = 0;
  while (i < lines.length) {
    const l = lines[i];
    if (!l.trim()) {
      i++;
    } else if (l.startsWith('```')) {
      const lang = l.slice(3).trim();
      const code = [];
      for (i++; i < lines.length && !lines[i].startsWith('```'); i++) code.push(lines[i]);
      i++;
      const text = code.join('\n');
      html += `<div class="ref-code"><pre>${lang === 'weft' ? highlightHtml(text, 'weft') : esc(text)}</pre><button class="icon-btn sm ref-copy" title="Copy" aria-label="Copy"></button></div>`;
    } else if (l.startsWith('### ')) {
      const m = /^### (.+?)(?: \{#([\w-]+)\})?$/.exec(l);
      html += `<h3 id="${anchor(m[2] || slug(m[1]))}">${inline(m[1])}</h3>`;
      i++;
    } else if (l.startsWith('|')) {
      const rows = [];
      for (; i < lines.length && lines[i].startsWith('|'); i++) rows.push(cells(lines[i]));
      const [head, , ...body] = rows;
      html += `<div class="ref-table"><table><thead><tr>${head.map((c) => `<th>${inline(c)}</th>`).join('')}</tr></thead><tbody>${body.map((r) => `<tr>${r.map((c) => `<td>${inline(c)}</td>`).join('')}</tr>`).join('')}</tbody></table></div>`;
    } else if (l.startsWith('- ')) {
      const items = [];
      for (; i < lines.length && lines[i].startsWith('- '); i++) items.push(lines[i].slice(2));
      html += `<ul>${items.map((x) => `<li>${inline(x)}</li>`).join('')}</ul>`;
    } else {
      const para = [];
      for (; i < lines.length && lines[i].trim() && !/^(```|### |\||- )/.test(lines[i]); i++) para.push(lines[i]);
      html += `<p>${inline(para.join(' '))}</p>`;
    }
  }
  return html;
}

/** The reference view: the sections, a filter, and anchors to scroll to. */
export class ReferenceView {
  constructor(el, app) {
    this.el = el;
    this.app = app;
    this.current = SECTIONS[0].id;
    this.filter = h('input', {
      type: 'search', placeholder: 'Filter the reference', 'aria-label': 'Filter the reference', spellcheck: false,
      oninput: () => {
        this.applyFilter();
        this.el.scrollTop = 0;
      },
    });
    this.body = h('div', { class: 'ref-body' });
    this.none = h('p', { class: 'ref-none muted', hidden: true }, 'Nothing matches.');
    el.append(
      h('div', { class: 'ref-bar' },
        h('label', { class: 'ref-search' }, icon('search'), this.filter),
        h('a', { class: 'ref-spec', href: SPEC, target: '_blank', rel: 'noopener' }, 'Full specification')),
      this.body, this.none);
    for (const s of SECTIONS) {
      this.body.append(h('section', { id: anchor(s.id), 'data-section': s.id },
        h('h2', {}, s.title), h('div', { html: renderDoc(s.md) })));
    }
    for (const b of this.body.querySelectorAll('.ref-copy')) {
      b.append(icon('copy'));
      b.addEventListener('click', () => {
        navigator.clipboard?.writeText(b.previousElementSibling.textContent)
          .then(() => toast('Copied'), () => toast('Could not copy'));
      });
    }
    this.body.addEventListener('click', (e) => {
      const a = e.target.closest('a[data-ref]');
      if (a) {
        e.preventDefault();
        this.show(a.dataset.ref);
      }
    });
    el.addEventListener('scroll', () => this.spy(), { passive: true });
  }

  /** Scrolls to a section or entry and flashes it. */
  show(id) {
    const target = this.el.querySelector(`#${anchor(id)}`);
    if (!target) return;
    if (this.filter.value) {
      this.filter.value = '';
      this.applyFilter();
    }
    const top = target.getBoundingClientRect().top - this.el.getBoundingClientRect().top + this.el.scrollTop;
    this.el.scrollTop = Math.max(0, top - this.el.firstChild.offsetHeight - 8);
    target.classList.remove('flash');
    void target.offsetWidth;
    target.classList.add('flash');
    this.spy();
  }

  /** Marks the section at the top of the view in the rail. */
  spy() {
    const top = this.el.getBoundingClientRect().top + 60;
    let cur = SECTIONS[0].id;
    for (const s of this.body.querySelectorAll('section:not([hidden])')) {
      if (s.getBoundingClientRect().top <= top) cur = s.dataset.section;
    }
    if (cur !== this.current) {
      this.current = cur;
      this.app.markReference?.();
    }
  }

  /** Shows the sections and entries that contain every word of the filter. */
  applyFilter() {
    const words = this.filter.value.trim().toLowerCase().split(/\s+/).filter(Boolean);
    const has = (el) => words.every((w) => el.textContent.toLowerCase().includes(w));
    let any = false;
    for (const sec of this.body.querySelectorAll('section')) {
      // An entry is its heading and what follows, up to the next heading.
      const groups = [[]];
      for (const c of sec.lastChild.children) {
        if (c.tagName === 'H3') groups.push([]);
        groups[groups.length - 1].push(c);
      }
      let shown = false;
      for (const g of groups) {
        const text = { textContent: g.map((c) => c.textContent).join(' ') };
        const on = !words.length || has(text) || has(sec.firstChild);
        for (const c of g) c.hidden = !on;
        shown ||= on && g.length > 0;
      }
      sec.hidden = !shown;
      any ||= shown;
    }
    this.none.hidden = any;
    this.spy();
  }

  visible() {
    this.spy();
  }
}
