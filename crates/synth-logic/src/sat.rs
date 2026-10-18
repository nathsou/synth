//! A small CDCL SAT solver in the style of MiniSat: two watched literals,
//! VSIDS branching, first-UIP clause learning, Luby restarts and learnt
//! clause deletion. It solves under assumptions, so one instance serves
//! many related queries (equivalence checks on a growing circuit).

/// A literal: `var << 1 | negated`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct SLit(u32);

impl SLit {
    pub fn new(var: u32, neg: bool) -> SLit {
        SLit(var << 1 | neg as u32)
    }

    pub fn var(self) -> u32 {
        self.0 >> 1
    }

    pub fn is_neg(self) -> bool {
        self.0 & 1 == 1
    }

    fn idx(self) -> usize {
        self.0 as usize
    }
}

impl std::ops::Not for SLit {
    type Output = SLit;
    fn not(self) -> SLit {
        SLit(self.0 ^ 1)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SatResult {
    Sat,
    Unsat,
    /// The conflict limit was reached.
    Unknown,
}

const UNDEF: u8 = 2;
const NO_REASON: u32 = u32::MAX;

#[derive(Clone, Debug)]
struct Clause {
    start: u32,
    len: u32,
    learnt: bool,
    deleted: bool,
    activity: f32,
}

#[derive(Copy, Clone, Debug)]
struct Watch {
    cref: u32,
    blocker: SLit,
}

#[derive(Debug, Default)]
pub struct Solver {
    arena: Vec<SLit>,
    clauses: Vec<Clause>,
    /// `watches[l]`: clauses watching literal `l`, visited when `l` becomes
    /// false.
    watches: Vec<Vec<Watch>>,
    assigns: Vec<u8>,
    level: Vec<u32>,
    reason: Vec<u32>,
    trail: Vec<SLit>,
    trail_lim: Vec<usize>,
    qhead: usize,
    activity: Vec<f64>,
    var_inc: f64,
    cla_inc: f32,
    heap: Vec<u32>,
    heap_pos: Vec<u32>,
    /// Saved phase (true = negative).
    polarity: Vec<bool>,
    seen: Vec<bool>,
    model: Vec<bool>,
    num_learnts: usize,
    max_learnts: usize,
    ok: bool,
    pub conflicts: u64,
}

impl Solver {
    pub fn new() -> Solver {
        Solver { var_inc: 1.0, cla_inc: 1.0, max_learnts: 4000, ok: true, ..Default::default() }
    }

    pub fn num_vars(&self) -> u32 {
        self.assigns.len() as u32
    }

    pub fn new_var(&mut self) -> u32 {
        let v = self.assigns.len() as u32;
        self.assigns.push(UNDEF);
        self.level.push(0);
        self.reason.push(NO_REASON);
        self.activity.push(0.0);
        self.polarity.push(true);
        self.seen.push(false);
        self.heap_pos.push(u32::MAX);
        self.watches.push(Vec::new());
        self.watches.push(Vec::new());
        self.heap_insert(v);
        v
    }

    fn value(&self, l: SLit) -> u8 {
        let a = self.assigns[l.var() as usize];
        if a == UNDEF { UNDEF } else { a ^ l.is_neg() as u8 }
    }

    /// Value of a variable in the last satisfying assignment.
    pub fn model_value(&self, v: u32) -> bool {
        self.model.get(v as usize).copied().unwrap_or(false)
    }

    fn decision_level(&self) -> usize {
        self.trail_lim.len()
    }

    /// Adds a clause (at decision level 0). Returns false if the formula
    /// became unsatisfiable.
    pub fn add_clause(&mut self, lits: &[SLit]) -> bool {
        debug_assert_eq!(self.decision_level(), 0);
        if !self.ok {
            return false;
        }
        let mut c: Vec<SLit> = Vec::with_capacity(lits.len());
        for &l in lits {
            match self.value(l) {
                1 => return true,
                0 => {}
                _ => {
                    if c.contains(&!l) {
                        return true;
                    }
                    if !c.contains(&l) {
                        c.push(l);
                    }
                }
            }
        }
        match c.len() {
            0 => {
                self.ok = false;
                false
            }
            1 => {
                self.enqueue(c[0], NO_REASON);
                self.ok = self.propagate().is_none();
                self.ok
            }
            _ => {
                self.attach(&c, false);
                true
            }
        }
    }

    fn attach(&mut self, lits: &[SLit], learnt: bool) -> u32 {
        let cref = self.clauses.len() as u32;
        self.clauses.push(Clause {
            start: self.arena.len() as u32,
            len: lits.len() as u32,
            learnt,
            deleted: false,
            activity: 0.0,
        });
        self.arena.extend_from_slice(lits);
        self.watches[lits[0].idx()].push(Watch { cref, blocker: lits[1] });
        self.watches[lits[1].idx()].push(Watch { cref, blocker: lits[0] });
        if learnt {
            self.num_learnts += 1;
        }
        cref
    }

    fn enqueue(&mut self, l: SLit, reason: u32) {
        let v = l.var() as usize;
        self.assigns[v] = !l.is_neg() as u8;
        self.level[v] = self.decision_level() as u32;
        self.reason[v] = reason;
        self.trail.push(l);
    }

    /// Unit propagation. Returns a conflicting clause, if any.
    fn propagate(&mut self) -> Option<u32> {
        let mut conflict = None;
        while self.qhead < self.trail.len() {
            let p = self.trail[self.qhead];
            self.qhead += 1;
            let false_lit = !p;
            let mut ws = std::mem::take(&mut self.watches[false_lit.idx()]);
            let (mut i, mut j) = (0, 0);
            while i < ws.len() {
                let w = ws[i];
                i += 1;
                if self.value(w.blocker) == 1 {
                    ws[j] = w;
                    j += 1;
                    continue;
                }
                let c = &self.clauses[w.cref as usize];
                if c.deleted {
                    continue;
                }
                let (s, len) = (c.start as usize, c.len as usize);
                if self.arena[s] == false_lit {
                    self.arena.swap(s, s + 1);
                }
                let first = self.arena[s];
                let nw = Watch { cref: w.cref, blocker: first };
                if first != w.blocker && self.value(first) == 1 {
                    ws[j] = nw;
                    j += 1;
                    continue;
                }
                let mut moved = false;
                for k in 2..len {
                    if self.value(self.arena[s + k]) != 0 {
                        self.arena.swap(s + 1, s + k);
                        let q = self.arena[s + 1];
                        self.watches[q.idx()].push(nw);
                        moved = true;
                        break;
                    }
                }
                if moved {
                    continue;
                }
                ws[j] = nw;
                j += 1;
                if self.value(first) == 0 {
                    conflict = Some(w.cref);
                    self.qhead = self.trail.len();
                    while i < ws.len() {
                        ws[j] = ws[i];
                        j += 1;
                        i += 1;
                    }
                } else {
                    self.enqueue(first, w.cref);
                }
            }
            ws.truncate(j);
            self.watches[false_lit.idx()] = ws;
            if conflict.is_some() {
                break;
            }
        }
        conflict
    }

    fn cancel_until(&mut self, lvl: usize) {
        if self.decision_level() <= lvl {
            return;
        }
        let lim = self.trail_lim[lvl];
        for k in (lim..self.trail.len()).rev() {
            let l = self.trail[k];
            let v = l.var() as usize;
            self.assigns[v] = UNDEF;
            self.reason[v] = NO_REASON;
            self.polarity[v] = l.is_neg();
            self.heap_insert(v as u32);
        }
        self.trail.truncate(lim);
        self.trail_lim.truncate(lvl);
        self.qhead = lim;
    }

    fn bump_var(&mut self, v: u32) {
        self.activity[v as usize] += self.var_inc;
        if self.activity[v as usize] > 1e100 {
            for a in &mut self.activity {
                *a *= 1e-100;
            }
            self.var_inc *= 1e-100;
        }
        if self.heap_pos[v as usize] != u32::MAX {
            self.sift_up(self.heap_pos[v as usize] as usize);
        }
    }

    fn bump_clause(&mut self, cref: u32) {
        let c = &mut self.clauses[cref as usize];
        if !c.learnt {
            return;
        }
        c.activity += self.cla_inc;
        if c.activity > 1e20 {
            for c in &mut self.clauses {
                c.activity *= 1e-20;
            }
            self.cla_inc *= 1e-20;
        }
    }

    /// First-UIP conflict analysis: the learnt clause (asserting literal
    /// first) and the level to backtrack to.
    fn analyze(&mut self, mut confl: u32) -> (Vec<SLit>, usize) {
        let mut learnt = vec![SLit(0)];
        let mut path = 0;
        let mut p: Option<SLit> = None;
        let mut idx = self.trail.len();
        let cur = self.decision_level() as u32;
        loop {
            self.bump_clause(confl);
            let c = &self.clauses[confl as usize];
            let (s, len) = (c.start as usize, c.len as usize);
            let skip = p.is_some() as usize;
            for k in skip..len {
                let q = self.arena[s + k];
                let v = q.var() as usize;
                if !self.seen[v] && self.level[v] > 0 {
                    self.bump_var(v as u32);
                    self.seen[v] = true;
                    if self.level[v] >= cur {
                        path += 1;
                    } else {
                        learnt.push(q);
                    }
                }
            }
            loop {
                idx -= 1;
                if self.seen[self.trail[idx].var() as usize] {
                    break;
                }
            }
            let pl = self.trail[idx];
            p = Some(pl);
            confl = self.reason[pl.var() as usize];
            self.seen[pl.var() as usize] = false;
            path -= 1;
            if path == 0 {
                break;
            }
        }
        learnt[0] = !p.expect("conflict analysis visits at least one literal");
        // Local minimisation: drop literals implied by others in the clause.
        let keep: Vec<bool> = learnt
            .iter()
            .enumerate()
            .map(|(k, &l)| {
                let r = self.reason[l.var() as usize];
                if k == 0 || r == NO_REASON {
                    return true;
                }
                let c = &self.clauses[r as usize];
                (1..c.len as usize).any(|i| {
                    let v = self.arena[c.start as usize + i].var() as usize;
                    !self.seen[v] && self.level[v] > 0
                })
            })
            .collect();
        for l in &learnt {
            self.seen[l.var() as usize] = false;
        }
        let mut out: Vec<SLit> =
            learnt.iter().zip(&keep).filter(|(_, k)| **k).map(|(l, _)| *l).collect();
        let mut bt = 0;
        if out.len() > 1 {
            let mut best = 1;
            for k in 2..out.len() {
                if self.level[out[k].var() as usize] > self.level[out[best].var() as usize] {
                    best = k;
                }
            }
            out.swap(1, best);
            bt = self.level[out[1].var() as usize] as usize;
        }
        (out, bt)
    }

    fn locked(&self, cref: u32) -> bool {
        let c = &self.clauses[cref as usize];
        let l = self.arena[c.start as usize];
        self.value(l) == 1 && self.reason[l.var() as usize] == cref
    }

    /// Deletes the less active half of the long learnt clauses.
    fn reduce_db(&mut self) {
        let mut cand: Vec<u32> = (0..self.clauses.len() as u32)
            .filter(|&k| {
                let c = &self.clauses[k as usize];
                c.learnt && !c.deleted && c.len > 2 && !self.locked(k)
            })
            .collect();
        cand.sort_by(|&a, &b| {
            self.clauses[a as usize].activity.total_cmp(&self.clauses[b as usize].activity)
        });
        for &k in &cand[..cand.len() / 2] {
            self.clauses[k as usize].deleted = true;
            self.num_learnts -= 1;
        }
        let clauses = &self.clauses;
        for ws in &mut self.watches {
            ws.retain(|w| !clauses[w.cref as usize].deleted);
        }
        // Compact the clause arena once deleted clauses dominate it.
        let live: usize = self.clauses.iter().filter(|c| !c.deleted).map(|c| c.len as usize).sum();
        if live * 2 < self.arena.len() {
            let mut arena = Vec::with_capacity(live);
            for c in &mut self.clauses {
                if c.deleted {
                    c.len = 0;
                }
                let s = c.start as usize;
                c.start = arena.len() as u32;
                arena.extend_from_slice(&self.arena[s..s + c.len as usize]);
            }
            self.arena = arena;
        }
    }

    /// Solves under `assumptions`, giving up after `conflict_limit`
    /// conflicts. The solver is back at decision level 0 afterwards, and
    /// after `Sat` the model is available through [`Solver::model_value`].
    pub fn solve(&mut self, assumptions: &[SLit], conflict_limit: u64) -> SatResult {
        if !self.ok {
            return SatResult::Unsat;
        }
        let start = self.conflicts;
        let mut restart = 0u32;
        let mut until_restart = 100 * luby(restart);
        loop {
            if let Some(confl) = self.propagate() {
                self.conflicts += 1;
                if self.decision_level() == 0 {
                    self.ok = false;
                    return SatResult::Unsat;
                }
                let (learnt, bt) = self.analyze(confl);
                self.cancel_until(bt);
                if learnt.len() == 1 {
                    self.enqueue(learnt[0], NO_REASON);
                } else {
                    let cref = self.attach(&learnt, true);
                    self.bump_clause(cref);
                    self.enqueue(learnt[0], cref);
                }
                self.var_inc /= 0.95;
                self.cla_inc /= 0.999;
                until_restart = until_restart.saturating_sub(1);
                continue;
            }
            if self.conflicts - start >= conflict_limit {
                self.cancel_until(0);
                return SatResult::Unknown;
            }
            if until_restart == 0 {
                self.cancel_until(0);
                restart += 1;
                until_restart = 100 * luby(restart);
            }
            if self.num_learnts >= self.max_learnts + self.trail.len() {
                self.reduce_db();
                self.max_learnts = self.max_learnts * 11 / 10;
            }
            let mut next = None;
            while self.decision_level() < assumptions.len() {
                let p = assumptions[self.decision_level()];
                match self.value(p) {
                    1 => self.trail_lim.push(self.trail.len()),
                    0 => {
                        self.cancel_until(0);
                        return SatResult::Unsat;
                    }
                    _ => {
                        next = Some(p);
                        break;
                    }
                }
            }
            let p = match next {
                Some(p) => p,
                None => loop {
                    let Some(v) = self.heap_pop() else {
                        self.model = self.assigns.iter().map(|&a| a == 1).collect();
                        self.cancel_until(0);
                        return SatResult::Sat;
                    };
                    if self.assigns[v as usize] == UNDEF {
                        break SLit::new(v, self.polarity[v as usize]);
                    }
                },
            };
            self.trail_lim.push(self.trail.len());
            self.enqueue(p, NO_REASON);
        }
    }

    // Binary max-heap of variables by activity.

    fn heap_insert(&mut self, v: u32) {
        if self.heap_pos[v as usize] != u32::MAX {
            return;
        }
        self.heap_pos[v as usize] = self.heap.len() as u32;
        self.heap.push(v);
        self.sift_up(self.heap.len() - 1);
    }

    fn heap_pop(&mut self) -> Option<u32> {
        let top = *self.heap.first()?;
        let last = self.heap.pop().expect("heap is not empty");
        self.heap_pos[top as usize] = u32::MAX;
        if !self.heap.is_empty() {
            self.heap[0] = last;
            self.heap_pos[last as usize] = 0;
            self.sift_down(0);
        }
        Some(top)
    }

    fn sift_up(&mut self, mut i: usize) {
        let v = self.heap[i];
        while i > 0 {
            let parent = (i - 1) / 2;
            let pv = self.heap[parent];
            if self.activity[pv as usize] >= self.activity[v as usize] {
                break;
            }
            self.heap[i] = pv;
            self.heap_pos[pv as usize] = i as u32;
            i = parent;
        }
        self.heap[i] = v;
        self.heap_pos[v as usize] = i as u32;
    }

    fn sift_down(&mut self, mut i: usize) {
        let v = self.heap[i];
        let n = self.heap.len();
        loop {
            let mut child = 2 * i + 1;
            if child >= n {
                break;
            }
            if child + 1 < n
                && self.activity[self.heap[child + 1] as usize]
                    > self.activity[self.heap[child] as usize]
            {
                child += 1;
            }
            let cv = self.heap[child];
            if self.activity[cv as usize] <= self.activity[v as usize] {
                break;
            }
            self.heap[i] = cv;
            self.heap_pos[cv as usize] = i as u32;
            i = child;
        }
        self.heap[i] = v;
        self.heap_pos[v as usize] = i as u32;
    }
}

/// The Luby restart sequence 1, 1, 2, 1, 1, 2, 4, ...
fn luby(mut i: u32) -> u64 {
    let mut size = 1u64;
    let mut seq = 0u32;
    while size < i as u64 + 1 {
        seq += 1;
        size = 2 * size + 1;
    }
    while size - 1 != i as u64 {
        size = (size - 1) >> 1;
        seq -= 1;
        i %= size as u32;
    }
    1 << seq
}

#[cfg(test)]
mod tests {
    use super::*;
    use synth_core::Rng;

    fn lit(x: i32) -> SLit {
        SLit::new(x.unsigned_abs() - 1, x < 0)
    }

    #[test]
    fn luby_sequence() {
        let s: Vec<u64> = (0..10).map(luby).collect();
        assert_eq!(s, [1, 1, 2, 1, 1, 2, 4, 1, 1, 2]);
    }

    #[test]
    fn pigeonhole_is_unsat() {
        // 6 pigeons in 5 holes.
        let (p, h) = (6, 5);
        let mut s = Solver::new();
        for _ in 0..p * h {
            s.new_var();
        }
        let v = |i: i32, j: i32| i * h + j + 1;
        for i in 0..p {
            let c: Vec<SLit> = (0..h).map(|j| lit(v(i, j))).collect();
            s.add_clause(&c);
        }
        for j in 0..h {
            for a in 0..p {
                for b in a + 1..p {
                    s.add_clause(&[lit(-v(a, j)), lit(-v(b, j))]);
                }
            }
        }
        assert_eq!(s.solve(&[], u64::MAX), SatResult::Unsat);
    }

    #[test]
    fn random_3sat_models_are_valid() {
        let mut rng = Rng::new(7);
        for round in 0..40 {
            let n = 40;
            let m = if round % 2 == 0 { 150 } else { 190 };
            let mut s = Solver::new();
            for _ in 0..n {
                s.new_var();
            }
            let mut cls = Vec::new();
            for _ in 0..m {
                let c: Vec<SLit> =
                    (0..3).map(|_| SLit::new(rng.below(n) as u32, rng.below(2) == 1)).collect();
                s.add_clause(&c);
                cls.push(c);
            }
            // Assume the first variable both ways.
            for a in [SLit::new(0, false), SLit::new(0, true)] {
                if s.solve(&[a], u64::MAX) == SatResult::Sat {
                    assert_eq!(s.model_value(0), !a.is_neg());
                    for c in &cls {
                        assert!(c.iter().any(|l| s.model_value(l.var()) != l.is_neg()));
                    }
                }
            }
        }
    }
}
