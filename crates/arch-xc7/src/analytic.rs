//! Analytical global placement (HeAP, Gort and Anderson, as in nextpnr):
//! quadratic wirelength under the bound-to-bound net model, solved per axis
//! by conjugate gradients; each solution is spread over the sites of each
//! object's kind by recursive cuts of the regions that overflow, and the
//! spread positions anchor the next solution with a weight that grows each
//! iteration, until solution and spread converge.

/// A net's pin: on a movable object, offset from its center, or fixed.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Pin {
    Object(usize, f64, f64),
    Fixed(f64, f64),
}

/// Sites per tile of one kind, over the tile grid.
#[derive(Clone, Debug)]
pub(crate) struct Grid {
    pub width: usize,
    pub height: usize,
    pub sites: Vec<f64>,
}
impl Grid {
    pub fn new(width: usize, height: usize) -> Self {
        Self { width, height, sites: vec![0.0; width * height] }
    }
    pub fn add(&mut self, x: i32, y: i32) {
        self.sites[y as usize * self.width + x as usize] += 1.0;
    }
    fn at(&self, x: usize, y: usize) -> f64 {
        self.sites[y * self.width + x]
    }
    fn bin(&self, (x, y): (f64, f64)) -> (usize, usize) {
        let clamp = |v: f64, n: usize| (v.round().max(0.0) as usize).min(n - 1);
        (clamp(x, self.width), clamp(y, self.height))
    }
}

pub(crate) struct Problem {
    /// Each object's kind, indexing `grids`, and the sites it takes.
    pub kind: Vec<usize>,
    pub area: Vec<f64>,
    pub nets: Vec<Vec<Pin>>,
    /// Connections pulled by timing, beside their nets, with their weights.
    pub arcs: Vec<(Pin, Pin, f64)>,
    pub grids: Vec<Grid>,
}

/// The cost of a tile of vertical distance against a horizontal one in
/// spreading and legalization: rows cost about 60 ps each over short
/// distances against 16 ps per tile column (`sta::predicted_delay`), and
/// slice columns are about two tile columns apart. Spreading wide rather
/// than tall shortens Euler 57's critical paths by rows, pressed into a
/// corner by its pins.
pub(crate) const VERTICAL_COST: f64 = 2.0;
/// The fraction of a region's sites spreading fills.
const DENSITY: f64 = 0.9;
/// Anchor weight per iteration (nextpnr's alpha).
const ALPHA: f64 = 0.5;
const MAX_ITERATIONS: usize = 40;
/// Iterations without a better spread wirelength before stopping.
const PATIENCE: usize = 5;

/// Iterations between timing analyses.
const RETIME_ITERATIONS: usize = 5;

/// Timing connections and their weights for spread positions.
pub(crate) type Retime<'a> = &'a mut dyn FnMut(&[(f64, f64)]) -> Vec<(Pin, Pin, f64)>;

/// Runs global placement from `pos` (each object's center); returns the
/// spread positions with the least wirelength since the last timing
/// analysis.
pub(crate) fn place(
    p: &mut Problem,
    pos: &mut [(f64, f64)],
    mut retime: Option<Retime<'_>>,
) -> Vec<(f64, f64)> {
    let n = pos.len();
    let mut anchors: Option<Vec<(f64, f64)>> = None;
    let mut best = (f64::INFINITY, pos.to_vec());
    let mut stale = 0;
    for iteration in 0..MAX_ITERATIONS {
        synth_core::progress::advance(0.2 * iteration as f64 / MAX_ITERATIONS as f64, || {
            format!("analytical placement: iteration {}", iteration + 1)
        });
        // The bound-to-bound model linearizes around the current positions:
        // a few solutions converge the first, unanchored one.
        let passes = if iteration == 0 { 5 } else { 1 };
        for _ in 0..passes {
            for axis in 0..2 {
                solve_axis(p, pos, anchors.as_deref(), ALPHA * iteration as f64, axis);
            }
        }
        let solved = wirelength(p, pos);
        let mut spread = pos.to_vec();
        for kind in 0..p.grids.len() {
            let members: Vec<usize> = (0..n).filter(|&o| p.kind[o] == kind).collect();
            spread_kind(p, kind, &members, &mut spread);
        }
        let legal = wirelength(p, &spread);
        if std::env::var("SYNTH_XC7_PLACE_TRACE").is_ok() {
            eprintln!("trace: analytic {iteration} solved {solved:.0} spread {legal:.0}");
        }
        if legal < best.0 {
            best = (legal, spread.clone());
            stale = 0;
        } else {
            stale += 1;
        }
        if stale >= PATIENCE || solved > 0.95 * legal {
            break;
        }
        if let Some(retime) = retime.as_mut()
            && iteration % RETIME_ITERATIONS == RETIME_ITERATIONS - 1
        {
            p.arcs = retime(&spread);
            if std::env::var("SYNTH_XC7_PLACE_TRACE").is_ok() {
                let w: f64 = p.arcs.iter().map(|a| a.2).sum();
                eprintln!("trace: analytic retime {} arcs weight {w:.1}", p.arcs.len());
            }
            best.0 = f64::INFINITY;
            stale = 0;
        }
        anchors = Some(spread);
    }
    best.1
}

fn pin_at(pin: &Pin, pos: &[(f64, f64)]) -> (f64, f64) {
    match *pin {
        Pin::Object(o, dx, dy) => (pos[o].0 + dx, pos[o].1 + dy),
        Pin::Fixed(x, y) => (x, y),
    }
}

/// Half-perimeter wirelength.
pub(crate) fn wirelength(p: &Problem, pos: &[(f64, f64)]) -> f64 {
    p.nets
        .iter()
        .map(|net| {
            let mut b = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
            for pin in net {
                let (x, y) = pin_at(pin, pos);
                b = (b.0.min(x), b.1.min(y), b.2.max(x), b.3.max(y));
            }
            b.2 - b.0 + b.3 - b.1
        })
        .sum()
}

/// A symmetric system over the objects' coordinates on one axis.
struct System {
    diagonal: Vec<f64>,
    off: Vec<Vec<(usize, f64)>>,
    rhs: Vec<f64>,
}
impl System {
    /// Adds a spring of weight `w` between two pins' coordinates.
    fn spring(&mut self, a: (Option<usize>, f64), b: (Option<usize>, f64), w: f64) {
        match (a, b) {
            ((Some(i), oi), (Some(j), oj)) if i != j => {
                self.diagonal[i] += w;
                self.diagonal[j] += w;
                self.off[i].push((j, -w));
                self.off[j].push((i, -w));
                self.rhs[i] += w * (oj - oi);
                self.rhs[j] += w * (oi - oj);
            }
            ((Some(i), oi), (None, f)) | ((None, f), (Some(i), oi)) => {
                self.diagonal[i] += w;
                self.rhs[i] += w * (f - oi);
            }
            _ => {}
        }
    }
    fn multiply(&self, v: &[f64], out: &mut [f64]) {
        for (i, o) in out.iter_mut().enumerate() {
            *o = self.diagonal[i] * v[i] + self.off[i].iter().map(|&(j, w)| w * v[j]).sum::<f64>();
        }
    }
    /// Jacobi-preconditioned conjugate gradients from `x`.
    fn solve(&self, x: &mut [f64]) {
        let n = x.len();
        let mut r = vec![0.0; n];
        self.multiply(x, &mut r);
        for (r, b) in r.iter_mut().zip(&self.rhs) {
            *r = b - *r;
        }
        let precondition = |r: &[f64]| -> Vec<f64> {
            r.iter().zip(&self.diagonal).map(|(r, d)| if *d > 0.0 { r / d } else { 0.0 }).collect()
        };
        let mut z = precondition(&r);
        let mut d = z.clone();
        let mut rz: f64 = r.iter().zip(&z).map(|(a, b)| a * b).sum();
        let norm: f64 = self.rhs.iter().map(|v| v * v).sum::<f64>().sqrt().max(1e-9);
        let mut q = vec![0.0; n];
        for _ in 0..(2 * n).clamp(50, 500) {
            if r.iter().map(|v| v * v).sum::<f64>().sqrt() < 1e-6 * norm {
                break;
            }
            self.multiply(&d, &mut q);
            let dq: f64 = d.iter().zip(&q).map(|(a, b)| a * b).sum();
            if dq <= 0.0 {
                break;
            }
            let step = rz / dq;
            for i in 0..n {
                x[i] += step * d[i];
                r[i] -= step * q[i];
            }
            z = precondition(&r);
            let next: f64 = r.iter().zip(&z).map(|(a, b)| a * b).sum();
            for i in 0..n {
                d[i] = z[i] + next / rz * d[i];
            }
            rz = next;
        }
    }
}

fn solve_axis(
    p: &Problem,
    pos: &mut [(f64, f64)],
    anchors: Option<&[(f64, f64)]>,
    anchor_weight: f64,
    axis: usize,
) {
    let n = pos.len();
    let mut sys = System { diagonal: vec![0.0; n], off: vec![Vec::new(); n], rhs: vec![0.0; n] };
    let coord = |pin: &Pin, pos: &[(f64, f64)]| -> ((Option<usize>, f64), f64) {
        let (x, y) = pin_at(pin, pos);
        let at = if axis == 0 { x } else { y };
        match *pin {
            Pin::Object(o, dx, dy) => ((Some(o), if axis == 0 { dx } else { dy }), at),
            Pin::Fixed(..) => ((None, at), at),
        }
    };
    for net in &p.nets {
        if net.len() < 2 {
            continue;
        }
        let pins: Vec<_> = net.iter().map(|pin| coord(pin, pos)).collect();
        let (mut lo, mut hi) = (0, 0);
        for (k, pin) in pins.iter().enumerate() {
            if pin.1 < pins[lo].1 {
                lo = k;
            }
            if pin.1 > pins[hi].1 {
                hi = k;
            }
        }
        if lo == hi {
            hi = usize::from(lo == 0);
        }
        let scale = 2.0 / (pins.len() - 1) as f64;
        let mut spring = |a: usize, b: usize| {
            let w = scale / (pins[a].1 - pins[b].1).abs().max(1.0);
            sys.spring(pins[a].0, pins[b].0, w);
        };
        spring(lo, hi);
        for k in 0..pins.len() {
            if k != lo && k != hi {
                spring(k, lo);
                spring(k, hi);
            }
        }
    }
    for (a, b, weight) in &p.arcs {
        let (a, b) = (coord(a, pos), coord(b, pos));
        sys.spring(a.0, b.0, weight / (a.1 - b.1).abs().max(1.0));
    }
    let current: Vec<f64> = pos.iter().map(|&(x, y)| if axis == 0 { x } else { y }).collect();
    for o in 0..n {
        let (target, weight) = match anchors {
            Some(a) => {
                let t = if axis == 0 { a[o].0 } else { a[o].1 };
                (t, anchor_weight / (t - current[o]).abs().max(1.0))
            }
            // A faint pull to where each object is keeps the first system,
            // anchorless, positive definite.
            None => (current[o], 1e-4),
        };
        sys.spring((Some(o), 0.0), (None, target), weight.max(1e-4));
    }
    let mut x = current;
    sys.solve(&mut x);
    for (o, v) in x.into_iter().enumerate() {
        if axis == 0 {
            pos[o].0 = v;
        } else {
            pos[o].1 = v;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
}
impl Rect {
    fn contains(&self, (x, y): (usize, usize)) -> bool {
        (self.x0..=self.x1).contains(&x) && (self.y0..=self.y1).contains(&y)
    }
    fn overlaps(&self, o: &Rect) -> bool {
        self.x0 <= o.x1 && o.x0 <= self.x1 && self.y0 <= o.y1 && o.y0 <= self.y1
    }
    fn union(&self, o: &Rect) -> Rect {
        Rect {
            x0: self.x0.min(o.x0),
            y0: self.y0.min(o.y0),
            x1: self.x1.max(o.x1),
            y1: self.y1.max(o.y1),
        }
    }
}

/// Spreads the objects of one kind: each overfull tile grows into a region
/// with sites enough for the objects in it (merging regions that meet), and
/// each region's objects are cut recursively over its sites.
fn spread_kind(p: &Problem, kind: usize, members: &[usize], pos: &mut [(f64, f64)]) {
    let g = &p.grids[kind];
    let mut used = vec![0.0; g.width * g.height];
    for &o in members {
        let (x, y) = g.bin(pos[o]);
        used[y * g.width + x] += p.area[o];
    }
    let sum = |r: &Rect, v: &[f64]| -> f64 {
        (r.y0..=r.y1).map(|y| v[y * g.width + r.x0..=y * g.width + r.x1].iter().sum::<f64>()).sum()
    };
    let mut regions: Vec<Rect> = Vec::new();
    for y in 0..g.height {
        for x in 0..g.width {
            let u = used[y * g.width + x];
            if u == 0.0 || u <= DENSITY * g.at(x, y) || regions.iter().any(|r| r.contains((x, y))) {
                continue;
            }
            let mut r = Rect { x0: x, y0: y, x1: x, y1: y };
            let mut grow_y = false;
            loop {
                while let Some(k) = regions.iter().position(|o| o.overlaps(&r)) {
                    r = r.union(&regions.swap_remove(k));
                }
                if sum(&r, &used) <= DENSITY * sum(&r, &g.sites)
                    || (r.x0 == 0 && r.y0 == 0 && r.x1 == g.width - 1 && r.y1 == g.height - 1)
                {
                    break;
                }
                // Grow the shorter side by cost, alternating when even.
                let (wide, tall) = ((r.x1 - r.x0) as f64, (r.y1 - r.y0) as f64 * VERTICAL_COST);
                grow_y = if wide < tall {
                    false
                } else if wide > tall {
                    true
                } else {
                    !grow_y
                };
                if grow_y && (r.y0 > 0 || r.y1 < g.height - 1) {
                    r.y0 = r.y0.saturating_sub(1);
                    r.y1 = (r.y1 + 1).min(g.height - 1);
                } else if r.x0 > 0 || r.x1 < g.width - 1 {
                    r.x0 = r.x0.saturating_sub(1);
                    r.x1 = (r.x1 + 1).min(g.width - 1);
                } else {
                    r.y0 = r.y0.saturating_sub(1);
                    r.y1 = (r.y1 + 1).min(g.height - 1);
                }
            }
            regions.push(r);
        }
    }
    for r in regions {
        let mut inside: Vec<usize> =
            members.iter().copied().filter(|&o| r.contains(g.bin(pos[o]))).collect();
        cut(p, g, r, &mut inside, pos);
    }
}

/// Splits a region's objects across a cut line in proportion to the sites
/// on either side, recursively, keeping their order along each cut axis.
fn cut(p: &Problem, g: &Grid, r: Rect, objects: &mut [usize], pos: &mut [(f64, f64)]) {
    if objects.is_empty() {
        return;
    }
    let single = r.x0 == r.x1 && r.y0 == r.y1;
    if objects.len() == 1 || single {
        // The leaf's objects go to its tile with sites nearest each.
        for &o in objects.iter() {
            let (bx, by) = g.bin(pos[o]);
            let mut best = None;
            for y in r.y0..=r.y1 {
                for x in r.x0..=r.x1 {
                    if g.at(x, y) == 0.0 {
                        continue;
                    }
                    let d = x.abs_diff(bx) as f64 + VERTICAL_COST * y.abs_diff(by) as f64;
                    if best.is_none_or(|(b, _)| d < b) {
                        best = Some((d, (x, y)));
                    }
                }
            }
            if let Some((_, (x, y))) = best {
                pos[o] = (x as f64, y as f64);
            }
        }
        return;
    }
    // Cut across the longer side by cost.
    let preferred = usize::from((r.y1 - r.y0) as f64 * VERTICAL_COST > (r.x1 - r.x0) as f64);
    for axis in [preferred, 1 - preferred] {
        let (lo, hi) = if axis == 0 { (r.x0, r.x1) } else { (r.y0, r.y1) };
        if lo == hi {
            continue;
        }
        let line = |i: usize| -> f64 {
            let s = if axis == 0 { Rect { x0: i, x1: i, ..r } } else { Rect { y0: i, y1: i, ..r } };
            (s.y0..=s.y1)
                .flat_map(|y| (s.x0..=s.x1).map(move |x| (x, y)))
                .map(|(x, y)| g.at(x, y))
                .sum()
        };
        let lines: Vec<f64> = (lo..=hi).map(line).collect();
        let total: f64 = lines.iter().sum();
        // The cut after the line where the sites reach half.
        let mut acc = 0.0;
        let mut split = None;
        for (k, s) in lines.iter().enumerate().take(lines.len() - 1) {
            acc += s;
            if acc > 0.0 && acc < total && (split.is_none() || acc <= total / 2.0) {
                split = Some((k, acc));
            }
        }
        let Some((k, left)) = split else { continue };
        let coordinate = |o: usize| if axis == 0 { pos[o].0 } else { pos[o].1 };
        objects.sort_by(|&a, &b| coordinate(a).total_cmp(&coordinate(b)).then(a.cmp(&b)));
        let area: f64 = objects.iter().map(|&o| p.area[o]).sum();
        let goal = area * left / total;
        let mut taken = 0.0;
        let mut at = 0;
        while at < objects.len() && taken + p.area[objects[at]] / 2.0 <= goal {
            taken += p.area[objects[at]];
            at += 1;
        }
        let mid = lo + k;
        let (mut a, mut b) = (r, r);
        if axis == 0 {
            a.x1 = mid;
            b.x0 = mid + 1;
        } else {
            a.y1 = mid;
            b.y0 = mid + 1;
        }
        let (first, second) = objects.split_at_mut(at);
        // Each side's objects keep their order, mapped into its span.
        for (side, rect) in [(&mut *first, a), (&mut *second, b)] {
            let (lo, hi) = if axis == 0 { (rect.x0, rect.x1) } else { (rect.y0, rect.y1) };
            for &o in side.iter() {
                let v = if axis == 0 { &mut pos[o].0 } else { &mut pos[o].1 };
                *v = v.clamp(lo as f64, hi as f64);
            }
        }
        cut(p, g, a, first, pos);
        cut(p, g, b, second, pos);
        return;
    }
    // No cut divides the sites: all are on one tile.
    let (x, y) = (r.x0..=r.x1)
        .flat_map(|x| (r.y0..=r.y1).map(move |y| (x, y)))
        .find(|&(x, y)| g.at(x, y) > 0.0)
        .unwrap_or((r.x0, r.y0));
    for &o in objects.iter() {
        pos[o] = (x as f64, y as f64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chain_of_objects_lines_up_between_fixed_pins_and_spreads() {
        // Ten objects in a chain between pins at x = 0 and x = 20, on a grid
        // with one site per tile.
        let mut grid = Grid::new(21, 5);
        for y in 0..5 {
            for x in 0..21 {
                grid.add(x, y);
            }
        }
        let mut nets = vec![vec![Pin::Fixed(0.0, 2.0), Pin::Object(0, 0.0, 0.0)]];
        for o in 0..9 {
            nets.push(vec![Pin::Object(o, 0.0, 0.0), Pin::Object(o + 1, 0.0, 0.0)]);
        }
        nets.push(vec![Pin::Object(9, 0.0, 0.0), Pin::Fixed(20.0, 2.0)]);
        let mut p = Problem {
            kind: vec![0; 10],
            area: vec![1.0; 10],
            nets,
            arcs: vec![],
            grids: vec![grid],
        };
        let mut pos = vec![(10.0, 2.0); 10];
        let out = place(&mut p, &mut pos, None);
        for o in 0..9 {
            assert!(out[o].0 <= out[o + 1].0, "{out:?}");
        }
        let mut tiles: Vec<_> = out.iter().map(|&(x, y)| (x as i32, y as i32)).collect();
        tiles.sort();
        tiles.dedup();
        assert_eq!(tiles.len(), 10, "{out:?}");
    }

    #[test]
    fn spreading_moves_a_clump_onto_sites_only() {
        // Sites only in even columns: forty objects at one point spread
        // over at least forty sites, none on a column without sites.
        let mut grid = Grid::new(20, 20);
        for y in 0..20 {
            for x in (0..20).step_by(2) {
                grid.add(x, y);
            }
        }
        let p = Problem {
            kind: vec![0; 40],
            area: vec![1.0; 40],
            nets: vec![],
            arcs: vec![],
            grids: vec![grid],
        };
        let mut pos = vec![(9.0, 9.0); 40];
        let members: Vec<usize> = (0..40).collect();
        spread_kind(&p, 0, &members, &mut pos);
        let mut tiles: Vec<_> = pos.iter().map(|&(x, y)| (x as i32, y as i32)).collect();
        assert!(tiles.iter().all(|&(x, _)| x % 2 == 0), "{tiles:?}");
        tiles.sort();
        tiles.dedup();
        assert!(tiles.len() >= 36, "{tiles:?}");
    }
}
