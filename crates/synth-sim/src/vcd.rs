//! Value change dump (VCD) output.
//!
//! Signals are grouped into scopes by the dots of their hierarchical names
//! (`cpu.alu.result` is `result` in scope `alu` of scope `cpu`). Each
//! [`Vcd::sample`] writes the signals that changed since the last one.

use std::io::{self, Write};

use synth_core::{Logic, LogicVec};

use crate::{Signal, Sim};

struct Var {
    sig: Signal,
    id: String,
    last: Option<LogicVec>,
}

/// A VCD file being written.
#[derive(Debug)]
pub struct Vcd<W: Write> {
    w: W,
    vars: Vec<Var>,
}

impl std::fmt::Debug for Var {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.id)
    }
}

/// The identifier of variable `i`: printable characters, base 94.
fn ident(mut i: usize) -> String {
    let mut s = String::new();
    loop {
        s.push((33 + (i % 94) as u8) as char);
        i /= 94;
        if i == 0 {
            return s;
        }
        i -= 1;
    }
}

fn clean(name: &str) -> String {
    name.chars().map(|c| if c.is_whitespace() || c == '\\' { '_' } else { c }).collect()
}

impl<W: Write> Vcd<W> {
    /// Starts a dump of the signals of `sim` whose names pass `keep`, with
    /// a time unit of `timescale` (such as `1ns`), under a top scope `top`.
    pub fn new(
        sim: &Sim,
        mut w: W,
        top: &str,
        timescale: &str,
        keep: impl Fn(&str) -> bool,
    ) -> io::Result<Vcd<W>> {
        let mut names: Vec<&str> = sim.signal_names().filter(|n| keep(n)).collect();
        names.sort_unstable();
        names.dedup();
        writeln!(w, "$version synth $end")?;
        writeln!(w, "$timescale {timescale} $end")?;
        writeln!(w, "$scope module {} $end", clean(top))?;
        let mut vars = Vec::new();
        let mut scope: Vec<String> = Vec::new();
        for name in names {
            let Some(sig) = sim.signal(name) else { continue };
            if sig.width() == 0 {
                continue;
            }
            let parts: Vec<&str> = name.split('.').collect();
            let (path, leaf) = parts.split_at(parts.len() - 1);
            let common = scope.iter().zip(path).take_while(|(a, b)| a == *b).count();
            for _ in common..scope.len() {
                writeln!(w, "$upscope $end")?;
            }
            scope.truncate(common);
            for p in &path[common..] {
                writeln!(w, "$scope module {} $end", clean(p))?;
                scope.push(p.to_string());
            }
            let id = ident(vars.len());
            writeln!(w, "$var wire {} {id} {} $end", sig.width(), clean(leaf[0]))?;
            vars.push(Var { sig, id, last: None });
        }
        for _ in 0..scope.len() + 1 {
            writeln!(w, "$upscope $end")?;
        }
        writeln!(w, "$enddefinitions $end")?;
        Ok(Vcd { w, vars })
    }

    /// Writes the values at `time` that changed since the last sample (all
    /// of them the first time).
    pub fn sample(&mut self, sim: &Sim, time: u64) -> io::Result<()> {
        let mut header = false;
        for v in &mut self.vars {
            let cur = sim.peek_vec(&v.sig);
            if v.last.as_ref() == Some(&cur) {
                continue;
            }
            if !header {
                writeln!(self.w, "#{time}")?;
                header = true;
            }
            if cur.width() == 1 {
                writeln!(self.w, "{}{}", cur.get(0).to_char(), v.id)?;
            } else {
                let bits: String = cur.bits().rev().map(Logic::to_char).collect();
                writeln!(self.w, "b{bits} {}", v.id)?;
            }
            v.last = Some(cur);
        }
        Ok(())
    }

    /// Flushes and returns the writer.
    pub fn finish(mut self, time: u64) -> io::Result<W> {
        writeln!(self.w, "#{time}")?;
        self.w.flush()?;
        Ok(self.w)
    }
}
