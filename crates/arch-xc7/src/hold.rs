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

/// Tiles around the sink a detour may use.
const WINDOW: i16 = 6;
/// Nodes a search may settle.
const SEARCH: usize = 20_000;
/// Candidate turning points tried per detour.
const CANDIDATES: usize = 24;

/// Reroutes the branch of net `net` that leads to `sink` alone through
/// nodes no net uses, so that it is at least `extra` ps slower by the
/// graph's edge delays and as little more as found. `used` counts the nets
/// on each node. Whether it found a detour.
#[allow(clippy::too_many_arguments)]
pub fn detour(
    graph: &RoutingGraph,
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
    let mut parent: FxHashMap<u32, u32> = FxHashMap::default();
    let mut children: FxHashMap<u32, u32> = FxHashMap::default();
    for &e in &route.edges {
        parent.insert(graph.dst[e as usize], e);
        *children.entry(edge_src(graph, e)).or_default() += 1;
    }
    let sinks: FxHashSet<u32> = requests[net].sinks.iter().copied().collect();
    // The branch: back from the sink to where the tree forks, reaches
    // another sink or starts.
    let mut branch = Vec::new();
    let mut n = sink;
    let junction = loop {
        let Some(&e) = parent.get(&n) else { return false };
        branch.push(e);
        let p = edge_src(graph, e);
        if p == source || children.get(&p).copied().unwrap_or(0) > 1 || sinks.contains(&p) {
            break p;
        }
        n = p;
    };
    let old: f64 = branch.iter().map(|&e| f64::from(graph.delay[e as usize])).sum();
    let target = old + extra;
    let freed: FxHashSet<u32> = branch.iter().map(|&e| graph.dst[e as usize]).collect();
    let (sx, sy) = (graph.x[sink as usize], graph.y[sink as usize]);
    let free = |node: u32, avoid: &FxHashSet<u32>| {
        let k = node as usize;
        (used[k] == 0 || freed.contains(&node))
            && !avoid.contains(&node)
            && (graph.x[k] - sx).abs() <= WINDOW
            && (graph.y[k] - sy).abs() <= WINDOW
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
    // Turning points whose delay leaves a short hop to the sink.
    let mut turns: Vec<(f64, u32)> = first
        .iter()
        .filter(|&(&n, &(d, _))| n != junction && d < target)
        .map(|(&n, &(d, _))| (d, n))
        .collect();
    turns.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut best: Option<(f64, Vec<u32>)> = None;
    for &(_, turn) in turns.iter().take(CANDIDATES) {
        let lead = path_to(&first, turn);
        let mut avoid = tree.clone();
        avoid.extend(lead.iter().map(|&e| edge_src(graph, e)));
        let second = search(turn, Some(sink), &avoid);
        let Some(&(d, _)) = second.get(&sink) else { continue };
        let total = first[&turn].0 + d;
        let tail = path_to(&second, sink);
        // The two halves may not cross.
        let mut seen: FxHashSet<u32> = lead.iter().map(|&e| graph.dst[e as usize]).collect();
        if tail
            .iter()
            .any(|&e| !seen.insert(graph.dst[e as usize]) && graph.dst[e as usize] != sink)
        {
            continue;
        }
        if total >= target && best.as_ref().is_none_or(|(t, _)| total < *t) {
            best = Some((total, lead.into_iter().chain(tail).collect()));
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
    true
}
