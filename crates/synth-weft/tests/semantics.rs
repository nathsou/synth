//! Behaviour of compiled designs, checked by simulating the RTL.

mod common;

use common::{Sim, ok};

#[test]
fn exact_arithmetic_and_mixed_signedness() {
    let m = ok(r#"
module top(in a: uint<3> @async, in b: sint<3> @async,
           out sum: sint<5>, out diff: sint<5>, out prod: sint<7>,
           out lt: bool, out q: uint<2>, out r: uint<2>, out neg: sint<4>) {
    sum = a + b;
    diff = b - a;
    prod = a * b;
    lt = a < b;
    q = a / 3;
    r = a % 3;
    neg = -a;
}
"#);
    let mut s = Sim::new(m);
    for a in 0..8i64 {
        for b in -4..4i64 {
            s.poke("a", a as u64);
            s.poke("b", (b & 7) as u64);
            let sx = |v: u64, w: u32| ((v << (64 - w)) as i64) >> (64 - w);
            assert_eq!(sx(s.peek("sum").unwrap(), 5), a + b, "{a} + {b}");
            assert_eq!(sx(s.peek("diff").unwrap(), 5), b - a);
            assert_eq!(sx(s.peek("prod").unwrap(), 7), a * b);
            assert_eq!(s.peek("lt").unwrap() == 1, a < b, "{a} < {b}");
            assert_eq!(s.peek("q").unwrap() as i64, a / 3);
            assert_eq!(s.peek("r").unwrap() as i64, a % 3);
            assert_eq!(sx(s.peek("neg").unwrap(), 4), -a);
        }
    }
}

#[test]
fn wrapping_and_index_counters() {
    let m = ok(r#"
module top(in clk: clock, out c8: uint<8>, out c5: uint<3>, out down: uint<3>) {
    reg a: uint<8> = 250;
    reg b: index<5> = 0;
    reg c: index<5> = 0;
    a := a +% 3;
    b := b +% 1;
    c := c -% 1;
    c8 = a;
    c5 = b;
    down = c;
}
"#);
    let mut s = Sim::new(m);
    let mut a = 250u64;
    for i in 0..20u64 {
        assert_eq!(s.peek("c8"), Some(a));
        assert_eq!(s.peek("c5"), Some(i % 5));
        assert_eq!(s.peek("down"), Some((5 - i % 5) % 5));
        s.step();
        a = (a + 3) % 256;
    }
}

#[test]
fn last_assignment_wins_and_enables() {
    let m = ok(r#"
module top(in clk: clock, in en: bool @clk, in sel: uint<2> @clk, out y: uint<4>, out held: uint<4>) {
    reg r: uint<4> = 0;
    y = 1;
    if sel == 2 { y = 2; }
    if sel == 3 { y = 3; }
    if en { r := r +% 1; }
    held = r;
}
"#);
    let mut s = Sim::new(m);
    for sel in 0..4 {
        s.poke("sel", sel);
        assert_eq!(s.peek("y"), Some(if sel >= 2 { sel } else { 1 }));
    }
    s.poke("en", 1);
    s.step();
    s.step();
    s.poke("en", 0);
    s.step();
    assert_eq!(s.peek("held"), Some(2));
}

#[test]
fn match_priority_patterns_and_narrowing() {
    let m = ok(r#"
module top(in op: uint<4> @async, out cls: uint<2>, out sat: uint<4>) {
    cls = match op {
        0 => 0,
        0b1??1 => 1,
        1..=7 => 2,
        _ => 3,
    };
    sat = if op < 15 { op + 1 } else { op };
}
"#);
    let mut s = Sim::new(m);
    for op in 0..16u64 {
        s.poke("op", op);
        let want = if op == 0 {
            0
        } else if op & 0b1001 == 0b1001 {
            1
        } else if op <= 7 {
            2
        } else {
            3
        };
        assert_eq!(s.peek("cls"), Some(want), "op = {op}");
        assert_eq!(s.peek("sat"), Some((op + 1).min(15)));
    }
}

#[test]
fn uart_transmitter_sends_a_byte() {
    let m = ok(r#"
enum TxState { Idle, Start, Data, Stop }

module uart_tx(in tick: bool, in start: bool, in byte: uint<8>, out tx: bool, out busy: bool) {
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

module top(in clk: clock, in go: bool @clk, out tx: bool, out busy: bool) {
    inst baud = divider<N: 4>();
    inst u = uart_tx(tick: baud.tick, start: go, byte: 0xA5);
    tx = u.tx;
    busy = u.busy;
}
"#);
    let mut s = Sim::new(m);
    s.poke("go", 1);
    s.step();
    s.poke("go", 0);
    let mut line = Vec::new();
    for _ in 0..60 {
        line.push(s.peek("tx").unwrap());
        s.step();
    }
    // Decode: wait for the start bit, then sample the middle of each bit.
    let start = line.iter().position(|&b| b == 0).unwrap();
    let mut byte = 0;
    for k in 0..8 {
        byte |= line[start + 4 * (k + 1) + 1] << k;
    }
    assert_eq!(byte, 0xA5, "line: {line:?}");
    assert_eq!(line[start + 4 * 9 + 1], 1, "stop bit");
}

#[test]
fn generic_fifo_with_struct_elements() {
    let m = ok(r#"
struct Pair { hi: uint<4>, lo: uint<4> }

module fifo<T: type, DEPTH: int = 16>(in push: bool, in data: T, in pop: bool,
                                     out head: T, out empty: bool, out full: bool) {
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

module top(in clk: clock, in push: bool @clk, in v: uint<8> @clk, in pop: bool @clk,
           out head: uint<8>, out empty: bool, out full: bool) {
    inst q = fifo<T: Pair, DEPTH: 3>(push: push, data: from_bits(v), pop: pop);
    head = q.head.bits();
    empty = q.empty;
    full = q.full;
}
"#);
    let mut s = Sim::new(m);
    s.poke("pop", 0);
    assert_eq!(s.peek("empty"), Some(1));
    s.poke("push", 1);
    for v in [0x12, 0x34, 0x56, 0x78] {
        s.poke("v", v);
        s.step();
    }
    s.poke("push", 0);
    assert_eq!(s.peek("full"), Some(1));
    s.poke("pop", 1);
    let mut got = Vec::new();
    for _ in 0..3 {
        s.step();
        got.push(s.peek("head").unwrap());
    }
    assert_eq!(got, vec![0x12, 0x34, 0x56]);
    assert_eq!(s.peek("empty"), Some(1));
}

#[test]
fn functions_with_vars_and_inference() {
    let m = ok(r#"
fn ones<N: int>(x: uint<N>) -> uint<bits_for(N)> {
    var n: uint<bits_for(N)> = 0;
    for i in 0..N {
        if x[i] { n = n +% 1; }
    }
    n
}

fn table() -> [uint<8>; 4] {
    var t: [uint<8>; 4] = [0; 4];
    for i in 0..4 { t[i] = i * i + 1; }
    t
}

module top(in x: uint<6> @async, in k: uint<2> @async, out n: uint<3>, out low: uint<3>, out sq: uint<8>) {
    n = ones(x);
    low = priority(x);
    sq = table()[k];
}
"#);
    let mut s = Sim::new(m);
    for x in 0..64u64 {
        s.poke("x", x);
        assert_eq!(s.peek("n"), Some(x.count_ones() as u64));
        let low = if x == 0 { 0 } else { x.trailing_zeros() as u64 };
        assert_eq!(s.peek("low"), Some(low), "x = {x:b}");
    }
    for k in 0..4 {
        s.poke("k", k);
        assert_eq!(s.peek("sq"), Some(k * k + 1));
    }
}

#[test]
fn registered_memory_reads_take_a_cycle() {
    let m = ok(r#"
fn contents() -> [uint<8>; 16] {
    var t: [uint<8>; 16] = [0; 16];
    for i in 0..16 { t[i] = 100 + i; }
    t
}

module top(in clk: clock, in we: bool @clk, in addr: uint<4> @clk, in wdata: uint<8> @clk, out q: uint<8>) {
    mem ram: [uint<8>; 16] init contents();
    reg r: uint<8> init 0;
    r := ram[addr];
    if we { ram[addr] := wdata; }
    q = r;
}
"#);
    let mut s = Sim::new(m);
    s.poke("addr", 3);
    assert_eq!(s.peek("q"), Some(0));
    s.step();
    assert_eq!(s.peek("q"), Some(103));
    s.poke("we", 1);
    s.poke("wdata", 7);
    s.step();
    // Read-first: the old contents.
    assert_eq!(s.peek("q"), Some(103));
    s.poke("we", 0);
    s.step();
    assert_eq!(s.peek("q"), Some(7));
}

#[test]
fn reset_styles() {
    let m = ok(r#"
module top(in clk: clock, in rst: bool @clk, out a: uint<4>, out b: uint<4>, out c: uint<4>) {
    domain s { clock: clk, reset: rst }
    domain r { clock: clk, async_reset: rst }
    reg x: uint<4> @s = 5;
    reg y: uint<4> @r = 5;
    reg z: uint<4> @s init 5;
    x := x +% 1;
    y := y +% 1;
    z := z +% 1;
    a = x;
    b = y;
    c = z;
}
"#);
    let mut s = Sim::new(m);
    s.step();
    s.step();
    assert_eq!((s.peek("a"), s.peek("b"), s.peek("c")), (Some(7), Some(7), Some(7)));
    // Asynchronous reset acts at once, synchronous at the next edge.
    s.poke("rst", 1);
    assert_eq!((s.peek("a"), s.peek("b")), (Some(7), Some(5)));
    s.step();
    assert_eq!((s.peek("a"), s.peek("b"), s.peek("c")), (Some(5), Some(5), Some(8)));
}

#[test]
fn power_on_reset_and_synchronizer() {
    let m = ok(r#"
module top(in clk: clock, in btn: bool, out rst: bool, out pressed: bool) {
    let por = power_on_reset<CYCLES: 3>() @clk;
    default domain sys { clock: clk, reset: por }
    rst = por;
    let p = rising(sync(btn));
    pressed = p;
}
"#);
    let mut s = Sim::new(m);
    s.poke("btn", 0);
    let mut r = Vec::new();
    for _ in 0..5 {
        r.push(s.peek("rst").unwrap());
        s.step();
    }
    assert_eq!(r, vec![1, 1, 1, 0, 0]);
    s.poke("btn", 1);
    let mut p = Vec::new();
    for _ in 0..5 {
        s.step();
        p.push(s.peek("pressed").unwrap());
    }
    assert_eq!(p, vec![0, 1, 0, 0, 0], "two synchronizer stages, then one pulse");
}

#[test]
fn structs_arrays_and_dynamic_writes() {
    let m = ok(r#"
struct Pixel { r: uint<5>, g: uint<6>, b: uint<5> }

module top(in clk: clock, in i: uint<2> @clk, in v: uint<8> @clk, out word: uint<16>, out sel: uint<8>) {
    reg regs: [uint<8>; 4] = [1, 2, 3, 4];
    regs[i] := v;
    sel = regs[i];
    let p = Pixel { r: 31, g: 0, b: 1 };
    word = p.bits();
}
"#);
    let mut s = Sim::new(m);
    assert_eq!(s.peek("word"), Some((31 << 11) | 1));
    s.poke("i", 2);
    assert_eq!(s.peek("sel"), Some(3));
    s.poke("v", 99);
    s.step();
    assert_eq!(s.peek("sel"), Some(99));
    s.poke("i", 1);
    assert_eq!(s.peek("sel"), Some(2));
}

#[test]
fn generate_loops_and_constant_conditions() {
    let m = ok(r#"
module stage(in x: uint<4>, out y: uint<4>) {
    y = x +% 1;
}

module top<N: int = 3, PARITY: bool = true>(in x: uint<4> @async, out y: uint<4>, out p: bool) {
    wire w: [uint<4>; N + 1];
    w[0] = x;
    for i in 0..N {
        inst s = stage(x: w[i]);
        w[i + 1] = s.y;
    }
    y = w[N];
    if PARITY {
        p = y.parity();
    } else {
        p = false;
    }
}
"#);
    let mut s = Sim::new(m);
    s.poke("x", 14);
    assert_eq!(s.peek("y"), Some(1));
    assert_eq!(s.peek("p"), Some(1));
}
