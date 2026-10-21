//! A compiled cycle simulator for RTL modules (ADR 0008).
//!
//! [`Sim::new`] compiles a [`synth_rtl::Module`] into a flat program over a
//! buffer of 64-bit words ([`program`]), which the interpreter ([`exec`])
//! runs. Inputs are written with [`Sim::poke`]; [`Sim::eval`] then settles
//! the combinational logic and fires every clock edge that results, until
//! nothing changes. Clocks are ordinary signals: toggling one and calling
//! `eval` runs the flip-flops of that edge.
//!
//! ```ignore
//! let mut sim = Sim::new(&module, &Options::default())?;
//! let (clk, out) = (sim.signal("clk").unwrap(), sim.signal("out").unwrap());
//! for _ in 0..10 {
//!     sim.poke(&clk, 1);
//!     sim.eval();
//!     sim.poke(&clk, 0);
//!     sim.eval();
//! }
//! println!("{:?}", sim.peek(&out));
//! ```

pub mod attach;
pub mod compile;
pub mod emit_c;
pub mod exec;
pub mod netlist;
pub mod program;
pub mod vcd;
pub mod words;

use synth_core::{FxHashMap, Logic, LogicVec};
use synth_rtl::Module;

pub use attach::{WritePort, attach_memory};
pub use compile::Options;
use exec::State;
use program::{Loc, Program, SlotKind};

/// A named signal, resolved to its bits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signal {
    locs: Vec<Loc>,
    /// The slot holding exactly this signal, when there is one.
    direct: Option<(u32, u32)>,
}

impl Signal {
    pub fn width(&self) -> usize {
        self.locs.len()
    }

    fn new(locs: Vec<Loc>, p: &Program) -> Signal {
        let w = locs.len() as u32;
        let direct = match locs.first() {
            Some(&Loc::Bit { word, bit: 0 })
                if w <= 64
                    && locs.iter().enumerate().all(|(k, l)| {
                        *l == Loc::Bit { word: word + k as u32 / 64, bit: (k % 64) as u8 }
                    }) =>
            {
                let _ = p;
                Some((word, w))
            }
            _ => None,
        };
        Signal { locs, direct }
    }
}

/// A memory of the design.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct MemoryId(u32);

/// A simulation.
#[derive(Debug)]
pub struct Sim {
    prog: Program,
    st: State,
    prev: Vec<Logic>,
    /// Units to run at the next settle, one bit per instruction.
    dirty: Vec<u64>,
    fired: Vec<u32>,
    names: FxHashMap<String, usize>,
    /// Clock edges processed (each fired domain counts once).
    pub edges: u64,
}

/// Runs the edges of the fired domains: every sample, then every memory
/// write, then every state update.
fn fire<const F: bool>(p: &Program, fired: &[u32], st: &mut State, dirty: &mut [u64]) {
    for &d in fired {
        exec::run::<F>(&p.aux, &p.domains[d as usize].sample, st);
    }
    for &d in fired {
        exec::write::<F>(p, &p.domains[d as usize].write, st, dirty);
    }
    for &d in fired {
        let dom = &p.domains[d as usize];
        exec::commit::<F>(p, &dom.commit, st, dirty);
        exec::commit_gated::<F>(p, dom, st, dirty);
    }
}

/// Rounds of clock edges in one [`Sim::eval`] before it gives up.
const MAX_ROUNDS: usize = 10_000;

impl Sim {
    /// Compiles a module and settles its initial state.
    pub fn new(m: &Module, opts: &Options) -> Result<Sim, String> {
        let prog = compile::compile(m, opts)?;
        Ok(Sim::from_program(prog))
    }

    pub fn from_program(prog: Program) -> Sim {
        let st = State::new(&prog);
        let mut names = FxHashMap::default();
        for (i, (n, _)) in prog.signals.iter().enumerate() {
            names.entry(n.clone()).or_insert(i);
        }
        let mut dirty = vec![0u64; (prog.units as usize).div_ceil(64)];
        for i in 0..prog.ops.len() {
            dirty[i / 64] |= 1 << (i % 64);
        }
        for d in &prog.domains {
            for i in 0..d.gated.len() {
                let u = d.base as usize + i;
                dirty[u / 64] |= 1 << (u % 64);
            }
        }
        let mut sim = Sim { st, prev: Vec::new(), dirty, fired: Vec::new(), names, edges: 0, prog };
        sim.settle();
        sim.prev = sim.prog.clocks.iter().map(|c| sim.bit(c.word, c.bit)).collect();
        sim
    }

    /// The compiled program.
    pub fn program(&self) -> &Program {
        &self.prog
    }

    /// Warnings from compilation (unsimulated instances).
    pub fn warnings(&self) -> &[String] {
        &self.prog.warnings
    }

    /// The value plane of the state (for diagnostics).
    pub fn state_words(&self) -> &[u64] {
        &self.st.val
    }

    /// Checks that the state is settled: running every instruction again
    /// changes nothing. Returns the instructions whose outputs would change
    /// (for testing the change propagation).
    pub fn unsettled(&self) -> Vec<usize> {
        let mut st = self.st.clone();
        if self.prog.four_state {
            exec::run::<true>(&self.prog.aux, &self.prog.ops, &mut st);
        } else {
            exec::run::<false>(&self.prog.aux, &self.prog.ops, &mut st);
        }
        let mut bad = Vec::new();
        for (i, ins) in self.prog.ops.iter().enumerate() {
            if ins.k == program::K::Loop {
                continue;
            }
            let n = self.prog.out[i].n as usize;
            let d = ins.d as usize;
            let differs = st.val[d..d + n] != self.st.val[d..d + n]
                || self.prog.four_state && st.unk[d..d + n] != self.st.unk[d..d + n];
            if differs {
                bad.push(i);
            }
        }
        bad
    }

    /// Combinational units run so far.
    pub fn units_run(&self) -> u64 {
        self.st.units_run
    }

    /// Combinational loops that did not settle so far.
    pub fn unstable_loops(&self) -> u64 {
        self.st.unstable
    }

    fn bit(&self, word: u32, bit: u8) -> Logic {
        let v = self.st.val[word as usize] >> bit & 1;
        let u = if self.prog.four_state { self.st.unk[word as usize] >> bit & 1 } else { 0 };
        match (u, v) {
            (0, 0) => Logic::Zero,
            (0, _) => Logic::One,
            (_, 0) => Logic::X,
            _ => Logic::Z,
        }
    }

    /// Runs what changes reached.
    fn settle(&mut self) {
        if self.prog.four_state {
            exec::settle::<true>(&self.prog, &mut self.st, &mut self.dirty);
        } else {
            exec::settle::<false>(&self.prog, &mut self.st, &mut self.dirty);
        }
    }

    /// Marks the readers of the slot holding `word` for the next settle.
    fn touch(&mut self, word: u32) {
        let slot = self.prog.slot_of[word as usize];
        exec::mark(&self.prog, &mut self.dirty, slot, u32::MAX);
        let u = self.prog.state_unit[slot as usize];
        if u != program::NONE {
            self.dirty[(u >> 6) as usize] |= 1 << (u & 63);
        }
    }

    /// Settles the design after inputs changed, then fires the clock edges
    /// that result, repeatedly, until no clock changes.
    pub fn eval(&mut self) {
        for _ in 0..MAX_ROUNDS {
            self.settle();
            self.fired.clear();
            for ci in 0..self.prog.clocks.len() {
                let c = &self.prog.clocks[ci];
                let cur = self.bit(c.word, c.bit);
                let prev = self.prev[ci];
                if cur == prev {
                    continue;
                }
                self.prev[ci] = cur;
                let rise = matches!((prev, cur), (Logic::Zero, _) | (_, Logic::One))
                    && prev != Logic::One
                    && cur != Logic::Zero;
                let fall = matches!((prev, cur), (Logic::One, _) | (_, Logic::Zero))
                    && prev != Logic::Zero
                    && cur != Logic::One;
                if rise && let Some(d) = c.pos {
                    self.fired.push(d);
                }
                if fall && let Some(d) = c.neg {
                    self.fired.push(d);
                }
            }
            if self.fired.is_empty() {
                return;
            }
            let fired = std::mem::take(&mut self.fired);
            let (p, st, dirty) = (&self.prog, &mut self.st, &mut self.dirty);
            if p.four_state {
                fire::<true>(p, &fired, st, dirty);
            } else {
                fire::<false>(p, &fired, st, dirty);
            }
            self.edges += fired.len() as u64;
            self.fired = fired;
        }
        panic!("clocks did not settle after {MAX_ROUNDS} rounds of edges");
    }

    /// Settles the design after inputs changed and takes the current clock
    /// values as the starting point, without firing any edge: for setting
    /// up initial values (such as clocks leaving `x`).
    pub fn eval_quiet(&mut self) {
        self.settle();
        self.prev = self.prog.clocks.iter().map(|c| self.bit(c.word, c.bit)).collect();
    }

    /// Looks a signal up by name (a wire of the module).
    pub fn signal(&self, name: &str) -> Option<Signal> {
        let &i = self.names.get(name)?;
        Some(Signal::new(self.prog.signals[i].1.clone(), &self.prog))
    }

    /// Bits `lo..lo + width` of a signal.
    pub fn slice(&self, s: &Signal, lo: usize, width: usize) -> Signal {
        Signal::new(s.locs[lo..lo + width].to_vec(), &self.prog)
    }

    /// Names of the signals.
    pub fn signal_names(&self) -> impl Iterator<Item = &str> {
        self.prog.signals.iter().map(|(n, _)| n.as_str())
    }

    /// Single-bit signals of every clock.
    pub fn clocks(&self) -> Vec<Signal> {
        self.prog
            .clocks
            .iter()
            .map(|c| Signal::new(vec![Loc::Bit { word: c.word, bit: c.bit }], &self.prog))
            .collect()
    }

    /// Whether every bit of `s` can be written: inputs (undriven bits) and
    /// state (flip-flops, latches, synchronous read ports).
    pub fn writable(&self, s: &Signal) -> bool {
        s.locs.iter().all(|l| match l {
            Loc::Bit { word, .. } => {
                matches!(self.prog.word_kind[*word as usize], SlotKind::Input | SlotKind::State)
            }
            Loc::Const(_) => false,
        })
    }

    /// Writes a value; takes effect at the next [`Sim::eval`].
    pub fn try_poke_vec(&mut self, s: &Signal, v: &LogicVec) -> Result<(), String> {
        if !self.writable(s) {
            return Err("the signal is driven by logic or constant".into());
        }
        let four = self.prog.four_state;
        if let Some((word, w)) = s.direct
            && v.width() <= 64
        {
            let v = v.resize(w as usize, false);
            let (val, unk) = (v.val_words()[0], v.unk_words()[0]);
            let o = word as usize;
            if four {
                self.st.val[o] = val;
                self.st.unk[o] = unk;
            } else {
                self.st.val[o] = val & !unk;
            }
            self.touch(word);
            return Ok(());
        }
        for (k, l) in s.locs.iter().enumerate() {
            let Loc::Bit { word, bit } = *l else { continue };
            let b = if k < v.width() { v.get(k) } else { Logic::Zero };
            let (val, unk) = match b {
                Logic::Zero => (0, 0),
                Logic::One => (1, 0),
                Logic::X => (0, 1),
                Logic::Z => (1, 1),
            };
            let (val, unk) = if four { (val, unk) } else { (val & !unk, 0) };
            let o = word as usize;
            self.st.val[o] = self.st.val[o] & !(1 << bit) | val << bit;
            if four {
                self.st.unk[o] = self.st.unk[o] & !(1 << bit) | unk << bit;
            }
            self.touch(word);
        }
        Ok(())
    }

    /// Writes a value; panics if the signal is not writable.
    pub fn poke_vec(&mut self, s: &Signal, v: &LogicVec) {
        if let Err(e) = self.try_poke_vec(s, v) {
            panic!("poke: {e}");
        }
    }

    /// Writes an integer value (truncated or zero-extended).
    pub fn poke(&mut self, s: &Signal, v: u64) {
        if let Some((word, w)) = s.direct
            && matches!(self.prog.word_kind[word as usize], SlotKind::Input | SlotKind::State)
        {
            let o = word as usize;
            self.st.val[o] = v & program::mask(w);
            if self.prog.four_state {
                self.st.unk[o] = 0;
            }
            self.touch(word);
            return;
        }
        let w = s.width();
        self.poke_vec(s, &LogicVec::from_u64(v, w.max(1)).resize(w, false));
    }

    /// Reads a value.
    pub fn peek_vec(&self, s: &Signal) -> LogicVec {
        if let Some((word, w)) = s.direct {
            let o = word as usize;
            let unk = if self.prog.four_state { self.st.unk[o] } else { 0 };
            return LogicVec::from_planes(w as usize, &[self.st.val[o]], &[unk]);
        }
        let bits: Vec<Logic> = s
            .locs
            .iter()
            .map(|l| match *l {
                Loc::Const(c) => {
                    if self.prog.four_state || c.is_known() {
                        c
                    } else {
                        Logic::Zero
                    }
                }
                Loc::Bit { word, bit } => self.bit(word, bit),
            })
            .collect();
        LogicVec::from_bits(&bits)
    }

    /// Reads a value as an integer: `None` if a bit is unknown or it is
    /// wider than 64 bits and does not fit.
    pub fn peek(&self, s: &Signal) -> Option<u64> {
        if let Some((word, _)) = s.direct {
            let o = word as usize;
            if self.prog.four_state && self.st.unk[o] != 0 {
                return None;
            }
            return Some(self.st.val[o]);
        }
        self.peek_vec(s).to_u64()
    }

    /// Looks up a signal by name and writes it.
    pub fn set(&mut self, name: &str, v: u64) {
        let s = self.signal(name).unwrap_or_else(|| panic!("no signal `{name}`"));
        self.poke(&s, v);
    }

    /// Looks up a signal by name and reads it.
    pub fn get(&self, name: &str) -> Option<u64> {
        let s = self.signal(name).unwrap_or_else(|| panic!("no signal `{name}`"));
        self.peek(&s)
    }

    /// One cycle of a clock: rising edge, then falling edge, evaluating
    /// after each.
    pub fn cycle(&mut self, clk: &Signal) {
        self.poke(clk, 1);
        self.eval();
        self.poke(clk, 0);
        self.eval();
    }

    /// A memory by name.
    pub fn memory(&self, name: &str) -> Option<MemoryId> {
        self.prog.aux.mems.iter().position(|m| m.name == name).map(|i| MemoryId(i as u32))
    }

    /// Names of the memories.
    pub fn memory_names(&self) -> impl Iterator<Item = &str> {
        self.prog.aux.mems.iter().map(|m| m.name.as_str())
    }

    /// The width, depth and first address of a memory.
    pub fn memory_shape(&self, m: MemoryId) -> (u32, u32, i64) {
        let mem = &self.prog.aux.mems[m.0 as usize];
        (mem.width, mem.depth, mem.offset)
    }

    /// Reads a memory word (`addr` counts from the memory's first address).
    pub fn read_memory(&self, m: MemoryId, addr: i64) -> LogicVec {
        let mem = &self.prog.aux.mems[m.0 as usize];
        let idx = addr - mem.offset;
        if idx < 0 || idx >= mem.depth as i64 {
            return LogicVec::xs(mem.width as usize);
        }
        let n = program::nwords(mem.width) as usize;
        let o = mem.base + idx as usize * n;
        let unk: &[u64] = if self.prog.four_state { &self.st.munk[o..o + n] } else { &[] };
        LogicVec::from_planes(mem.width as usize, &self.st.mval[o..o + n], unk)
    }

    /// Writes a memory word; takes effect at the next [`Sim::eval`].
    pub fn write_memory(&mut self, m: MemoryId, addr: i64, v: &LogicVec) {
        let mem = &self.prog.aux.mems[m.0 as usize];
        let idx = addr - mem.offset;
        if idx < 0 || idx >= mem.depth as i64 {
            return;
        }
        let n = program::nwords(mem.width) as usize;
        let o = mem.base + idx as usize * n;
        let v = v.resize(mem.width as usize, false);
        for j in 0..n {
            let (val, unk) = (v.val_words()[j], v.unk_words()[j]);
            if self.prog.four_state {
                self.st.mval[o + j] = val;
                self.st.munk[o + j] = unk;
            } else {
                self.st.mval[o + j] = val & !unk;
            }
        }
        for &r in &self.prog.mem_readers[m.0 as usize] {
            self.dirty[(r >> 6) as usize] |= 1 << (r & 63);
        }
    }

    /// Fills a memory with words of up to 64 bits from address `start`.
    pub fn load_memory(&mut self, m: MemoryId, start: i64, words: &[u64]) {
        let w = self.prog.aux.mems[m.0 as usize].width as usize;
        for (i, &x) in words.iter().enumerate() {
            self.write_memory(
                m,
                start + i as i64,
                &LogicVec::from_u64(x, w.max(1)).resize(w, false),
            );
        }
    }
}
