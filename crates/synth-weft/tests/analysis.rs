//! The editor index: references, declarations and hover text.

use synth_core::SourceMap;

const SRC: &str = "\
enum Mode { idle, run }

fn inc(x: uint<4>) -> uint<4> {
    x +% 1
}

module counter(in en: bool, out q: uint<4>) {
    reg count: uint<4> = 0;
    reg mode: Mode = Mode.idle;
    if en {
        count := inc(count);
        mode := Mode.run;
    }
    q = count;
}

module top(in clk: clock, in btn: bool, out led: uint<4>) {
    let b = sync(btn);
    inst c = counter(en: b);
    led = c.q;
}

module spare(in a: bool, out y: bool) {
    y = !a;
}
";

fn find(src: &str, needle: &str, nth: usize) -> u32 {
    src.match_indices(needle).nth(nth).map(|(i, _)| i as u32).unwrap()
}

#[test]
fn index() {
    let mut sm = SourceMap::new();
    let f = sm.add("t.weft", SRC);
    let a = synth_weft::analyze(&mut sm, &[f], &[], None, &Default::default());
    assert!(!a.diags.has_errors(), "{}", a.diags.render(&sm));
    let at = |pos: u32| {
        a.refs.iter().find(|r| r.span.file == f && r.span.start <= pos && pos < r.span.end)
    };
    // `count` in `inc(count)` points at the register.
    let r = at(find(SRC, "(count)", 0) + 1).expect("use of count");
    assert_eq!(r.decl.map(|d| d.start), Some(find(SRC, "count:", 0)));
    // Assignment targets too.
    let r = at(find(SRC, "count :=", 0)).unwrap();
    assert_eq!(r.decl.map(|d| d.start), Some(find(SRC, "count:", 0)));
    assert!(r.hover.contains("reg count: uint<4> = 0"), "{}", r.hover);
    // Function call.
    let r = at(find(SRC, "inc(count)", 0)).unwrap();
    assert_eq!(r.decl.map(|d| d.start), Some(find(SRC, "inc", 0)));
    assert!(r.hover.contains("fn inc(x: uint<4>) -> uint<4>"), "{}", r.hover);
    // Variant and type.
    let r = at(find(SRC, "run;", 0)).unwrap();
    assert_eq!(r.decl.map(|d| d.start), Some(find(SRC, "run", 0)));
    let r = at(find(SRC, "Mode =", 0)).unwrap();
    assert!(r.hover.contains("enum Mode"), "{}", r.hover);
    // Instance ports, named connections and module names.
    let r = at(find(SRC, "q;", 0)).unwrap();
    assert_eq!(r.decl.map(|d| d.start), Some(find(SRC, "q: uint", 0)));
    let r = at(find(SRC, "en: b", 0)).unwrap();
    assert_eq!(r.decl.map(|d| d.start), Some(find(SRC, "en: bool", 0)));
    let r = at(find(SRC, "counter(en", 0)).unwrap();
    assert!(r.hover.contains("module counter("), "{}", r.hover);
    // `let` uses.
    let r = at(find(SRC, "b);", 0)).unwrap();
    assert_eq!(r.decl.map(|d| d.start), Some(find(SRC, "b = sync", 0)));
    // A module the top does not use is still indexed.
    let r = at(find(SRC, "!a", 0) + 1).unwrap();
    assert_eq!(r.decl.map(|d| d.start), Some(find(SRC, "a: bool", 0)));
}

#[test]
fn no_top_needed() {
    let src = "module a(in x: bool, out y: bool) { y = x; }\nmodule b(in x: bool, out y: bool) { y = z; }\n";
    let mut sm = SourceMap::new();
    let f = sm.add("t.weft", src);
    let a = synth_weft::analyze(&mut sm, &[f], &[], None, &Default::default());
    let text = a.diags.render(&sm);
    assert!(text.contains("`z` is not declared"), "{text}");
    assert!(!text.contains("top module"), "{text}");
}
