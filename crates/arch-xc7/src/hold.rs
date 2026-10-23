//! Hold repair after routing. A connection whose fastest path reaches a
//! flip-flop before the capture clock's hold window closes (mostly paths
//! launched one row of clock regions below their capture, whose clock
//! arrives about 350 ps later) is rerouted through unused wires near its
//! sink until it is slow enough, as Vivado's router does.
use crate::routing::RoutingGraph;
use crate::sta::edge_src;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use synth_core::{FxHashMap, FxHashSet};
use synth_route::{NetRequest, NetRoute};

/// Tiles around a branch's ends a detour may use.
const WINDOW: i16 = 6;
/// Nodes a search may settle.
const SEARCH: usize = 20_000;
/// Candidate turning points tried per detour.
const CANDIDATES: usize = 64;
/// Branches up from the sink a detour may replace.
const UPSTREAM: usize = 3;

/// The routing nodes by tile.
pub struct Index(FxHashMap<(i16, i16), Vec<u32>>);
impl Index {
    pub fn new(graph: &RoutingGraph) -> Self {
        let mut tiles: FxHashMap<(i16, i16), Vec<u32>> = FxHashMap::default();
        for node in 0..graph.x.len() {
            tiles.entry((graph.x[node], graph.y[node])).or_default().push(node as u32);
        }
        Index(tiles)
    }
}

/// Reroutes net `net` to `sink` through nodes no net uses, so that it is
/// at least `extra` ps slower by the graph's edge delays and as little more
/// as found. `used` counts the nets on each node. Where the sink's own
/// branch leaves no room (it hangs off a node feeding other sinks, like a
/// bypass input feeding both flip-flops of a lane), the branch above that
/// node is rerouted instead, slowing its other sinks too. Whether it found
/// a detour.
#[allow(clippy::too_many_arguments)]
pub fn detour(
    graph: &RoutingGraph,
    index: &Index,
    requests: &[NetRequest],
    routes: &mut [NetRoute],
    used: &mut [u16],
    blocked: &FxHashSet<u32>,
    net: usize,
    sink: u32,
    extra: f64,
) -> bool {
    let mut target = sink;
    for _ in 0..UPSTREAM {
        if detour_to(graph, index, requests, routes, used, blocked, net, target, extra) {
            return true;
        }
        match branch(graph, &requests[net], &routes[net], target) {
            Some((_, up)) if up != requests[net].source => target = up,
            _ => return false,
        }
    }
    false
}

/// The edges from `sink` back to where net `req`'s tree forks, reaches
/// another sink or starts, and that node.
fn branch(
    graph: &RoutingGraph,
    req: &NetRequest,
    route: &NetRoute,
    sink: u32,
) -> Option<(Vec<u32>, u32)> {
    let mut parent: FxHashMap<u32, u32> = FxHashMap::default();
    let mut children: FxHashMap<u32, u32> = FxHashMap::default();
    for &e in &route.edges {
        parent.insert(graph.dst[e as usize], e);
        *children.entry(edge_src(graph, e)).or_default() += 1;
    }
    let mut edges = Vec::new();
    let mut n = sink;
    loop {
        let &e = parent.get(&n)?;
        edges.push(e);
        let p = edge_src(graph, e);
        if p == req.source || children.get(&p).copied().unwrap_or(0) > 1 || req.sinks.contains(&p) {
            return Some((edges, p));
        }
        n = p;
    }
}

/// [`detour`] around the branch to `sink` alone.
#[allow(clippy::too_many_arguments)]
fn detour_to(
    graph: &RoutingGraph,
    index: &Index,
    requests: &[NetRequest],
    routes: &mut [NetRoute],
    used: &mut [u16],
    blocked: &FxHashSet<u32>,
    net: usize,
    sink: u32,
    extra: f64,
) -> bool {
    let route = &routes[net];
    let source = requests[net].source;
    let Some((branch, junction)) = branch(graph, &requests[net], route, sink) else {
        return false;
    };
    let old: f64 = branch.iter().map(|&e| f64::from(graph.delay[e as usize])).sum();
    let target = old + extra;
    let freed: FxHashSet<u32> = branch.iter().map(|&e| graph.dst[e as usize]).collect();
    // The box around the branch's ends.
    let (j, k) = (junction as usize, sink as usize);
    let (x0, x1) = (graph.x[j].min(graph.x[k]) - WINDOW, graph.x[j].max(graph.x[k]) + WINDOW);
    let (y0, y1) = (graph.y[j].min(graph.y[k]) - WINDOW, graph.y[j].max(graph.y[k]) + WINDOW);
    let free = |node: u32, avoid: &FxHashSet<u32>| {
        let k = node as usize;
        (used[k] == 0 || freed.contains(&node))
            && !avoid.contains(&node)
            && (x0..=x1).contains(&graph.x[k])
            && (y0..=y1).contains(&graph.y[k])
            && graph.cost[k].is_finite()
    };
    // Least-delay paths from `from` over free nodes, to `to` if given.
    let search = |from: u32, to: Option<u32>, avoid: &FxHashSet<u32>| {
        let mut dist: FxHashMap<u32, (f64, u32)> = FxHashMap::default();
        let mut heap = BinaryHeap::new();
        heap.push(Reverse((0u64, from)));
        let mut settled = FxHashSet::default();
        dist.insert(from, (0.0, u32::MAX));
        while let Some(Reverse((_, node))) = heap.pop() {
            if !settled.insert(node) || settled.len() > SEARCH {
                continue;
            }
            if Some(node) == to {
                break;
            }
            let d = dist[&node].0;
            let (a, b) = (graph.offsets[node as usize], graph.offsets[node as usize + 1]);
            for e in a..b {
                let next = graph.dst[e as usize];
                // Only the sink ends a path: other sinks and the net's own
                // tree are not crossed.
                let open = if Some(next) == to { true } else { next != sink && free(next, avoid) };
                if !open || blocked.contains(&e) || next == from {
                    continue;
                }
                let nd = d + f64::from(graph.delay[e as usize]);
                if dist.get(&next).is_none_or(|&(old, _)| nd < old) {
                    dist.insert(next, (nd, e));
                    heap.push(Reverse(((nd * 16.0) as u64, next)));
                }
            }
        }
        dist
    };
    let path_to = |dist: &FxHashMap<u32, (f64, u32)>, mut node: u32| {
        let mut edges = Vec::new();
        while let Some(&(_, e)) = dist.get(&node).filter(|(_, e)| *e != u32::MAX) {
            edges.push(e);
            node = edge_src(graph, e);
        }
        edges.reverse();
        edges
    };
    let tree: FxHashSet<u32> = route
        .edges
        .iter()
        .map(|&e| graph.dst[e as usize])
        .chain([source])
        .filter(|n| !freed.contains(n))
        .collect();
    let first = search(junction, None, &tree);
    // Least delays to the sink from the free nodes in the box, over the
    // edges between them.
    let mut into: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
    for x in x0..=x1 {
        for y in y0..=y1 {
            for &node in index.0.get(&(x, y)).into_iter().flatten() {
                if !free(node, &tree) || node == sink {
                    continue;
                }
                let (a, b) = (graph.offsets[node as usize], graph.offsets[node as usize + 1]);
                for e in (a..b).filter(|e| !blocked.contains(e)) {
                    let next = graph.dst[e as usize];
                    if next == sink || free(next, &tree) {
                        into.entry(next).or_default().push(e);
                    }
                }
            }
        }
    }
    let mut back: FxHashMap<u32, (f64, u32)> = FxHashMap::default();
    back.insert(sink, (0.0, u32::MAX));
    let mut heap = BinaryHeap::new();
    heap.push(Reverse((0u64, sink)));
    let mut settled = FxHashSet::default();
    while let Some(Reverse((_, node))) = heap.pop() {
        if !settled.insert(node) || settled.len() > SEARCH {
            continue;
        }
        let d = back[&node].0;
        for &e in into.get(&node).into_iter().flatten() {
            let prev = edge_src(graph, e);
            let nd = d + f64::from(graph.delay[e as usize]);
            if back.get(&prev).is_none_or(|&(old, _)| nd < old) {
                back.insert(prev, (nd, e));
                heap.push(Reverse(((nd * 16.0) as u64, prev)));
            }
        }
    }
    // Turning points by the delay through them, the least above the
    // target first; the two halves may not cross.
    let mut turns: Vec<(f64, u32)> = first
        .iter()
        .filter(|&(&n, _)| n != junction && n != sink)
        .filter_map(|(&n, &(d, _))| back.get(&n).map(|&(b, _)| (d + b, n)))
        .filter(|&(total, _)| total >= target)
        .collect();
    turns.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut best: Option<(f64, Vec<u32>)> = None;
    for &(total, turn) in turns.iter().take(CANDIDATES) {
        let lead = path_to(&first, turn);
        let mut tail = Vec::new();
        let mut n = turn;
        while let Some(&(_, e)) = back.get(&n).filter(|(_, e)| *e != u32::MAX) {
            tail.push(e);
            n = graph.dst[e as usize];
        }
        let mut seen: FxHashSet<u32> = [junction].into_iter().collect();
        if lead.iter().chain(&tail).all(|&e| seen.insert(graph.dst[e as usize])) {
            best = Some((total, lead.into_iter().chain(tail).collect()));
            break;
        }
    }
    let Some((_, path)) = best else { return false };
    let removed: FxHashSet<u32> = branch.iter().copied().collect();
    for &e in &branch {
        used[graph.dst[e as usize] as usize] -= 1;
    }
    let route = &mut routes[net];
    route.edges.retain(|e| !removed.contains(e));
    for &e in &path {
        used[graph.dst[e as usize] as usize] += 1;
    }
    route.edges.extend(path);
    // Edges below a rerouted fork came before the path now reaching it:
    // order the tree from its source again.
    let mut by_src: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
    for &e in &route.edges {
        by_src.entry(edge_src(graph, e)).or_default().push(e);
    }
    let mut order = Vec::with_capacity(route.edges.len());
    let mut stack = vec![source];
    while let Some(n) = stack.pop() {
        for e in by_src.remove(&n).unwrap_or_default() {
            order.push(e);
            stack.push(graph.dst[e as usize]);
        }
    }
    route.edges = order;
    true
}
