# Weft — language specification

Status: version 0.1, implemented in `crates/synth-weft`. *Weft* is a working name.

Weft is a small hardware description language for synchronous FPGA designs. It
compiles directly to synth's RTL IR (`synth-rtl`), without going through Verilog.
It is designed so that the mistakes Verilog makes easy either cannot be written
or are rejected with a precise diagnostic, while the code still reads like
ordinary RTL.

Contents:

1. [Design principles](#1-design-principles)
2. [A tour](#2-a-tour)
3. [Lexical structure](#3-lexical-structure)
4. [Source files and items](#4-source-files-and-items)
5. [Types](#5-types)
6. [Expressions](#6-expressions)
7. [Modules](#7-modules)
8. [Clock domains](#8-clock-domains)
9. [Registers](#9-registers)
10. [Memories](#10-memories)
11. [Instances and extern modules](#11-instances-and-extern-modules)
12. [Functions](#12-functions)
13. [Compile-time evaluation and generics](#13-compile-time-evaluation-and-generics)
14. [The top module](#14-the-top-module)
15. [Semantics](#15-semantics)
16. [Static checks](#16-static-checks)
17. [Lowering to the RTL IR](#17-lowering-to-the-rtl-ir)
18. [Standard library](#18-standard-library)
19. [Not in this version](#19-not-in-this-version)
- [Appendix A: grammar](#appendix-a-grammar)
- [Appendix B: Verilog to Weft](#appendix-b-verilog-to-weft)
- [Appendix C: examples](#appendix-c-examples)

---

## 1. Design principles

1. **One meaning per program.** A program that compiles has one hardware
   behaviour, and simulation and synthesis agree on it. Values are two-state.
   Behaviour is left open only where the source says so explicitly: `undef`
   (§6.10), `unsafe_cross` (§8.6), synchronizer latency (§18) and extern modules
   (§11.3).
2. **Arithmetic is exact.** `+`, `-`, `*`, `/` and `%` produce a type that can
   hold every possible result. Wrapping is spelled out with `+%`, `-%` and `*%`.
   Implicit conversions never lose information.
3. **Registers are declared, not inferred.** Whether a signal is state follows
   from its declaration (`reg`), never from how it is assigned. A register's
   next value is assigned with `:=`; a combinational signal is assigned with `=`.
4. **Clock domains are types.** Every signal belongs to a clock domain. Mixing
   domains without a synchronizer is a compile error. In a module with one
   domain, none of this has to be written down.
5. **Small.** The language has three kinds of state (`reg`, `mem`, `inst`), one
   kind of combinational signal (`wire`, with `let` for named values), pure
   functions, enums, structs and arrays. There are no processes, sensitivity
   lists, blocking assignments, latches, tristates or `initial` blocks.
6. **Direct lowering.** Every construct maps onto the RTL IR's word-level cells,
   flip-flops (with enable, synchronous and asynchronous reset, power-on value)
   and memories (with declared read-during-write behaviour). There is no
   symbolic process lowering.

### Verilog bug classes and where Weft removes them

| Verilog pitfall | Weft | Section |
|---|---|---|
| Silent truncation and extension; context-determined expression widths | Exact result types; only lossless implicit conversions; wrapping only with `%` operators | §6.3, §6.4 |
| Signed and unsigned mixing (one unsigned operand makes the expression unsigned) | Arithmetic and comparisons are exact on mathematical values, so mixing is always correct | §6.3 |
| Out-of-range indexing returns `x` | Index ranges are checked at compile time; `index<N>` and range narrowing make the check easy to satisfy | §5.2, §6.7 |
| Blocking (`=`) versus non-blocking (`<=`); simulation races | One assignment model; registers update at the clock edge, reads see the current state | §7.5, §15 |
| Latches inferred from incomplete `always @*` | Combinational signals must be assigned on every path; latches cannot be written | §7.6 |
| Incomplete sensitivity lists | No sensitivity lists | — |
| Implicit nets from typos | Every name is declared | §4 |
| Multiple drivers; undriven outputs | Single owner per signal; every output and instance input must be driven | §7.6 |
| Positional port connections; dropped ports | Named connections; all inputs connected or defaulted | §11.1 |
| Non-exhaustive `case`; `casez`/`casex` x-matching | Exhaustive `match`; explicit `?` patterns of exact width | §6.9 |
| `!` versus `~`; `if (vec)`; C precedence of `&` versus `==` | `bool` is a separate type; conditions must be `bool`; bitwise operators bind tighter than comparisons; mixing `&&`/`\|\|` needs parentheses | §5.1, §6.2 |
| Unsynchronized inputs; clock-domain crossings | Domain-checked signals; top-level inputs are asynchronous until synchronized | §8, §14 |
| Clocks made from logic; gated clocks | Clocks cannot be computed; use clock enables | §8.1 |
| Forgotten resets; reset polarity mixups; `initial` versus reset | Every register states its reset or power-on value; resets are active high; reset style is chosen per domain | §9 |
| Fragile block RAM inference; unclear read-during-write | `mem` is a declared storage kind; read timing follows from `:=`; read-during-write is defined | §10 |
| x-propagation differences between simulator and synthesis | Two-state values; don't-care only through explicit `undef` | §6.10 |

---

## 2. A tour

A blinking LED. The module has one clock port, so `clk` is its only domain and
nothing needs a domain annotation.

```weft
module top(in clk: clock, out led: bool) {
    reg count: uint<24> = 0;
    count := count +% 1;
    led = count[23];
}
```

A UART transmitter. It has no clock port, so it gets the clock domain of
whichever module instantiates it.

```weft
enum TxState { Idle, Start, Data, Stop }

/// 8N1 transmitter. `tick` pulses once per bit period.
module uart_tx(
    in tick: bool,
    in start: bool,
    in byte: uint<8>,
    out tx: bool,
    out busy: bool,
) {
    reg state: TxState = .Idle;
    reg shift: uint<8> = 0;
    reg bit: index<8> = 0;

    tx = match state {
        .Idle | .Stop => true,
        .Start => false,
        .Data => shift[0],
    };
    busy = state != .Idle;

    match state {
        .Idle => if start {
            shift := byte;
            state := .Start;
        }
        .Start => if tick { state := .Data; }
        .Data => if tick {
            shift := shift >> 1;
            bit := bit +% 1;
            if bit == 7 { state := .Stop; }
        }
        .Stop => if tick { state := .Idle; }
    }
}
```

A board top level with a power-on reset and a synchronized button:

```weft
module top(in clk: clock, in btn: bool, out tx: bool, out led: bool) {
    let por = power_on_reset<CYCLES: 16>() @clk;
    default domain sys { clock: clk, reset: por }

    let go = rising(sync(btn));            // `btn` is asynchronous until synchronized
    inst baud = divider<N: 217>();         // 25 MHz / 115200
    inst uart = uart_tx(tick: baud.tick, start: go, byte: 0x41);
    tx = uart.tx;
    led = uart.busy;
}
```

Things the compiler rejects in these examples, if written differently:

- `count := count + 1;`: the sum is `uint<25>`, which does not fit `uint<24>`.
  The diagnostic suggests `+%` (wrap) or `.trunc(24)`.
- Using `btn` directly in `rising(btn)`: `btn` is an asynchronous top-level input
  and `rising` registers its input. The diagnostic suggests `sync(btn)`.
- Leaving out the `.Stop` arm: the `match` is not exhaustive.
- `busy = true;` inside only the `.Idle` arm: `busy` is not assigned on every
  path. The diagnostic names the missing path.

---

## 3. Lexical structure

**Encoding.** Source files are UTF-8. Identifiers are ASCII:
`[A-Za-z_][A-Za-z0-9_]*`. Identifiers starting with `__` are reserved.

**Comments.** `// …` to end of line, `/* … */` (not nested). `///` starts a doc
comment that attaches to the next item, port, or declaration.

**Keywords.**

```
async bool clock const default domain else enum extern fn for if in index init
inout inst int let match mem module out reg sint string struct true false type
uint undef var wire
```

These names are predeclared builtins and cannot be redefined:
`concat`, `from_bits`, `unsafe_cross`, `hex_file`, `bin_file`, `clog2`,
`bits_for`, `min`, `max`, `assert`.

**Integer literals.** Decimal `42`, hexadecimal `0x2A`, binary `0b101010`,
octal `0o52`. Underscores may separate digits: `1_000_000`, `0xDEAD_BEEF`.
Integer literals have no type of their own (§6.1). Binary and hexadecimal
literals also have a *natural width*: the number of digits, times 1 or 4,
including leading zeros and excluding underscores. It is used only where a
width is required and no other source of width exists (`concat`, §6.8).

**Bit patterns.** In `match` patterns only, binary literals may contain `?` for
a don't-care bit: `0b1??0_01??`.

**Strings.** `"…"` with the escapes `\\`, `\"`, `\n`, `\t`. Strings appear only as
generic arguments and file names.

**Punctuation and operators.**

```
+ - * / % +% -% *% ** ~ ! & | ^ && || << >> == != < <= > >=
= := => -> . .. ..= +: , ; : @ # ( ) [ ] { }
```

---

## 4. Source files and items

A design is the set of source files given to the compiler. All files share one
namespace. Items may appear in any order and may refer to each other regardless
of file or order. Defining a name twice is an error.

Items:

| Item | Form |
|---|---|
| Module | `module name<generics>(ports) { body }` |
| Extern module | `extern module name<generics>(ports);` |
| Function | `fn name<generics>(params) -> Type { body }` |
| Struct | `struct Name { field: Type, … }` |
| Enum | `enum Name { A, B, … }` or `enum Name: uint<N> { A = 1, … }` |
| Constant | `const NAME: Type = expr;` |
| Static assertion | `assert(cond, "message");` |

Generic parameter lists (`<…>`) are optional on modules, extern modules and
functions (§13). Doc comments (`///`) and attributes (`#[…]`, §7.9) may precede
any item.

Naming conventions are not enforced: `snake_case` for values, modules and
functions; `CamelCase` for structs, enums and variants; `UPPER_CASE` for
constants and value generics (extern modules keep their Verilog parameter
names).

**Layout.** `synth fmt` (and the language server's formatting) indents by four
spaces per open bracket, with one more level for the continuation of a
statement, and aligns the port names of a port list written one per line
(`in  start: bool,` above `out tx: bool,`). It changes only layout, never the
code on a line.

**Scopes.** Items are visible everywhere. Module-level declarations (§7.3) are
visible throughout their module body, before and after the declaration.
Declarations inside a block (`let`, `var`, loop variables, and declarations
inside `for` or compile-time `if`) are visible from their declaration to the end
of the block. Declaring a name that is already visible is an error: there is no
shadowing.

---

## 5. Types

### 5.1 Hardware types

| Type | Values | Width |
|---|---|---|
| `bool` | `false`, `true` | 1 |
| `uint<N>`, N ≥ 1 | 0 … 2^N − 1 | N |
| `sint<N>`, N ≥ 1 | −2^(N−1) … 2^(N−1) − 1, two's complement | N |
| `index<N>`, N ≥ 1 | 0 … N − 1 | max(1, clog2(N)) |
| `[T; N]`, N ≥ 1 | N elements of type T | N × width(T) |
| struct | one value per field | sum of the field widths |
| enum | one of the variants | see §5.4 |

`bool` is not an integer. Conditions (`if`, `&&`, `||`, `!`) require `bool`, and
there is no implicit conversion between `bool` and integers (§6.5).

`uint`, `sint` and `index` are the *integer types*. Arithmetic on them is exact
(§6.3). They are all unsigned in representation except `sint`.

### 5.2 Integer ranges

Every integer value has a statically known **range** `lo..=hi`:

- `uint<N>`: `0..=2^N−1`; `sint<N>`: `−2^(N−1)..=2^(N−1)−1`; `index<N>`: `0..=N−1`.
- An integer literal `c`: `c..=c`.
- An arithmetic expression: the exact range of its possible results (§6.3).
- Inside a branch guarded by a comparison, a narrowed range (§6.7).

The range is the expression's type. Diagnostics print a range that is not one of
the declared forms as `int<lo..=hi>`. That form cannot be written in source;
expressions get it from their operands.

A value is stored in its **natural representation**: `uint<w>` with
w = max(1, bits(hi)) when lo ≥ 0, otherwise the smallest `sint<w>` containing
the range. `index<N>` is stored as `uint<max(1, clog2(N))>`. Bit-level
operations (§6.6) see the natural representation.

`index<N>` is the type of a valid index into anything with N elements (arrays,
memories, the bits of a `uint<N>`). Wrapping arithmetic on it wraps modulo N,
which makes it the type for counters with a period that is not a power of two
(§6.4).

### 5.3 Structs

```weft
struct Pixel { r: uint<5>, g: uint<6>, b: uint<5> }
```

Fields have hardware types. A struct value is written `Pixel { r: 0, g: 63, b: 0 }`
and every field must be given. In bit form (`.bits()`, §6.5), the first field
holds the most significant bits, so `p.bits() == concat(p.r, p.g, p.b)`. Structs
are compared with `==` and `!=` (all fields equal). Structs cannot be generic.

### 5.4 Enums

```weft
enum State { Idle, Busy, Done }                 // encoding chosen by the compiler
enum Opcode: uint<7> { Load = 0x03, Store = 0x23, Op = 0x33 }   // explicit encoding
```

Variants are written `State.Idle`. Where the expected type is known (an
assignment, comparison or `match` against a value of the enum type, an argument
or a port connection), they may be written `.Idle`.

An enum without an encoding has no observable bit representation: `.bits()` is
not allowed on it and the compiler is free to choose (and re-choose) the
encoding. An enum with an explicit encoding `: uint<N>` declares distinct values
that fit `uint<N>`. `.bits()` returns them. There is no conversion from integers
to enums: a value outside the declared encodings is impossible. To decode an
integer, `match` it against constants (§6.9).

Enums support `==`, `!=` and `match`, and nothing else.

### 5.5 Arrays

`[T; N]` holds N elements of type T, indexed from 0. Array literals are written
`[a, b, c]` or `[v; N]` (N copies of v). In bit form, element 0 holds the least
significant bits. Arrays support `==`, `!=`, constant and dynamic indexing (§6.7),
and assignment of single elements.

### 5.6 Compile-time types

These types exist only at compile time:

| Type | Use |
|---|---|
| `int` | Arbitrary-precision signed integers: generics, constants, loop variables |
| `bool` | As a generic parameter or constant |
| `string` | Generic arguments (file names, extern parameters) |
| `type` | Type generics (§13) |

An `int` value used in a hardware context behaves like an integer literal with
that value.

### 5.7 Clocks and domains

`clock` is the type of clock signals. Clocks cannot be used in expressions. They
are only connected to ports, used to define domains (§8.2), and passed to
instances. A domain is not a value; it is a name that refers to a clock and a
reset (§8).

---

## 6. Expressions

### 6.1 Literals and expected types

An integer literal takes the type the context expects, if there is one: the
declared type of an assignment target, a port, a parameter or a `let` with a
type annotation. Its value must fit that type. Without an expected type, it keeps
its exact range `c..=c`. `true` and `false` are `bool`. `.Variant` needs an
expected enum type. `undef` (§6.10) needs an expected type. Expected types
propagate into the branches of `if` and `match` expressions and into the elements
of array literals, so `state := if ok { .Done } else { .Idle };` works.

### 6.2 Operators and precedence

From highest to lowest:

| Level | Operators | Associativity |
|---|---|---|
| 1 | `.field` `.method()` `[i]` `[hi:lo]` `[i +: W]` calls | left |
| 2 | `**` (compile-time `int` only) | right |
| 3 | unary `-` `!` `~` | prefix |
| 4 | `*` `*%` `/` `%` | left |
| 5 | `+` `-` `+%` `-%` | left |
| 6 | `<<` `>>` | left |
| 7 | `&` | left |
| 8 | `^` | left |
| 9 | `\|` | left |
| 10 | `==` `!=` `<` `<=` `>` `>=` | none |
| 11 | `&&` | left |
| 12 | `\|\|` | left |

Rules that differ from C and Verilog:

- Bitwise operators bind tighter than comparisons: `a & mask == 0` means
  `(a & mask) == 0`.
- Comparisons do not chain: `a < b < c` is an error.
- Mixing `&&` and `||`, or mixing two of `&`, `|`, `^`, without parentheses is
  an error.
- There is no `?:`. Use an `if` expression (§6.9).

### 6.3 Exact arithmetic

`a + b`, `a - b`, `a * b`, `-a`, `a / c` and `a % c` take integer operands of any
range and signedness. They compute the mathematical result, and their type is the
exact range of that result:

| Expression | Range, for a in `al..=ah` and b in `bl..=bh` |
|---|---|
| `a + b` | `al+bl ..= ah+bh` |
| `a - b` | `al−bh ..= ah−bl` |
| `a * b` | min to max of the four corner products |
| `-a` | `−ah ..= −al` |
| `a / c` | c a constant > 0; truncates toward zero |
| `a % c` | c a constant > 0; the sign of the result follows `a` (`0..=c−1` when al ≥ 0) |

Examples, with `x: uint<8>` and `s: sint<8>`:

| Expression | Range | Natural representation |
|---|---|---|
| `x + 1` | `1..=256` | `uint<9>` |
| `x - 1` | `−1..=254` | `sint<9>` |
| `x + s` | `−128..=382` | `sint<10>` |
| `x * 3` | `0..=765` | `uint<10>` |
| `x / 10` | `0..=25` | `uint<5>` |
| `x % 10` | `0..=9` | `uint<4>` |

Mixing signed and unsigned operands is always correct, because no operand is
reinterpreted.

Division and remainder require a constant positive divisor. Dividers built from
runtime divisors are large and slow, and are rarely what a design wants; the
standard library can provide sequential dividers later.

### 6.4 Wrapping arithmetic

`a +% b`, `a -% b` and `a *% b` compute modulo the size of a **wrap type** W:

- W is the declared integer type (`uint<N>`, `sint<N>` or `index<N>`) of an
  operand. If both operands have declared types, they must be the same type:
  convert one first (`a +% uint<8>(b)`). If neither has one (for example
  `(a + b) +% c` with only ranges), it is an error.
- The other operand must convert implicitly to W (§6.5): a constant, or a value
  whose range fits.
- The result has type W. For `uint<N>` and `sint<N>`, this is ordinary
  two's-complement wrapping. For `index<N>`, `+%` and `-%` wrap modulo N, and
  `*%` is not allowed.

```weft
reg phase: index<217> = 0;
phase := phase +% 1;            // 0, 1, …, 216, 0, …
let full = (wr +% 1) == rd;     // FIFO full, with wr and rd: index<DEPTH>
```

### 6.5 Conversions

**Implicit conversions** happen at assignments, port connections, function
arguments and results, `if` and `match` arms, and array literal elements. An
integer value converts implicitly to an integer type when its range is contained
in that type's range. Nothing else converts implicitly: not between `bool` and
integers, not from enums or structs, and never with loss.

| From | To | Implicit? |
|---|---|---|
| `uint<8>` | `uint<12>`, `sint<9>`, `index<256>` | yes |
| `index<200>` | `uint<8>` | yes |
| `x + 1` with `x: uint<8>` (`1..=256`) | `uint<9>` | yes |
| `x + 1` | `uint<8>` | no: use `x +% 1` or `(x + 1).trunc(8)` |
| `sint<8>` | `uint<16>` | no: negative values do not fit |
| `bool` | `uint<1>` | no: use `b.bits()` |

**Explicit conversions:**

| Form | Meaning |
|---|---|
| `T(x)` | Lossless conversion to integer type T; an error if `x`'s range does not fit. Use it to give a value a type without an assignment |
| `x.trunc(N)` | The low N bits of `x`'s natural representation: `uint<N>` for unsigned values, `sint<N>` for signed values. N ≤ width |
| `x.bits()` | The bit representation as `uint<width>`, for any type except enums without an encoding |
| `x.as_sint()` | Reinterpret `uint<N>` as `sint<N>` |
| `from_bits(x)` | Rebuild a value of the expected type from a `uint` of exactly its width, or from a constant that fits that width. Allowed for `bool`, integer types except `index`, and structs and arrays built from these. Not allowed for enums |

### 6.6 Bitwise operators, shifts and methods

Bitwise operators work on the natural representation:

- `a & b`, `a | b`, `a ^ b`: both operands `bool`, or both unsigned with equal
  widths. A constant operand takes the other's width. `sint` operands are not
  allowed: use `.bits()`.
- `~a`: unsigned integers. `!a`: `bool` only.
- `a << n`, `a >> n`: `a` is an integer, and the result has the natural
  representation of `a` (shifts do not widen, and bits shifted out are lost).
  `>>` fills with the sign bit for signed values and with zeros otherwise. `n`
  is a non-negative constant or an unsigned integer. Shifting by the width or
  more gives zero (or all sign bits). To scale exactly, use `*`, or
  `concat(x, 0b00)`.

Methods on unsigned integers (natural representation):

| Method | Result |
|---|---|
| `x.any()`, `x.all()` | `bool`: some bit set, all bits set |
| `x.parity()` | `bool`: XOR of all bits |
| `x.reverse()` | Bit-reversed, same type |
| `x.replicate(k)` | `uint<k × width>`, k a constant ≥ 1 |
| `x.rotate_left(n)`, `x.rotate_right(n)` | Same type; n constant or unsigned |
| `x.width` | `int`: the width |

`.width` is available on every hardware value and type name (`T.width` for a
type parameter T).

### 6.7 Indexing and slicing

| Form | Applies to | Result |
|---|---|---|
| `x[i]` | integer: bit i of the natural representation | `bool` |
| `a[i]` | array or memory: element i | element type |
| `x[hi:lo]` | integer; constants with hi ≥ lo | `uint<hi−lo+1>` |
| `x[i +: W]` | integer; W constant, i unsigned | `uint<W>`: bits i … i+W−1 |

Bits are numbered from 0 (least significant). Slices are always written high to
low. Slices of `sint` values give unsigned bits.

**Every index is checked at compile time.** The index's range must lie within the
valid positions: `0..=N−1` for `x[i]` and `a[i]`, and `0..=N−W` for `x[i +: W]`.
With constant indices, this is a bounds check. With runtime indices, the index
type has to fit:

```weft
mem fb: [uint<4>; 4800];
reg addr: index<4800> = 0;
let px = fb[addr];                  // ok: addr is always below 4800

let wide: uint<13> = …;
let bad = fb[wide];                 // error: index may be up to 8191 but `fb` has 4800 entries
```

**Range narrowing.** Inside a branch guarded by a comparison between a name
(a port, `let`, `wire`, `reg` or loop variable) and a constant, the name has a
narrowed range:

- In the `then` branch of `if c`, where `c` is a comparison `n < k`, `n <= k`,
  `n > k`, `n >= k` or `n == k` (or the same with `k` on the left), or a
  conjunction of such comparisons with `&&`.
- In the `else` branch of an `if` whose condition is a single such comparison.
- In a `match` arm whose pattern is a literal, a range, or alternatives of those
  (§6.9), for the scrutinee when it is a name.

```weft
let pixel = if wide < 4800 { fb[wide] } else { 0 };       // ok
if wide < 4800 { fb[wide] := color; }                      // ok
let mag = if s < 0 { -s } else { s };                      // range 0..=128: uint<8>
if level < 255 { level := level + 1; }                     // saturates; fits uint<8> here
```

Narrowing changes only the static range, not the value or its representation.

### 6.8 Concatenation

`concat(a, b, …)` joins values of type `bool`, `uint<N>` or `sint<N>`, with the
first argument most significant. The result is `uint<sum of widths>`. Every
argument needs a width: binary and hexadecimal literals use their natural width
(§3), and other constants must be converted (`uint<3>(5)`).

```weft
led = concat(beat[24], 0b00, !soc.q);       // uint<4>
let word = concat(hi, lo);                  // uint<16> from two uint<8>
```

### 6.9 Conditional expressions and match

**If expressions.** `if c { a } else { b }`, where `c` is `bool` and both
branches are present. `else if` chains are allowed. The branches are joined into
a common type: for integers, the union of their ranges; otherwise both branches
must have the same type. A branch block may
start with `let` statements before its final expression.

**Match.** `match x { pattern => value, … }` selects the first arm whose pattern
matches. The scrutinee may be an integer, `bool` or enum.

| Pattern | Matches |
|---|---|
| `5`, `-1`, `0x2A` | That value; it must lie in the scrutinee's range |
| `0..=9` | An inclusive range |
| `0b1??0` | A bit pattern; its digit count must equal the scrutinee width; integers only |
| `.Idle`, `State.Idle` | An enum variant |
| `true`, `false` | A `bool` value |
| `p \| q` | Either pattern |
| `_` | Anything |

A `match` must be **exhaustive**: its patterns must cover every value in the
scrutinee's range (every variant, for enums). An arm that can never match
because earlier arms cover it is an error. A `_` arm on an enum whose variants
are all listed is an error; a `_` that covers variants is allowed but produces a
warning, because it hides variants added later.

As an expression, arms are separated by commas, every arm gives a value, and the
arms are joined as for `if`. As a statement (§7.4), every arm is a statement:
an assignment, an `if`, a `match` or a block, without commas between arms.

### 6.10 Undef

`undef` is an explicit don't-care of the expected type. It stands for some
valid value of that type, which the compiler may choose freely and differently
at each use and in each cycle. It is the only way to give the optimizer that
freedom.

```weft
data = match sel { 0 => a, 1 => b, 2 => c, _ => undef };
```

`undef` is not allowed in register reset or power-on values, memory initial
contents, constants, or generic arguments. Comparing `undef` with anything is an
error.

### 6.11 Function and module calls

`f(a, b)` calls a function (§12). `m(port: value, …)` in a `let` initializer
instantiates a module with a single output and gives that output's value
(§11.2).

---

## 7. Modules

### 7.1 Declaration

```weft
module name<generics>(ports) {
    body
}
```

### 7.2 Ports

```
in    name: Type [@domain] [= default],
out   name: Type [@domain],
inout name: Type,
domain name,
default domain name,
```

- `in` ports are read-only in the body. A default value (a constant) lets
  instantiations leave the port unconnected.
- `out` ports behave like wires (§7.3): the body must drive them completely.
  They may be read inside the module.
- `inout` ports exist only on the top module and on extern modules. They can be
  connected only to `inout` ports of extern modules (pins to IO primitives).
  Their type is `bool` or `uint<N>`.
- `in name: clock` declares a clock input. It also defines a domain named `name`
  (§8.2).
- `domain name` declares a domain parameter (§8.2).
- `@domain` annotations are needed only in modules without a default domain
  (§8.3), and for `@async` ports.

A trailing comma is allowed after the last port.

### 7.3 Declarations in the body

| Declaration | Meaning |
|---|---|
| `const NAME[: T] = expr;` | A compile-time constant; T is a hardware type, `int` or `string` |
| `let name[: T] [@d] = expr;` | A named combinational value |
| `wire name: T [@d];` | A combinational signal, driven by assignments (§7.6) |
| `reg name: T [@d] = expr;` | A register with a reset value (§9) |
| `reg name: T [@d] init expr;` | A register with only a power-on value (§9) |
| `mem name: [T; N] [@d] [init expr];` | A memory (§10) |
| `inst name = module<args>(conns) [@d];` | An instance (§11) |
| `[default] domain name { … }` | A clock domain (§8.2) |

`let` at module level is visible throughout the module body, like the other
declarations. The value of a `let` is its expression; a `let` cannot be assigned.
A `let` whose value is a compile-time constant is itself a constant.

### 7.4 Statements

| Statement | Meaning |
|---|---|
| `target = expr;` | Drive a wire, `out` port or instance input |
| `target := expr;` | Set the next value of a register, or write a memory element |
| `if c { … } [else if c { … }] [else { … }]` | Conditional statements |
| `match x { pattern => stmt … }` | Conditional statements; arms are statements or blocks, without commas |
| `for i in a..b { … }` | Compile-time unrolled loop; `..=` for inclusive. `a` and `b` are constants |
| `assert(cond, "message");` | Compile-time assertion |

Targets are names, optionally followed by field accesses and indices:
`out_bus`, `pix.r`, `regs[3]`, `regs[i]`, `word[7:0]`, `inst.port`.

Declarations other than `let` may appear only at module level, inside `for`, or
inside an `if` whose condition is a compile-time constant (§13.3). A `let` inside
a branch is local to the branch.

### 7.5 Signals are not variables

A module body is a description of hardware, not a sequence of steps. Every
signal has one value per clock cycle:

- **Reading** a wire, `out` port or instance input anywhere in the module gives
  the value it is driven to in this cycle, whatever the textual position of the
  read.
- **Reading** a register gives its current state, never its next value.
- **Assignments** to the same target are prioritized in program order: when
  several assignments to a target apply in a cycle, the last one wins. This is
  how defaults are written:

```weft
ready = false;                      // default
if state == .Wait && valid {
    ready = true;                   // overrides the default when it applies
}
```

For sequential computations, such as running sums in a loop, use a function with
`var` locals (§12).

### 7.6 Driving rules

- **Single owner.** Wires and `out` ports are driven only by `=` in their own
  module. Instance inputs are driven only by the parent, by connection or by
  `inst.port = …`. Registers and memories are assigned only by `:=` in their
  module. Inputs and instance outputs cannot be assigned.
- **Complete assignment.** Every bit of every wire, `out` port and instance input
  without a default must be assigned on every path. The check is structural: an
  assignment at the top of the body, or inside every branch of an `if` with an
  `else`, or inside every arm of a `match`, covers the paths through that
  statement. The check is per bit for constant field, index and slice targets.
  An assignment with a runtime index (`w[i] = v`) never counts as complete.
  A missing assignment is an error that names the path, for example "`ready` is
  not assigned when `state == .Busy` and `!done`".
- **Registers hold.** On paths without an assignment, a register keeps its value.
  This is how clock enables are written (§9).
- **Dead assignments.** An assignment whose bits are always overridden by later
  assignments produces a warning.

There are no combinational loops: a signal that depends combinationally on
itself, through any number of wires, lets and instances, is an error that lists
the cycle.

### 7.7 Assigning parts of signals

Constant parts of a signal can be assigned independently: fields (`p.r = …`),
elements (`a[3] = …`), slices (`w[7:4] = …`). Runtime indices are allowed on
the left of `=` and `:=` for arrays, registers holding arrays, and memories: the
assignment applies to the element selected in that cycle.

```weft
reg regs: [uint<32>; 32] = [0; 32];
if we && rd != 0 { regs[rd] := wdata; }
```

### 7.8 Instances inside loops

Declarations inside a `for` body are created once per iteration and are visible
only in that iteration. Results leave the loop through assignments to
module-level signals:

```weft
wire lanes_out: [uint<8>; 4];
for i in 0..4 {
    inst lane = scrambler(data: lanes_in[i]);
    lanes_out[i] = lane.q;
}
```

### 7.9 Attributes

Attributes are written `#[name]` or `#[name(args)]` before a declaration or item.

| Attribute | On | Effect |
|---|---|---|
| `#[keep]` | `reg`, `wire`, `inst` | Not removed by optimization |
| `#[async_reg]` | `reg` | A synchronizer stage: kept, not merged into shift registers, placed close together |
| `#[attr("name", value)]` | declarations | Passed to the RTL IR as an attribute |

Attributes never change behaviour.

---

## 8. Clock domains

### 8.1 Model

A **domain** is a clock (rising or falling edge) together with a reset, which may
be absent. Registers, memories and module ports belong to domains. Every value
has a **timing tag**, one of:

- **static**: constants, and the result of `unsafe_cross` (§8.6);
- **a clock**: values that change only at the edges of that clock;
- **async**: values that may change at any time.

Domains that share a clock share a tag. They differ only in reset, and mixing
their signals is safe.

Clocks come only from clock input ports and from clock outputs of instances
(PLLs, oscillators, global buffers). Clocks cannot be computed from logic, so
divided and gated clocks cannot be written. Use a clock enable: a register
assigned under `if tick { … }`.

### 8.2 Declaring domains

A module has domains from three sources:

1. **Clock input ports.** `in clk: clock` defines a domain `clk` with that clock
   and no reset.
2. **Domain parameters.** `domain name` in the port list receives a domain from
   the parent: its clock and reset.
3. **Body definitions.**

   ```weft
   domain sys { clock: clk, reset: rst }               // synchronous reset
   domain sys { clock: clk, async_reset: rst }         // asynchronous reset
   domain spi { clock: sck, edge: falling }            // falling edge, no reset
   ```

   `clock` is a clock value. `reset` or `async_reset` is a `bool` expression,
   active high. Its tag must be static or the domain's own clock. It must not
   depend combinationally on a register that this domain resets. `edge` is
   `rising` (the default) or `falling`; these two words are contextual here and
   do not refer to the standard library modules of the same names.

A module with no clock input ports and no domain parameters has one **implicit
domain parameter**. This is how most modules are written: they use whatever
domain the parent gives them.

Each domain `d` has two properties: `d.clock` (its clock) and `d.reset` (a `bool`
with tag `d.clock`, `false` if the domain has no reset). In these expressions,
`default` names the default domain (§8.3), which is how the implicit domain is
referred to: `default.clock`, `default.reset`.

### 8.3 The default domain

A module's **default domain** is used for every declaration and port that has no
`@` annotation:

- the implicit domain parameter, if the module has one;
- otherwise the module's only domain, if it has exactly one;
- otherwise the domain declared with `default` (`default domain sys { … }` in the
  body, or `default domain sys` in the ports). At most one domain may be marked,
  and none may be marked in a module with an implicit domain parameter.

In a module without a default domain, every port, `reg` and `mem` needs an `@`
annotation, and so does every instance or module call whose domain parameter is
not connected explicitly. `wire` and `let` get their tags from their values.

`@async` may annotate ports in any module. An `@async` input accepts values with
any tag (the module promises to synchronize it). An `@async` output may carry
values with any tag.

### 8.4 Tag checking

Tags are computed for every expression:

- static joined with T gives T; T joined with T gives T; two different clocks
  joined give async; async joined with anything gives async;
- a register or instance output has the tag of its domain's clock;
- a memory read has the join of its address's tag and the memory's clock;
- the tag of a wire, `let` or `out` value is the join of everything assigned to
  it and every condition guarding those assignments.

A value may flow into a **sink** of clock C (a register or memory write in a
domain of C, an `out` port or instance input of a domain of C, a domain reset)
only if its tag is static or C. The conditions guarding an assignment flow into
its target too. A sink tagged async accepts any value. Anything
else is an error that names the offending source and the path it took:

```
error: `btn` is asynchronous and reaches register `prev` (domain `sys`)
  --> top.weft:6:24
   |
 6 |     let go = rising(btn);
   |                     ^^^ asynchronous top-level input
   = help: synchronize it first: `rising(sync(btn))`
   = help: or, if the board drives it synchronously to `clk`, declare it `in btn: bool @clk`
```

Function calls take the join of their arguments' tags. Instances map the
child's domains to the parent's: a child port of domain `d` has, in the parent,
the tag of the clock connected to `d`.

Inside a module, different domain parameters and different clock ports have
different clocks, even if the parent connects the same clock to them. The check
is modular and conservative.

### 8.5 Crossing domains

| Crossing | Use |
|---|---|
| One bit, level | `sync(x)`: a two-flip-flop synchronizer (§18) |
| One bit, pulse | `rising(sync(x))` or a toggle handshake |
| Reset from a button or another domain | `reset_sync(x)`: asynchronous assertion, synchronous release |
| Multi-bit data | A handshake or an asynchronous FIFO (not yet in the standard library, §19) |
| Quasi-static configuration | `unsafe_cross(x)` |

`sync` accepts only `bool`. Synchronizing the bits of a bus separately is the
classic bug that this prevents.

### 8.6 Unsafe crossing

`unsafe_cross(x)` returns `x` with a static tag, so it is accepted by every
domain. The expression inside it is not tag-checked. It is for signals that are
stable when sampled by construction (configuration written before use), for
dual-clock memory reads (§10.4), and for the implementation of synchronizers in
the standard library. It is the only way to bypass tag checking, so a search for
`unsafe_cross` finds every crossing that is not checked.

### 8.7 Example: a PLL clock domain

The standard library's `synth_pll<IN_HZ, OUT_HZ>(clk_in, clk_out, locked)`
makes a clock of `OUT_HZ` from `clk_in` (`IN_HZ`) with the device's PLL: an
`SB_PLL40_CORE` on iCE40 (its settings found as `icepll` finds them), a
`PLLE2_ADV` on Artix-7. When `OUT_HZ` equals `IN_HZ` it is `clk_in` itself,
and `locked` is always high, so one source serves boards with and without a
PLL. The output frequency must be one the PLL makes to within 0.2%; it
becomes the clock's timing target. The same primitive exists in Verilog:
`synth_pll #(.IN_HZ(..), .OUT_HZ(..)) pll (.clk_in(..), .clk_out(..),
.locked(..))`.

```weft
module top<CLK_HZ: int = 100_000_000>(in clk: clock, in btn: bool, out led: bool) {
    inst pll = synth_pll<IN_HZ: CLK_HZ, OUT_HZ: 40_000_000>(clk_in: clk);
    domain px_raw { clock: pll.clk_out }
    let rst = reset_sync(!pll.locked || btn) @px_raw;
    default domain px { clock: pll.clk_out, reset: rst }

    reg count: uint<26> = 0;        // in `px`, reset by `rst`
    count := count +% 1;
    led = count[25];
}
```

`!pll.locked || btn` mixes two asynchronous values, which is fine: it goes to
the asynchronous input of `reset_sync`. Memory writes are not blocked by reset
(§10.2): a design whose writes depend on registers with reset values writes
nothing unexpected while it waits for the PLL.

---

## 9. Registers

```weft
reg state: State = .Idle;              // reset value .Idle; powers up as .Idle
reg shift: uint<8> init 0;             // powers up as 0; not affected by reset
reg acc: sint<16> @dsp = 0;            // in domain `dsp`
```

- Every register states its value: `= v` is the **reset value**, which is also
  its power-on value; `init v` is the **power-on value** only. There is no
  register with an unspecified start. `v` is a constant (it may use `.Variant`,
  struct and array literals).
- Whether reset is synchronous or asynchronous, and whether there is one at all,
  is a property of the domain (§8.2), chosen where the domain is defined. A
  module written with `= v` works in every domain: without a reset, `= v` is a
  power-on value.
- Use `init` for registers that must not be reset: a power-on reset counter, a
  synchronizer, a wide datapath register where reset logic would cost area.
- While the domain's reset is asserted, registers with a reset value take it
  (at each clock edge for `reset`, immediately for `async_reset`), whatever their
  assignments say. Registers with `init` are not affected.
- At each active clock edge of the register's domain (when not in reset), the
  register takes the value of the last `:=` that applies in that cycle, or keeps
  its value if none applies. Conditions around `:=` become clock enables.

```weft
reg sample: uint<12> init 0;
if adc_valid { sample := adc_data; }          // clock enable `adc_valid`
```

---

## 10. Memories

```weft
mem ram: [uint<32>; 1024] init hex_file("firmware.hex");
mem fb: [uint<4>; 4800] @sys;                  // initial contents 0
```

### 10.1 Declaration and initial contents

A memory is an array of words with no reset. Its initial contents are given by
`init` with a constant array (which may be computed by a function, §13.2),
`hex_file("path")` or `bin_file("path")` (the formats of `$readmemh` and
`$readmemb`). Paths are relative to the directory of the source file that
contains the string. Without `init`, all words start at 0.

The element type may be any hardware type. Memories cannot be compared, passed
or assigned as a whole.

### 10.2 Writes

`m[i] := v` writes element `i` at the next active edge of the memory's domain,
under the conditions that guard the statement. Each `:=` to a memory is a write
port. When several writes hit the same address in a cycle, the last one in
program order wins. Assigning a part of an element writes only those bits
(per-bit write enables):

```weft
if we {
    if be[0] { ram[addr][7:0] := wdata[7:0]; }
    if be[1] { ram[addr][15:8] := wdata[15:8]; }
}
```

Writes are not blocked by the domain's reset.

### 10.3 Reads

`m[i]` reads the current contents of element `i`: the contents before this
cycle's writes take effect.

- **Registered read** (block RAM): assign the read to a register.

  ```weft
  reg q: uint<32> init 0;
  q := ram[raddr];          // q shows ram[raddr] one cycle after raddr was presented
  ```

  If the same address is written in the same cycle, `q` gets the old contents
  (read-first). For the new contents (write-first), say so:

  ```weft
  q := if we && waddr == raddr { wdata } else { ram[raddr] };
  ```

  The first form is a read-first block RAM port. The second is the same port
  plus a bypass multiplexer, which synthesis may fold into a transparent port
  where the target supports one.
- **Combinational read**: `m[i]` used directly in logic. It needs LUT RAM or
  registers. On iCE40, which has no LUT RAM, the compiler reports that the
  memory is implemented in flip-flops.

The `:=` makes the one-cycle latency of block RAM visible in the source, which is
the usual source of off-by-one-cycle bugs.

### 10.4 Dual-clock memories

Reading a memory in a domain whose clock differs from the memory's domain is a
crossing: reading a word while it is being written gives an undefined value in
hardware. Write the read inside `unsafe_cross`, as a registered read:

```weft
mem fb: [uint<4>; 4800] @sys;
reg px_color: uint<4> @px init 0;
px_color := unsafe_cross(fb[px_addr]);     // dual-clock block RAM
```

This is the pattern for frame buffers and asynchronous FIFOs.

---

## 11. Instances and extern modules

### 11.1 Instances

```weft
inst tx = uart_tx(tick: baud.tick, start: go, byte: data);
inst q = fifo<T: Pixel, DEPTH: 512>(push: p, data: d, pop: r);
inst u = uart_tx(tick: t, start: s, byte: b) @sys;
```

- Connections are by name. Generic arguments are by name (§13).
- Every input without a default must be driven, either in the connection list or
  by assignments `tx.start = …;` in the body. Assignments follow the wire rules
  (§7.6), so inputs can be driven conditionally with defaults.
- Outputs are read as `tx.busy`. They cannot be assigned.
- **Domains.** The child's domain parameters (the implicit one, named domain
  parameters, and clock ports) are connected like ports: `wr: sys`,
  `clk: sys.clock`. A child with exactly one domain parameter gets the parent's
  default domain when the connection is left out; `@d` after the instance binds it
  to domain `d` instead.
- Instances may refer to each other's outputs in any order. Combinational cycles
  through instances are errors (§7.6).

### 11.2 Module calls

A module with exactly one `out` port can be instantiated inside a `let`
initializer, as a call:

```weft
let rx = sync(rx_pin);
let go = rising(sync(btn));
let por = power_on_reset<CYCLES: 16>() @clk;
```

The call creates an instance and gives its output. Arguments are named, except
that a module with exactly one input without a default may take it positionally.
Module calls are allowed only in `let` initializers at module level (including
inside `for` and compile-time `if`), never inside runtime `if`/`match`, because
the instance exists unconditionally.

### 11.3 Extern modules

An extern module declares the interface of a module defined elsewhere: a
Verilog module given to the compiler, or a device primitive.

```weft
extern module servant<memfile: string, memsize: int>(
    domain { clock: wb_clk, reset: wb_rst },
    out q: bool,
);

extern module SB_PLL40_CORE<FEEDBACK_PATH: string = "SIMPLE", DIVR: int = 0,
                            DIVF: int = 0, DIVQ: int = 0, FILTER_RANGE: int = 0>(
    in REFERENCECLK: clock,
    out PLLOUTCORE: clock,
    out LOCK: bool @async,
    in RESETB: bool @async = true,
    in BYPASS: bool @async = false,
);
```

- Ports use the module port forms. Types must match the Verilog port widths:
  `bool` for 1 bit, `uint<N>`/`sint<N>` otherwise.
- `domain { clock: c, reset: r }` binds the extern's implicit domain to Verilog
  ports: the parent connects a domain, and its clock and reset drive ports `c`
  and `r`. `reset: !r_n` binds an active-low reset port. Named forms
  (`domain wr { clock: wclk }`) declare several domains. Ports named in a binding
  are not connected separately.
- The compiler trusts the declaration: it cannot check the Verilog module's
  internal timing.
- Generic parameters become Verilog parameters (`int` and `string`).

Device primitives for the selected architecture are predeclared (§18.2). For a
Verilog module, the compiler elaborates it with the given parameters using the
Verilog frontend and inlines the result. A declared port that does not exist in
the definition, or a width mismatch, is an error.

---

## 12. Functions

```weft
fn parity_bit<N: int>(x: uint<N>) -> bool {
    x.parity()
}

fn count_ones<N: int>(x: uint<N>) -> uint<bits_for(N)> {
    var n: uint<bits_for(N)> = 0;
    for i in 0..N {
        if x[i] { n = n +% 1; }
    }
    n
}
```

- Functions are pure combinational logic. They cannot declare registers, wires,
  memories, instances or domains. Every call is inlined.
- The body is a block: statements, then a final expression that gives the
  result.
- `var name: T = expr;` declares a mutable local. Unlike signals, locals are
  sequential: a read sees the most recent assignment in program order, and
  assignments inside `if` and `match` branches merge at the end of the branch.
  `var` exists only in functions.
- Value generic parameters can be inferred from argument types where they appear
  directly as `uint<N>`, `sint<N>`, `index<N>` or `[T; N]`.
- Recursion is allowed. It must terminate during elaboration; the depth limit
  is 256.
- Every function can be evaluated at compile time when its arguments are
  constants (§13.2).
- A call's result has the join of its arguments' tags (§8.4).

---

## 13. Compile-time evaluation and generics

### 13.1 Generic parameters

```weft
module fifo<T: type, DEPTH: int = 16>(
    in push: bool, in data: T,
    in pop: bool, out head: T,
    out empty: bool, out full: bool,
) { … }

inst q = fifo<T: uint<8>, DEPTH: 64>(push: p, data: d, pop: r);
inst tick = divider<25_000_000>();   // one parameter: the name is optional
```

Generic parameters have kind `int`, `bool`, `string` or `type`, and may have a
default. Arguments are given by name and must be compile-time constants. A
module or function with a single generic parameter may take its argument
without the name: `divider<25_000_000>()` is `divider<N: 25_000_000>()`.

Generic modules and functions are checked for each set of arguments they are
used with, as C++ templates are; errors show the instantiation chain. Items
without generic parameters, or whose parameters all have defaults, are also
checked on their own, even when unused.

Inside `<…>`, comparison and shift operators must be parenthesized, so that a
`>` always closes the list: `uint<(W >> 1)>`.

### 13.2 Constant expressions

Constant expressions use literals, `const`s, generic parameters, loop variables,
`.width`, and these builtins on `int`:

| Builtin | Value |
|---|---|
| `clog2(n)` | Smallest k with 2^k ≥ n (`clog2(1) == 0`) |
| `bits_for(v)` | Width of `uint` needed to hold v (≥ 1): `bits_for(255) == 8`, `bits_for(256) == 9` |
| `min(a, b)`, `max(a, b)` | |
| `a ** b` | Power, b ≥ 0 |

Any function called with constant arguments is evaluated at compile time. This
computes tables:

```weft
fn squares() -> [uint<16>; 256] {
    var t: [uint<16>; 256] = [0; 256];
    for i in 0..256 { t[i] = i * i; }
    t
}
mem rom: [uint<16>; 256] init squares();
```

### 13.3 Compile-time `if` and `for`

An `if` whose condition is a compile-time constant is resolved during
elaboration: only the taken branch is elaborated and checked, and it may contain
declarations. This is Verilog's `generate if`.

`for i in a..b` (exclusive) and `for i in a..=b` (inclusive) unroll at compile
time. `i` is an `int` constant in each iteration.

```weft
if WITH_PARITY {
    reg p: bool = false;
    p := data.parity();
    parity_out = p;
} else {
    parity_out = false;
}
```

`assert(cond, "message")` fails elaboration when `cond` is false:

```weft
assert(DEPTH >= 2 && DEPTH <= 4096, "fifo depth must be in 2..=4096");
```

---

## 14. The top module

Weft sources are recognized by the `.weft` extension. Verilog files on the same
command line provide the definitions of extern modules (§11.3). The top module is
chosen with `--top`, or is the only module that no other module instantiates
and that can be instantiated with the given `-P` arguments (a generic module
whose parameters lack defaults is not a candidate).

- Its ports are the design's pins. Port types are `bool`, `uint<N>`, `sint<N>`,
  `clock` and `inout`. Bit i of a port `led: uint<4>` is named `led[i]` in
  constraint files, as in Verilog.
- **Its non-clock inputs are `@async` unless annotated.** A pin's timing relative
  to any internal clock is unknown, so the compiler requires synchronization
  before an input reaches a register. A bus that the board drives synchronously
  (an external SRAM sampled at a known phase) is declared `@clk`.
- Unannotated outputs belong to the default domain, or are `@async` if the module
  has none.
- Generic parameters must have defaults or be set with `-P name=value`.
- If the top module has an implicit domain parameter (no clock input), that
  domain has no clock and cannot hold registers.

---

## 15. Semantics

A design is a synchronous circuit. Its behaviour is defined as follows.

**State.** The state is the value of every register and the contents of every
memory.

**Power-on.** Every register holds its reset value or `init` value. Every memory
holds its initial contents.

**Combinational values.** At any instant, every wire, `let`, port and expression
has the value determined by the current state and the current inputs. This is
well defined because there are no combinational cycles. A signal's value is the
value of the last assignment that applies (§7.5).

**Clock edges.** At each active edge of a clock C, every register in a domain of
C, with that edge:

1. takes its reset value, if the domain has a synchronous reset that is asserted
   and the register has a reset value;
2. otherwise takes the value of the last applicable `:=`, if any;
3. otherwise keeps its value.

All of these updates use values from before the edge. Memory writes of C apply
in program order. Memory reads see contents before the edge.

**Asynchronous reset.** While a domain's asynchronous reset is asserted,
registers with a reset value hold it.

**Clocks.** Clocks of different sources are unrelated. Values in different clock
tags interact only through `sync`, `reset_sync`, dual-clock memories and
`unsafe_cross`.

**Unspecified behaviour** occurs only:

- through `undef` values (§6.10);
- through `unsafe_cross`, when the crossed value changes near the sampling edge;
- in the exact cycle in which a synchronizer's output follows an input that
  changed near a clock edge (§18);
- inside extern modules.

This list is exhaustive.

---

## 16. Static checks

**Errors** (compilation stops):

- Type errors: non-`bool` conditions, implicit narrowing, operators on the wrong
  types, mismatched bitwise widths, wrap types that differ.
- Out-of-range constant values and indices; runtime indices whose range exceeds
  the target.
- Incomplete assignment of wires, outputs and instance inputs; assignments to
  inputs or instance outputs; `=` on a register or `:=` on a wire.
- Combinational cycles.
- Non-exhaustive `match`; unreachable arms.
- Tag errors: domain mixing at sinks, unsynchronized asynchronous values, reset
  expressions in the wrong clock, resets that depend on registers they reset.
- Mixing `&&`/`||`, or different bitwise operators, without parentheses.
- Missing instance connections; unknown ports; positional arguments to modules
  with more than one input.
- Shadowing; duplicate names.
- Failed `assert`.

**Warnings:**

- Comparisons that are always true or always false given the operand ranges
  (`x < 256` with `x: uint<8>`).
- Dead assignments (§7.6), and `r := r;`, which has no effect.
- `_` arms that cover enum variants.
- Unused registers, wires, `let` values, memories and module inputs; instances
  none of whose outputs are used; registers that are never assigned.
- Constant shifts that move every bit out.
- Memories that are never written and have no initial contents.
- Combinational reads of memories of 64 words or more, which cannot use block
  RAM.

Names starting with `_` are exempt from the unused-name warnings. Warnings are
not reported for a module that has errors, and never for the standard library.
When an asynchronous or foreign-domain value reaches several sinks, one error
is reported for the source, with the other sinks as notes.

Diagnostics follow synth's usual format (source excerpt, labels, notes and
`help:` suggestions). Every error in the table of §1 has a suggestion that names
the fix: `+%` or `.trunc()` for widths, `sync()` for asynchronous inputs, the
missing path for incomplete assignments, the missing variants for `match`.

---

## 17. Lowering to the RTL IR

Elaboration starts at the top module, instantiates generics per argument set, and
flattens the hierarchy into one `synth_rtl::Module`. Cells are built with the
`Module` builder methods (`build.rs`), which fold constants as they go.

| Weft | RTL IR |
|---|---|
| `bool`, integers | `Sig` of the natural width; `Wire.signed` for `sint` |
| `index<N>` | `Sig` of `max(1, clog2(N))` bits |
| enum | binary encoding in declaration order (or the declared encoding), width `max(1, clog2(variants))` |
| struct, array | concatenation of fields (first field highest) or elements (element 0 lowest) |
| exact arithmetic | operands extended (zero or sign, per their range) to the result width, then `Binary`/`Unary` cells, as the IR expects; comparisons with mixed signedness extend both operands to a common signed width |
| `+%`, `-%`, `*%` on `uint`/`sint` | the cell at the wrap type's width |
| `+%`, `-%` on `index<N>` | sum, then compare with N and subtract (a `Mux`) |
| `/`, `%` by a constant | `Div`/`Mod` (the constant divisor lets synthesis simplify) |
| shifts | `Shl`, `Shr`, `Sshr` |
| constant bit, field and slice selects | `Sig` slicing, no cells |
| runtime bit read | `Shiftx` (in range by construction) |
| runtime element read | a `Pmux` over the elements the index can select |
| `if`, `match` on drivers | per assignment, a `Mux` on the assignment's guard, chained in program order; synthesis turns the chains into trees and one-hot selects where it pays |
| `undef` | `SigBit::X` |
| `reg` with `= v` | `Dff` with `clk` from the domain (`posedge` from `edge`), `srst` or `arst` from the domain reset with value `v`, `init` = `v` |
| `reg` with `init v` | `Dff` without resets, `init` = `v` |
| register hold paths | the `d` input muxed with `q`; `dff::extract_controls` turns the feedback into `en` |
| domain reset `!x` | `Control { sig: x, pol: false }` |
| `mem` | `Memory` with `init` from the initializer |
| `m[i] := v` | a `MemWrite` per assignment, in program order (later ports win), per-bit `en` from the guards and the written bits |
| `m[i]` | an asynchronous `MemRead`; `mem::merge_read_ffs` absorbs the register of a registered read into a synchronous port (read-first), or the address register (write-first) |
| native `inst` | inlined, with names prefixed by the instance path (`uart.state`) |
| extern Verilog module | elaborated by `synth-elab` with the generic arguments as parameter overrides, then inlined |
| device primitive | an `Instance` cell |
| `#[keep]`, `#[async_reg]`, `#[attr]` | `Wire.attrs` |

Public names follow the source: registers, wires and ports keep their names,
prefixed by the instance path, so reports, timing paths and the Verilog writer
(`synth elab --verilog`) show source names. Loop iterations add `[i]` to the
scope name. Each cell carries the source span it came from.

Because the source already states clock enables, resets and read-port behaviour,
the existing RTL passes find exactly what was written. The frontend does not
recognize patterns.

**Simulation.** Until synth has its own simulator, a Weft design is simulated by
writing the elaborated RTL as Verilog (`synth elab design.weft --verilog out.v`)
and using Icarus Verilog or Verilator. `undef` appears as `x` there.

---

## 18. Standard library

### 18.1 Modules and functions

The standard library is written in Weft and always available.

```weft
/// Two-flip-flop synchronizer for a single asynchronous bit. The output follows
/// the input after STAGES or STAGES + 1 clock cycles.
module sync<STAGES: int = 2>(in x: bool @async, out y: bool) {
    assert(STAGES >= 2, "a synchronizer needs at least two stages");
    #[async_reg] reg s: uint<STAGES> init 0;
    s := concat(s[STAGES - 2:0], unsafe_cross(x));
    y = s[STAGES - 1];
}

/// A reset that asserts as soon as `x` is high and releases synchronously,
/// STAGES cycles after `x` falls.
module reset_sync<STAGES: int = 2>(in x: bool @async, out rst: bool) {
    assert(STAGES >= 2, "a reset synchronizer needs at least two stages");
    domain r { clock: default.clock, async_reset: unsafe_cross(x) }
    #[async_reg] reg s: uint<STAGES> @r = 2 ** STAGES - 1;
    s := concat(s[STAGES - 2:0], false);
    rst = s[STAGES - 1];
}

/// High for the first CYCLES cycles after configuration.
module power_on_reset<CYCLES: int = 16>(out rst: bool) {
    reg count: uint<bits_for(CYCLES)> init 0;
    rst = count != CYCLES;
    if rst { count := count +% 1; }
}

/// One-cycle pulse when `x` goes from low to high.
module rising(in x: bool, out y: bool) {
    reg prev: bool init false;
    prev := x;
    y = x && !prev;
}

/// One-cycle pulse when `x` goes from high to low.
module falling(in x: bool, out y: bool) {
    reg prev: bool init false;
    prev := x;
    y = !x && prev;
}

/// Pulses `tick` once every N cycles.
module divider<N: int>(out tick: bool) {
    reg count: index<N> = 0;
    count := count +% 1;
    tick = count == N - 1;
}
```

Functions: `count_ones` (§12), `onehot_to_index<N>(x: uint<N>) -> index<N>`,
`priority<N>(x: uint<N>) -> index<N>` (lowest set bit).

### 18.2 Device primitives

Each `arch-*` crate ships the extern declarations of its primitives, generated
from the same primitive library that the Verilog frontend uses, with `clock`
types on clock pins and `@async` on asynchronous pins. On iCE40, the prelude
(`crates/arch-ice40/src/prims.weft`) declares the primitives the backend can
implement today: `SB_IO` (combinational modes), `SB_GB` and `SB_GB_IO`. PLLs
and oscillators are added when the backend supports them. Block RAMs and
flip-flops are not declared: `mem` and `reg` cover them.

Extern modules may have clock outputs (`out GLOBAL_BUFFER_OUTPUT: clock`). They
are clock values (`inst.port`) that can define domains.

---

## 19. Not in this version

These are left out to keep the language small. Each can be added later without
changing the meaning of existing programs.

- Enums with payloads (tagged unions) and tuples.
- Directional bundles (valid/ready interfaces with flipped fields).
- Generic structs; traits or interfaces.
- `match` guards.
- Runtime assertions, cover properties and formal verification.
- Testbench constructs and a native simulator (`synth sim`).
- Asynchronous FIFO and handshake crossings in the standard library.
- Saturating arithmetic; runtime division.
- Packages, imports and visibility.
- Latches, tristates in logic, DDR registers (use primitives).
- Pipelining and retiming annotations.

---

## Appendix A: grammar

EBNF. `{ x }` is zero or more, `[ x ]` is optional, `x | y` is a choice, and
`','` lists allow a trailing separator.

```ebnf
design      = { attribute } { item } ;
item        = { attribute } ( module | extern | function | struct | enum
                            | const_decl | assert_stmt ) ;
attribute   = "#[" ident [ "(" [ attr_arg { "," attr_arg } ] ")" ] "]" ;
attr_arg    = string | expr ;

module      = "module" ident [ generics ] "(" [ port { "," port } [ "," ] ] ")" block ;
extern      = "extern" "module" ident [ generics ]
              "(" [ ext_port { "," ext_port } [ "," ] ] ")" ";" ;
function    = "fn" ident [ generics ] "(" [ param { "," param } [ "," ] ] ")"
              "->" type fn_block ;
struct      = "struct" ident "{" field { "," field } [ "," ] "}" ;
field       = ident ":" type ;
enum        = "enum" ident [ ":" type ] "{" variant { "," variant } [ "," ] "}" ;
variant     = ident [ "=" expr ] ;
const_decl  = "const" ident [ ":" ( type | "int" | "string" ) ] "=" expr ";" ;

generics    = "<" generic { "," generic } [ "," ] ">" ;
generic     = ident ":" ( "int" | "bool" | "string" | "type" ) [ "=" expr ] ;
gen_args    = "<" gen_arg { "," gen_arg } [ "," ] ">" ;
gen_arg     = [ ident ":" ] ( type | expr ) ;   (* the name only when a single parameter *)

port        = { attribute }
              ( ( "in" | "out" ) ident ":" type [ dom ] [ "=" expr ]
              | "inout" ident ":" type
              | [ "default" ] "domain" ident ) ;
ext_port    = port | "domain" [ ident ] "{" binding { "," binding } [ "," ] "}" ;
binding     = ( "clock" | "reset" ) ":" [ "!" ] ident ;
param       = ident ":" type ;
dom         = "@" ( ident | "async" | "default" ) ;

type        = "bool" | "clock"
            | ( "uint" | "sint" | "index" ) "<" expr ">"
            | "[" type ";" expr "]"
            | ident ;                                  (* struct, enum, type parameter *)

block       = "{" { { attribute } stmt } "}" ;
stmt        = decl | assign | if_stmt | match_stmt | for_stmt | assert_stmt | block ;
arm_stmt    = assign | if_stmt | match_stmt | block ;
decl        = const_decl
            | "let" ident [ ":" type ] [ dom ] "=" expr ";"
            | "wire" ident ":" type [ dom ] ";"
            | "reg" ident ":" type [ dom ] ( "=" expr | "init" expr ) ";"
            | "mem" ident ":" "[" type ";" expr "]" [ dom ] [ "init" expr ] ";"
            | "inst" ident "=" call [ dom ] ";"
            | [ "default" ] "domain" ident "{" dom_field { "," dom_field } [ "," ] "}" ;
dom_field   = ( "clock" | "reset" | "async_reset" ) ":" expr
            | "edge" ":" ( "rising" | "falling" ) ;
assign      = target ( "=" | ":=" ) expr ";" ;
target      = ident { "." ident | "[" expr "]" | "[" expr ":" expr "]" } ;
if_stmt     = "if" expr block [ "else" ( if_stmt | block ) ] ;
match_stmt  = "match" expr "{" { pattern "=>" arm_stmt } "}" ;
for_stmt    = "for" ident "in" expr ( ".." | "..=" ) expr block ;
assert_stmt = "assert" "(" expr "," string ")" ";" ;

fn_block    = "{" { fn_stmt } expr "}" ;
fn_stmt     = const_decl
            | ( "let" | "var" ) ident [ ":" type ] "=" expr ";"
            | target "=" expr ";"
            | if_stmt | match_stmt | for_stmt | assert_stmt ;
            (* inside functions, if/match/for bodies contain fn_stmt *)

expr        = binary ;                                 (* precedence: §6.2 *)
unary       = [ "-" | "!" | "~" ] postfix ;
postfix     = primary { "." ident [ "(" args ")" ]
                      | "[" expr [ ( ":" | "+:" ) expr ] "]" } [ dom ] ;
primary     = literal | ident | "default" | "." ident | "undef" | "(" expr ")"
            | call | struct_lit | array_lit | if_expr | match_expr
            | ( "uint" | "sint" | "index" ) "<" expr ">" "(" expr ")" ;
call        = ident [ gen_args ] "(" [ args ] ")" ;
args        = arg { "," arg } [ "," ] ;
arg         = [ ident ":" ] expr ;
struct_lit  = ident "{" ident ":" expr { "," ident ":" expr } [ "," ] "}" ;
array_lit   = "[" expr ( ";" expr | { "," expr } [ "," ] ) "]" ;
if_expr     = "if" expr "{" { "let" ident [ ":" type ] "=" expr ";" } expr "}"
              "else" ( if_expr | "{" { "let" ident [ ":" type ] "=" expr ";" } expr "}" ) ;
match_expr  = "match" expr "{" pattern "=>" expr { "," pattern "=>" expr } [ "," ] "}" ;
pattern     = pat_atom { "|" pat_atom } ;
pat_atom    = [ "-" ] int_lit [ "..=" [ "-" ] int_lit ] | bit_pattern
            | "." ident | ident "." ident | "true" | "false" | "_" ;
literal     = int_lit | "true" | "false" | string ;
```

Struct literals are not allowed directly in `if`, `match` and `for` headers
(parenthesize them), as in Rust. A name followed by `<` starts generic
arguments when what follows parses as `gen_args` and is followed by `(`, or
when the next two tokens are an identifier and `:`; otherwise `<` is a
comparison. `uint`, `sint` and `index` are keywords, so `uint<8>(x)` is
unambiguous.

---

## Appendix B: Verilog to Weft

| Verilog | Weft |
|---|---|
| `input [7:0] a` | `in a: uint<8>` |
| `output reg [7:0] q` | `out q: uint<8>` plus `reg r: uint<8> = 0; q = r;` |
| `wire [7:0] w = a & b;` | `let w = a & b;` |
| `assign y = s ? a : b;` | `y = if s { a } else { b };` |
| `always @(posedge clk) q <= d;` | `reg q: T init 0; q := d;` |
| `always @(posedge clk) if (rst) q <= 0; else if (en) q <= d;` | `reg q: T = 0; if en { q := d; }` in a domain with `reset: rst` |
| `always @(posedge clk or posedge rst)` | `domain d { clock: clk, async_reset: rst }` |
| `always @* begin y = 0; if (c) y = 1; end` | `y = false; if c { y = true; }` |
| `case (s) … default: … endcase` | `match s { … }` |
| `casez (op) 4'b1??0: …` | `match op { 0b1??0 => … }` |
| `{a, b}` / `{4{x}}` | `concat(a, b)` / `x.replicate(4)` |
| `a[7:0]` / `a[i +: 4]` | `a[7:0]` / `a[i +: 4]` |
| `&a`, `\|a`, `^a` | `a.all()`, `a.any()`, `a.parity()` |
| `$signed(a)` | `a.as_sint()` |
| `cnt <= cnt + 1;` | `cnt := cnt +% 1;` |
| `parameter W = 8;` | `module m<W: int = 8>(…)` |
| `localparam N = 4;` | `const N: int = 4;` |
| `generate for` / `generate if` | `for` / `if` with constant bounds or condition |
| `reg [7:0] mem [0:255]; initial $readmemh("f.hex", mem);` | `mem m: [uint<8>; 256] init hex_file("f.hex");` |
| `reg [3:0] x = 0;` (power-on value) | `reg x: uint<4> init 0;` |
| `function` | `fn` |
| `` `define `` / `` `ifdef `` | `const` / compile-time `if` |
| `module m (…); … endmodule` instance `m u (.a(x), .b(y));` | `inst u = m(a: x, b: y);` |

---

## Appendix C: examples

### C.1 `tests/designs/blinky_ram`

The Verilog version feeds the asynchronous `btn` straight into logic. Weft
rejects that. This port keeps the Verilog behaviour by declaring the button
synchronous; a real design would write `let b = sync(btn);` instead.

```weft
/// Counter-driven LEDs through an initialized, writable block RAM.
module top(in clk: clock, in btn: bool @clk, out led: uint<8>, out tx: bool) {
    reg cnt: uint<24> init 0;
    mem ram: [uint<8>; 512] init ram_init();
    reg q: uint<8> init 0;

    cnt := cnt +% (if btn { 2 } else { 1 });
    q := ram[cnt[23:15]];
    if btn { ram[cnt[8:0]] := cnt[16:9]; }
    led = q ^ cnt[23:16];
    tx = cnt.parity();
}

fn ram_init() -> [uint<8>; 512] {
    var t: [uint<8>; 512] = [0; 512];
    for i in 0..512 { t[i] = (i * 7 + 3) % 256; }
    t
}
```

### C.2 `examples/go_board_serv`

```weft
extern module servant<memfile: string, memsize: int>(
    domain { clock: wb_clk, reset: wb_rst },
    out q: bool,
);

/// SERV (bit-serial RV32I) on the Nandland Go Board (iCE40 HX1K, 25 MHz):
/// prints a line over the USB UART (115200 8N1) once a second.
/// LED1 lights while a character is being sent; LED4 blinks.
module top(in clk: clock, out led: uint<4>, out tx: bool) {
    let por = power_on_reset<CYCLES: 16>() @clk;
    default domain sys { clock: clk, reset: por }

    inst soc = servant<memfile: "hello.hex", memsize: 4096>();
    reg beat: uint<25> init 0;
    beat := beat +% 1;

    tx = soc.q;
    led = concat(beat[24], 0b00, !soc.q);
}
```

### C.3 A synchronous FIFO

```weft
/// A standard (not first-word-fall-through) FIFO: `pop` takes the oldest word,
/// which appears on `head` in the next cycle, as with a block RAM read.
module fifo<T: type, DEPTH: int = 16>(
    in push: bool, in data: T,
    in pop: bool,
    out head: T, out empty: bool, out full: bool,
) {
    assert(DEPTH >= 2, "fifo depth must be at least 2");
    mem buf: [T; DEPTH];
    reg wr: index<DEPTH> = 0;
    reg rd: index<DEPTH> = 0;
    reg count: uint<bits_for(DEPTH)> = 0;
    reg out_q: T init from_bits(0);

    empty = count == 0;
    full = count == DEPTH;
    let do_push = push && !full;
    let do_pop = pop && !empty;

    if do_push {
        buf[wr] := data;
        wr := wr +% 1;
    }
    if do_pop {
        out_q := buf[rd];
        rd := rd +% 1;
    }
    if do_push && !do_pop { count := count +% 1; }
    if do_pop && !do_push { count := count -% 1; }
    head = out_q;
}
```

`reg out_q: T init from_bits(0)` gives a power-on value for any `T`:
`from_bits(0)` builds the all-zero value of the expected type.

### C.4 A UART receiver with a synchronized input

```weft
enum RxState { Idle, Start, Data, Stop }

/// 8N1 receiver. `valid` pulses for one cycle with each correctly framed byte.
module uart_rx<CLKS_PER_BIT: int>(in rx: bool @async, out byte: uint<8>, out valid: bool) {
    let line = sync(rx);

    reg state: RxState = .Idle;
    reg timer: index<CLKS_PER_BIT> = 0;
    reg bit: index<8> = 0;
    reg shift: uint<8> = 0;
    reg done: bool = false;

    let mid = timer == CLKS_PER_BIT / 2;
    let end = timer == CLKS_PER_BIT - 1;
    done := false;
    timer := timer +% 1;

    match state {
        .Idle => if !line { state := .Start; timer := 0; }
        .Start => if mid {
            state := if line { .Idle } else { .Data };
            timer := 0;
        }
        .Data => if end {
            shift := concat(line, shift[7:1]);
            bit := bit +% 1;
            if bit == 7 { state := .Stop; }
        }
        .Stop => if end {
            done := line;
            state := .Idle;
        }
    }
    byte = shift;
    valid = done;
}
```

The timer runs freely and wraps at CLKS_PER_BIT (an `index` counter). It is
cleared at the falling edge of the start bit, so `mid` falls in the middle of the
start bit and `end` in the middle of each data bit.
