//! Finishing a module: drivers into logic, registers into flip-flops, tag
//! checks, completeness and lints.

use std::collections::HashMap;

use synth_core::Diagnostic;
use synth_rtl::{CellKind, Clock, Control, Dff, Sig, SigBit};

use super::*;

impl<'a> ModCx<'a> {
    pub fn finish(&mut self, d: &mut Design<'a>, errors_before: usize) -> R<()> {
        let mut ok = self.walk_failed || self.check_complete(d).is_ok();
        self.build_signals(d);
        for m in &self.mems {
            let id = m.id;
            let nw = d.out.memories[id].writes.len();
            for r in &mut d.out.memories[id].reads {
                r.transparent = vec![false; nw];
            }
        }
        ok &= self.check_tags(d).is_ok();
        // Lints are noise next to errors in the same module.
        if d.diags.error_count() == errors_before {
            self.lints(d);
        }
        if ok { Ok(()) } else { Err(()) }
    }

    fn check_complete(&mut self, d: &mut Design<'a>) -> R<()> {
        let mut ok = true;
        for (i, s) in self.signals.iter().enumerate() {
            let needs = match &s.kind {
                SigKind::Wire | SigKind::Out => true,
                SigKind::InstIn { default, .. } => default.is_none(),
                _ => false,
            };
            if !needs {
                continue;
            }
            let w = s.sig.width();
            let bits = self.top_defs.get(&i);
            let missing: Vec<usize> = (0..w).filter(|&b| !bits.is_some_and(|v| v[b])).collect();
            if missing.is_empty() {
                continue;
            }
            ok = false;
            let what = match &s.kind {
                SigKind::Out => format!("output `{}`", s.name),
                SigKind::InstIn { inst, .. } => format!(
                    "input `{}` of instance `{}`",
                    s.name.rsplit('.').next().unwrap_or(""),
                    self.insts[*inst].name
                ),
                _ => format!("wire `{}`", s.name),
            };
            let part = if missing.len() == w || w == 1 {
                String::new()
            } else if missing.len() == 1 {
                format!(" (bit {})", missing[0])
            } else {
                format!(" ({} of its {w} bits)", missing.len())
            };
            if !s.assigned {
                let mut dg = Diagnostic::error(s.span, format!("{what} is never assigned{part}"))
                    .with_label("needs a value");
                dg = match &s.kind {
                    SigKind::InstIn { inst, .. } => {
                        let iname = self.insts[*inst].name.clone();
                        let port = s.name.rsplit('.').next().unwrap_or("").to_string();
                        dg.with_secondary(self.insts[*inst].span, "instantiated here")
                            .with_help(format!("connect it, `{}(... {port}: value ...)`, or assign `{iname}.{port} = value;`", self.insts[*inst].module))
                    }
                    SigKind::Out => dg.with_help(format!("drive it: `{} = ...;`", s.name)),
                    _ => dg.with_help(format!("assign it (`{} = ...;`), or remove it", s.name)),
                };
                d.emit(dg);
                continue;
            }
            let mut dg =
                Diagnostic::error(s.span, format!("{what} is not assigned on every path{part}"))
                    .with_label("a combinational signal needs a value in every case")
                    .with_help(format!(
                        "assign a default first (`{} = ...;`) and override it where needed",
                        s.name.rsplit('.').next().unwrap_or(&s.name)
                    ));
            let mut shown = 0;
            for (si, sp, text) in self.partial.iter().rev() {
                if *si == i && shown < 2 {
                    dg = dg.with_secondary(*sp, text.clone());
                    shown += 1;
                }
            }
            dg = dg.with_note(None, "in Verilog this would infer a latch");
            d.emit(dg);
        }
        if ok { Ok(()) } else { Err(()) }
    }

    fn build_signals(&mut self, d: &mut Design<'a>) {
        let mut by_sig: HashMap<usize, Vec<usize>> = HashMap::new();
        for (k, dr) in self.drivers.iter().enumerate() {
            by_sig.entry(dr.sig).or_default().push(k);
        }
        for i in 0..self.signals.len() {
            let s = self.signals[i].clone();
            let base = match &s.kind {
                SigKind::Wire | SigKind::Out => Sig::xs(s.sig.width()),
                SigKind::InstIn { default, .. } => {
                    default.clone().unwrap_or_else(|| Sig::xs(s.sig.width()))
                }
                SigKind::Reg { .. } => s.sig.clone(),
                SigKind::In | SigKind::InstOut { .. } => continue,
            };
            let mut v = base;
            for &k in by_sig.get(&i).map(|v| v.as_slice()).unwrap_or(&[]) {
                let dr = &self.drivers[k];
                let (lo, w) = (dr.lo as usize, dr.width as usize);
                let old = v.slice(lo, w);
                let new = d.out.mux(dr.guard, old, dr.value.clone());
                for j in 0..w {
                    v.0[lo + j] = new.0[j];
                }
            }
            match &s.kind {
                SigKind::Reg { dom, has_reset, value } => {
                    let dm = &self.domains[*dom];
                    let ctl = dm
                        .reset
                        .filter(|_| *has_reset)
                        .map(|(b, asy)| (Control { sig: b, pol: true }, asy));
                    let (srst, arst) = match ctl {
                        Some((c, false)) => (Some((c, value.clone())), None),
                        Some((c, true)) => (None, Some((c, value.clone()))),
                        None => (None, None),
                    };
                    let dff = Dff {
                        clk: Clock { sig: dm.clock.bit, posedge: dm.posedge },
                        d: v,
                        q: s.sig.clone(),
                        en: None,
                        srst,
                        en_over_srst: false,
                        arst,
                        init: value.clone(),
                    };
                    let name = s.sig.0.first().map(|b| match b {
                        SigBit::Wire(w, _) => d.out.wires[*w].name.clone(),
                        _ => s.name.clone(),
                    });
                    d.out.add_cell(name.unwrap_or_default(), CellKind::Dff(Box::new(dff)));
                }
                _ => {
                    if by_sig.contains_key(&i) {
                        d.out.connect(s.sig.clone(), v);
                    }
                }
            }
        }
    }

    /// Resolves wire tags and checks every sink.
    fn check_tags(&mut self, d: &mut Design<'a>) -> R<()> {
        // Fixpoint: substitute wire tags until nothing changes.
        let mut resolved: HashMap<usize, Tags> = HashMap::new();
        loop {
            let mut changed = false;
            for (si, src) in &self.var_sources {
                let mut t = Tags::none();
                expand(src, &resolved, &mut t);
                let cur = resolved.entry(*si).or_default();
                let before = cur.0.len();
                cur.join(&t);
                if cur.0.len() != before {
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let mut ok = true;
        // One error per offending source; later sinks become notes.
        let mut by_origin: Vec<(String, Diagnostic)> = Vec::new();
        for sink in &self.sinks {
            let mut t = Tags::none();
            expand(&sink.tags, &resolved, &mut t);
            let bad = t.0.iter().find(|(a, _)| match a {
                TagAtom::Async => true,
                TagAtom::Clock(c) => *c != sink.clock,
                TagAtom::Var(_) => false,
            });
            let Some((atom, org)) = bad else { continue };
            ok = false;
            if let Some((_, dg)) = by_origin.iter_mut().find(|(s, _)| *s == org.what) {
                if dg.notes.len() < 3 {
                    dg.notes.push((None, format!("it also reaches {}", sink.target)));
                }
                continue;
            }
            let sink_dom = d.clock_names.get(&sink.clock).cloned().unwrap_or_default();
            let _ = sink_dom;
            let dg = match atom {
                TagAtom::Async => {
                    let mut dg = Diagnostic::error(
                        org.span,
                        format!("an asynchronous value reaches {}", sink.target),
                    )
                    .with_label(org.what.clone())
                    .with_help(format!(
                        "synchronize a single bit first: `sync({})`",
                        d.snippet(org.span)
                    ));
                    if org.what.contains("top-level input") {
                        let name = d.snippet(org.span);
                        let clk = self
                            .domains
                            .iter()
                            .find(|x| x.clock.id == sink.clock)
                            .map(|x| x.name.clone())
                            .unwrap_or_else(|| "clk".into());
                        dg = dg.with_help(format!("or, if the board drives it synchronously to the clock, declare it `in {name}: ... @{clk}`"));
                    }
                    dg.with_note(
                        None,
                        "sampling an asynchronous signal directly can make flip-flops metastable",
                    )
                }
                _ => Diagnostic::error(org.span, format!("{} mixes clock domains", sink.target))
                    .with_label(format!("{} is from another clock domain", org.what))
                    .with_help("cross single bits with `sync(...)`, a reset with `reset_sync(...)`")
                    .with_help(
                        "for a value that is stable when it is sampled, write `unsafe_cross(...)`",
                    ),
            };
            let dg = if sink.span.start > org.span.end || sink.span.end < org.span.start {
                dg.with_secondary(sink.span, "sampled here")
            } else {
                dg
            };
            by_origin.push((org.what.clone(), dg));
        }
        for (_, dg) in by_origin {
            d.emit(dg);
        }
        if ok { Ok(()) } else { Err(()) }
    }

    fn lints(&mut self, d: &mut Design<'a>) {
        let quiet = |n: &str| n.starts_with('_') || n.contains('$');
        for s in &self.signals {
            if quiet(&s.name) {
                continue;
            }
            match &s.kind {
                SigKind::Reg { .. } if !s.assigned => d.emit(
                    Diagnostic::warning(s.span, format!("register `{}` is never assigned", s.name))
                        .with_label("always holds its initial value")
                        .with_help("assign its next value with `:=`, or use a `const`"),
                ),
                SigKind::Reg { .. } | SigKind::Wire if !s.read => d.emit(
                    Diagnostic::warning(s.span, format!("`{}` is never read", s.name))
                        .with_label("unused")
                        .with_help(format!("remove it, or name it `_{}` to keep it", s.name)),
                ),
                SigKind::In if !s.read && !self.is_top => d.emit(
                    Diagnostic::warning(s.span, format!("input `{}` is never used", s.name))
                        .with_label("unused input")
                        .with_help(format!("remove it, or name it `_{}`", s.name)),
                ),
                _ => {}
            }
        }
        for (n, (sp, _)) in &self.lets_read {
            if !self.let_used.contains(n) && !quiet(n) {
                d.emit(
                    Diagnostic::warning(*sp, format!("`{n}` is never used"))
                        .with_label("unused value")
                        .with_help(format!("remove it, or name it `_{n}`")),
                );
            }
        }
        for inst in &self.insts {
            if inst.name.contains('$') || quiet(&inst.name) || !inst.has_outputs {
                continue;
            }
            let any = inst.ports.iter().any(|(_, s)| {
                matches!(self.signals[*s].kind, SigKind::InstOut { .. }) && self.signals[*s].read
            });
            if !any {
                d.emit(
                    Diagnostic::warning(
                        inst.span,
                        format!(
                            "instance `{}` has no effect: none of its outputs are used",
                            inst.name
                        ),
                    )
                    .with_label("unused instance")
                    .with_help("use one of its outputs, or remove it"),
                );
            }
        }
        for m in &self.mems {
            if quiet(&m.name) {
                continue;
            }
            if !m.read {
                d.emit(
                    Diagnostic::warning(m.span, format!("memory `{}` is never read", m.name))
                        .with_label("unused")
                        .with_help("remove it"),
                );
            } else if !m.written && !m.has_init {
                d.emit(
                    Diagnostic::warning(
                        m.span,
                        format!("memory `{}` is never written and has no initial contents", m.name),
                    )
                    .with_label("always reads 0")
                    .with_help(
                        "give its contents with `init`, or write it with `mem[addr] := value;`",
                    ),
                );
            }
            if m.depth >= 64
                && let Some(sp) = m.comb_reads.first()
            {
                d.emit(
                    Diagnostic::warning(*sp, format!("memory `{}` is read combinationally, so it cannot use block RAM", m.name))
                        .with_label("read used directly in logic")
                        .with_secondary(m.span, format!("{} words", m.depth))
                        .with_help("register the read to use block RAM: `q := mem[addr];`, then use `q` (one cycle later)"),
                );
            }
        }
    }
}

fn expand(src: &Tags, resolved: &HashMap<usize, Tags>, out: &mut Tags) {
    for (a, org) in &src.0 {
        match a {
            TagAtom::Var(v) => {
                if let Some(r) = resolved.get(&(*v as usize)) {
                    out.join(r);
                }
            }
            _ => out.join(&Tags(vec![(*a, org.clone())])),
        }
    }
}
