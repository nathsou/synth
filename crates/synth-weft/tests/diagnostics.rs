//! Each error class and lint: the message, and the fix it suggests.

mod common;

use common::{errors, warnings};

fn has(out: &str, parts: &[&str]) {
    for p in parts {
        assert!(out.contains(p), "missing `{p}` in:\n{out}");
    }
}

#[test]
fn width_overflow_suggests_wrapping() {
    let e = errors(
        "module top(in clk: clock, out led: bool) { reg c: uint<4> = 0; c := c + 1; led = c[3]; }",
    );
    has(
        &e,
        &[
            "`c + 1` does not fit register `c` (uint<4>)",
            "has values 1..=16",
            "use `+%`: `c +% 1`",
            "`(c + 1).trunc(4)`",
        ],
    );
    assert!(!e.contains("never assigned"), "no lint noise next to an error:\n{e}");
}

#[test]
fn negative_values_into_unsigned() {
    let e = errors("module top(in a: uint<4> @async, out y: uint<4>) { y = a - 1; }");
    has(&e, &["may be negative", "-1..=14", "`-%`"]);
}

#[test]
fn unsynchronized_input() {
    let e = errors(
        "module top(in clk: clock, in btn: bool, out led: bool) { reg p: bool init false; p := btn; led = btn && !p; }",
    );
    has(
        &e,
        &[
            "an asynchronous value reaches register `p`",
            "an asynchronous top-level input",
            "`sync(btn)`",
            "in btn: ... @clk",
            "it also reaches output `led`",
        ],
    );
    assert_eq!(e.matches("error:").count(), 1, "one error per source:\n{e}");
}

#[test]
fn clock_domain_crossing() {
    let e = errors(
        "module top(in a: clock, in b: clock, in d: uint<8> @a, out q: uint<8> @b) { reg r: uint<8> @b = 0; r := d; q = r; }",
    );
    has(
        &e,
        &[
            "register `r` (in domain `b`) mixes clock domains",
            "`d` (in domain `a`)",
            "unsafe_cross",
        ],
    );
}

#[test]
fn crossing_through_a_wire() {
    let e = errors(
        "module top(in a: clock, in b: clock, in d: bool @a, out q: bool @b) { wire w: bool; w = d; reg r: bool @b = false; r := w; q = r; }",
    );
    has(&e, &["mixes clock domains", "`d` (in domain `a`)"]);
}

#[test]
fn synchronizer_fixes_it() {
    warnings(
        "module top(in clk: clock, in btn: bool, out led: bool) { let b = sync(btn); reg p: bool init false; p := b; led = b && !p; }",
    );
}

#[test]
fn incomplete_assignment_names_the_path() {
    let e = errors(
        "module top(in clk: clock, in s: bool @clk, out busy: bool) { if s { busy = true; } }",
    );
    has(
        &e,
        &[
            "output `busy` is not assigned on every path",
            "not assigned when `s` is false",
            "assign a default first",
            "latch",
        ],
    );
}

#[test]
fn incomplete_match_arm() {
    let e = errors(
        "enum E { A, B } module top(in clk: clock, out y: bool) { reg e: E = .A; e := .B; match e { .A => y = true; .B => {} } }",
    );
    has(&e, &["not assigned in the arm `.B`"]);
}

#[test]
fn non_exhaustive_match() {
    let e = errors(
        "enum S { Idle, Run, Stop } module top(in clk: clock, out y: bool) { reg s: S = .Idle; s := .Run; y = match s { .Idle => true, .Run => false }; }",
    );
    has(&e, &["does not handle variant `.Stop`", "`S` is defined here"]);
    let e = errors(
        "module top(in x: uint<2> @async, out y: uint<4>) { y = match x { 0 => 1, 1 => 2, 2 => 3 }; }",
    );
    has(&e, &["`match` is not exhaustive: `x` may be 3"]);
    let e = errors(
        "module top(in x: uint<3> @async, out y: uint<4>) { y = match x { 0b1?? => 1, 0b11? => 2, _ => 0 }; }",
    );
    has(&e, &["this arm can never match"]);
}

#[test]
fn wildcard_over_enum_variants_warns() {
    let w = warnings(
        "enum S { A, B, C } module top(in clk: clock, out y: bool) { reg s: S = .A; s := .B; y = match s { .A => true, _ => false }; }",
    );
    has(&w, &["`_` here stands for `.B`, `.C`"]);
}

#[test]
fn bit_pattern_width() {
    let e = errors(
        "module top(in x: uint<4> @async, out y: bool) { y = match x { 0b1?? => true, _ => false }; }",
    );
    has(&e, &["has 3 digits, but `x` is 4 bits wide"]);
}

#[test]
fn index_range_checks_and_narrowing() {
    let e = errors(
        "module top(in clk: clock, in i: uint<4> @clk, out y: uint<8>) { mem m: [uint<8>; 10]; reg q: uint<8> init 0; q := m[i]; y = q; }",
    );
    has(
        &e,
        &[
            "index `i` may be out of range: memory `m` has 10 entries",
            "use an `index<10>` value",
            "if i < 10",
        ],
    );
    // Guarded accesses are accepted.
    warnings(
        "module top(in clk: clock, in i: uint<4> @clk, in w: uint<8> @clk, out y: uint<8>) { mem m: [uint<8>; 10]; reg q: uint<8> init 0; if i < 10 { q := m[i]; m[i] := w; } y = q; }",
    );
}

#[test]
fn bitwise_width_mismatch() {
    let e = errors(
        "module top(in a: uint<8> @async, in b: uint<4> @async, out y: uint<8>) { y = a & b; }",
    );
    has(
        &e,
        &["`&` operands have different widths: 8 and 4 bits", "8 bits", "4 bits", "`uint<8>(b)`"],
    );
}

#[test]
fn bool_is_not_an_integer() {
    let e = errors(
        "module top(in x: uint<8> @async, out y: bool) { y = if x { true } else { false }; }",
    );
    has(&e, &["expected `bool` for the condition, found uint<8>", "`x != 0`"]);
    let e = errors("module top(in x: uint<8> @async, out y: bool) { y = !x == 0; }");
    has(&e, &["`!` needs a `bool`", "`~x`"]);
}

#[test]
fn mixing_logical_and_bitwise_operators() {
    let e = errors(
        "module top(in a: bool @async, in b: bool @async, in c: bool @async, out y: bool) { y = a && b || c; }",
    );
    has(&e, &["`&&` and `||` are mixed without parentheses", "(a && b) || c"]);
    let e = errors("module top(in a: uint<2> @async, out y: bool) { y = 0 < a < 3; }");
    has(&e, &["comparisons cannot be chained"]);
}

#[test]
fn assignment_operator_matches_the_declaration() {
    let e = errors(
        "module top(in clk: clock, in x: uint<8> @clk, out y: uint<8>) { reg r: uint<8> = 0; r = x; y = r; }",
    );
    has(&e, &["`r` is a register: set its next value with `:=`", "declared as a register here"]);
    let e = errors("module top(in clk: clock, in x: uint<8> @clk, out y: uint<8>) { y := x; }");
    has(&e, &["`y` is combinational: assign it with `=`"]);
    let e = errors("module top(in x: uint<8> @async, out y: uint<8>) { let v = x; v = 3; y = v; }");
    has(&e, &["`v` cannot be assigned", "a `let` value", "wire v"]);
}

#[test]
fn verilog_habits() {
    has(
        &errors("module top(in clk: clock, out y: bool) { always @(posedge clk) y <= 1; }"),
        &["`always` is Verilog, not Weft"],
    );
    has(
        &errors(
            "module top(in clk: clock, out y: bool) { reg r: bool = false; r <= true; y = r; }",
        ),
        &["`<=` is a comparison, not an assignment", "`:=`"],
    );
    has(
        &errors("module top(out y: uint<8>) { y = 8'hFF; }"),
        &["Verilog-style sized literals are not supported"],
    );
    has(
        &errors("module top(in x: bool @async, out y: uint<8>) { y = x ? 1 : 2; }"),
        &["Weft has no `?:` operator"],
    );
    has(
        &errors("module top(in x: uint<8> @async, out y: uint<8>) { y = {x[3:0], x[7:4]}; }"),
        &["`concat(a, b, ...)`"],
    );
    has(&errors("module top(in x: uint<8> @async, out y: bool) { y = |x; }"), &["`x.any()`"]);
    has(&errors("module top(input x: bool, out y: bool) { y = x; }"), &["`input` is Verilog"]);
    has(
        &errors(
            "module top(in x: uint<3> @async, out y: uint<4>) { y = match x { 0 => 1, default => 0 }; }",
        ),
        &["`_ => ...`"],
    );
}

#[test]
fn reversed_and_out_of_range_slices() {
    let e = errors(
        "module top(in x: uint<8> @async, out y: uint<5>, out z: bool) { y = x[3:7]; z = x[9]; }",
    );
    has(&e, &["slices are written high to low", "write `[7:3]`", "bit 9 is out of range"]);
}

#[test]
fn concat_needs_widths() {
    let e = errors("module top(in x: uint<8> @async, out y: uint<4>) { y = concat(x[1:0], 2); }");
    has(&e, &["2 has no width in `concat`", "uint<2>(2)"]);
    warnings("module top(in x: uint<8> @async, out y: uint<4>) { y = concat(x[1:0], 0b10); }");
}

#[test]
fn wrap_type_must_be_clear() {
    let e = errors(
        "module top(in x: uint<8> @async, in y: uint<4> @async, out z: uint<8>) { z = x +% y; }",
    );
    has(&e, &["`+%` operands have different types: uint<8> and uint<4>", "`x +% uint<8>(y)`"]);
}

#[test]
fn enum_encoding_is_private() {
    let e = errors(
        "enum S { A, B } module top(in clk: clock, out y: uint<1>) { reg s: S = .A; s := .B; y = s.bits(); }",
    );
    has(&e, &["`S` has no bit representation", "enum S: uint<N>"]);
    warnings(
        "enum S: uint<2> { A = 1, B = 2 } module top(in clk: clock, out y: uint<2>) { reg s: S = .A; s := .B; y = s.bits(); }",
    );
}

#[test]
fn names_and_suggestions() {
    has(
        &errors("module top(in count: uint<8> @async, out y: uint<8>) { y = cout; }"),
        &["`cout` is not declared", "did you mean `count`?"],
    );
    has(
        &errors(
            "module sub(in a: uint<4>, out b: uint<4>) { b = a; } module top(in x: uint<4> @async, out y: uint<4>) { inst s = sub(aa: x); y = s.bb; }",
        ),
        &["`sub` has no port `aa`", "did you mean `a`?", "did you mean `s.b`?"],
    );
    has(
        &errors(
            "module top(in x: uint<8> @async, in y: uint<4> @async, out z: uint<8>) { let x = y; z = x; }",
        ),
        &["`x` is already declared as a port", "no shadowing"],
    );
    has(
        &errors(
            "enum S { Idle, Run } module top(in clk: clock, out y: bool) { reg s: S = .Idel; s := .Run; y = s == .Run; }",
        ),
        &["`S` has no variant `Idel`", "did you mean `.Idle`?"],
    );
}

#[test]
fn instances_must_be_fully_connected() {
    let e = errors(
        "module sub(in a: uint<4>, in b: uint<4>, out c: uint<4>) { c = a +% b; } module top(in x: uint<4> @async, out y: uint<4>) { inst s = sub(a: x); y = s.c; }",
    );
    has(&e, &["input `b` of instance `s` is never assigned", "instantiated here"]);
    let e = errors(
        "module sub(in a: uint<4>, in b: uint<4>, out c: uint<4>) { c = a +% b; } module top(in x: uint<4> @async, out y: uint<4>) { let t = sub(x, x); y = t; }",
    );
    has(&e, &["module ports are connected by name"]);
}

#[test]
fn module_calls_only_in_lets() {
    let e = errors("module top(in clk: clock, in b: bool, out y: bool) { y = rising(sync(b)); }");
    has(&e, &["`rising` is a module and cannot be called here", "let x = rising(...);"]);
}

#[test]
fn declarations_under_runtime_conditions() {
    let e = errors(
        "module top(in clk: clock, in x: uint<8> @clk, out y: uint<8>) { if x > 3 { reg r: uint<8> = 0; } y = x; }",
    );
    has(&e, &["declarations cannot depend on a runtime condition"]);
}

#[test]
fn registers_need_defined_start_values() {
    let e = errors("module top(in clk: clock, out y: uint<8>) { reg r: uint<8> = undef; y = r; }");
    has(&e, &["the reset value of `r` must be fully defined"]);
    let e = errors("module top(in clk: clock, out y: uint<8>) { reg r: uint<8>; y = r; }");
    has(&e, &["register `r` needs a reset or power-on value", "init 0"]);
}

#[test]
fn several_domains_need_annotations() {
    let e = errors(
        "module top(in a: clock, in b: clock, out y: bool) { reg r: bool = false; r := !r; y = r; }",
    );
    has(&e, &["register `r` needs a domain", "`@a`, `@b`", "mark one domain `default`"]);
}

#[test]
fn top_module_without_clock() {
    let e = errors("module top(out y: bool) { reg r: bool = false; r := !r; y = r; }");
    has(&e, &["the top module has no clock input", "in clk: clock"]);
}

#[test]
fn combinational_cycle() {
    let e =
        errors("module top(in a: bool @async, out y: bool) { wire w: bool; w = !y; y = w && a; }");
    has(&e, &["combinational cycle: `w` → `y` → `w`", "break the cycle with a register"]);
}

#[test]
fn domain_reset_from_its_own_register() {
    let e = errors(
        "module top(in clk: clock, out led: bool) { default domain sys { clock: clk, reset: x == 3 } reg x: uint<4> = 0; x := x +% 1; led = x[0]; }",
    );
    has(&e, &["`sys` depends on itself", "power_on_reset()"]);
}

#[test]
fn generic_arguments() {
    let e = errors("module top(in clk: clock, out t: bool) { inst d = divider(); t = d.tick; }");
    has(&e, &["missing generic argument `N` of `divider`"]);
    let e =
        errors("module top(in clk: clock, out t: bool) { inst d = divider<M: 3>(); t = d.tick; }");
    has(&e, &["`divider` has no generic parameter `M`", "did you mean `N`?"]);
    let e =
        errors("module top(in clk: clock, out t: bool) { inst d = divider<N: 0>(); t = d.tick; }");
    has(&e, &["assertion failed: the period is at least one cycle", "with N = 0"]);
}

#[test]
fn lints() {
    let w = warnings(
        "module top(in clk: clock, in x: uint<8> @clk, out y: uint<8>, out z: bool, out k: uint<8>) {
            let unused = x + 1;
            let _kept = x + 2;
            reg never: bool = false;
            reg r: uint<8> = 0;
            r := r;
            y = 0;
            y = x;
            z = never || x < 300;
            k = x << 8;
        }",
    );
    has(
        &w,
        &[
            "`unused` is never used",
            "register `never` is never assigned",
            "`r := r;` has no effect",
            "this assignment to `y` never takes effect",
            "this comparison is always true",
            "shifting a 8-bit value by 8 always gives 0",
        ],
    );
    assert!(!w.contains("_kept"), "names starting with `_` are not reported:\n{w}");
}

#[test]
fn memory_lints() {
    let w = warnings(
        "module top(in clk: clock, in a: uint<8> @clk, out y: uint<8>) { mem big: [uint<8>; 256]; y = big[a]; }",
    );
    has(
        &w,
        &["cannot use block RAM", "q := mem[addr];", "never written and has no initial contents"],
    );
}

#[test]
fn unused_instance() {
    let w =
        warnings("module top(in clk: clock, out y: bool) { inst d = divider<N: 4>(); y = true; }");
    has(&w, &["instance `d` has no effect"]);
}

#[test]
fn several_errors_are_reported_at_once() {
    let e = errors(
        "module top(in clk: clock, out y: uint<8>) {
            reg a: uint<8> = 0;
            a <= 1;
            let b = c;
            y = a
        }",
    );
    has(&e, &["`<=` is a comparison", "expected `;`"]);
}

#[test]
fn unnamed_generic_argument_with_several_parameters() {
    let out = errors(
        "module m<A: int, B: int>(out y: uint<8>) { y = A + B; }\nmodule top(out y: uint<8>) { inst u = m<1, B: 2>(); y = u.y; }\n",
    );
    has(&out, &["`m` has 2 generic parameters: name this argument", "write `A: ...`"]);
}

#[test]
fn compile_time_types_are_not_hardware_types() {
    let out = errors("fn f(x: int) -> uint<8> { x }\nmodule top(out y: uint<8>) { y = f(1); }\n");
    has(&out, &["`int` is a compile-time type, not a hardware type", "`fn f<N: int>(…)`"]);
    let out = errors("const N: int = \"eight\";\nmodule top(out y: uint<8>) { y = N; }\n");
    has(&out, &["constant `N` is declared an integer", "declared here"]);
}
