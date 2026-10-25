//! LUT input pins after routing. The router picks each LUT input among a
//! lane's six pins, whose delays into the LUT differ by up to 515 ps (A1
//! slowest, A6 fastest), but it caps criticality, so connections near the
//! worst slack do not compete for the fast pins by how late they arrive.
//! Here the nets into a critical lane are routed again together, each to
//! the pin that leaves the lane's worst input the most slack.
use crate::hold::{branch, reorder};
use crate::routing::{RoutingGraph, VIRTUAL};
use crate::sta::edge_src;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use synth_core::{FxHashMap, FxHashSet};
use synth_route::{NetRequest, NetRoute};

/// Tiles around a lane and its nets' junctions a new branch may use.
const WINDOW: i16 = 6;
/// Nodes a search may settle.
const SEARCH: usize = 20_000;
/// The least predicted gain (ps) worth rerouting a lane for.
const MIN_GAIN: f64 = 10.0;

/// A lane's routed inputs: each net's request, virtual sink and slack (ps),
/// and the lane's pins A1-A6 (none where the router may not use a pin).
pub struct Lane {
    pub inputs: Vec<(usize, u32, f64)>,
    pub pins: [Option<u32>; 6],
}

/// Delays (ps) from a route's source to each node of its tree.
fn tree_delays(graph: &RoutingGraph, route: &NetRoute, source: u32) -> FxHashMap<u32, f64> {
    let mut at: FxHashMap<u32, f64> = FxHashMap::default();
    at.insert(source, 0.0);
    for &e in &route.edges {
        let d = at.get(&edge_src(graph, e)).copied().unwrap_or(0.0);
        at.insert(graph.dst[e as usize], d + f64::from(graph.delay[e as usize]));
    }
    at
}

/// Least-delay paths from a tree's nodes (each at its delay from the
/// source) over free nodes in a box, until every target is settled.
fn search(
    graph: &RoutingGraph,
    free: &dyn Fn(u32) -> bool,
    sources: &FxHashMap<u32, f64>,
    targets: &[u32],
) -> FxHashMap<u32, (f64, u32)> {
    let mut dist: FxHashMap<u32, (f64, u32)> = FxHashMap::default();
    let mut heap = BinaryHeap::new();
    for (&n, &d) in sources {
        dist.insert(n, (d, u32::MAX));
        heap.push(Reverse(((d * 16.0) as u64, n)));
    }
    let mut settled = FxHashSet::default();
    let mut left = targets.len();
    while let Some(Reverse((_, node))) = heap.pop() {
        if !settled.insert(node) || settled.len() > SEARCH {
            continue;
        }
        if targets.contains(&node) {
            left -= 1;
            if left == 0 {
                break;
            }
            // A pin leads only into its lane's virtual sinks.
            continue;
        }
        let d = dist[&node].0;
        for e in graph.offsets[node as usize]..graph.offsets[node as usize + 1] {
            let next = graph.dst[e as usize];
            if graph.pip[e as usize].0 == VIRTUAL || sources.contains_key(&next) {
                continue;
            }
            if !targets.contains(&next) && !free(next) {
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
}

/// The edges of a search's path to `node`, from the tree.
fn path_to(graph: &RoutingGraph, dist: &FxHashMap<u32, (f64, u32)>, mut node: u32) -> Vec<u32> {
    let mut edges = Vec::new();
    while let Some(&(_, e)) = dist.get(&node).filter(|(_, e)| *e != u32::MAX) {
        edges.push(e);
        node = edge_src(graph, e);
    }
    edges.reverse();
    edges
}

/// The virtual edge from pin node `pin` into virtual sink `sink`.
fn into_sink(graph: &RoutingGraph, blocked: &FxHashSet<u32>, pin: u32, sink: u32) -> Option<u32> {
    (graph.offsets[pin as usize]..graph.offsets[pin as usize + 1])
        .find(|&e| graph.dst[e as usize] == sink && !blocked.contains(&e))
}

/// The assignment of inputs to distinct pins that leaves the least slack
/// the most, by each input's slack on each pin; and that slack.
fn assign(slack: &[[f64; 6]]) -> (Vec<usize>, f64) {
    fn go(
        i: usize,
        slack: &[[f64; 6]],
        used: &mut [bool; 6],
        cur: f64,
        at: &mut Vec<usize>,
        best: &mut (Vec<usize>, f64),
    ) {
        if cur <= best.1 {
            return;
        }
        if i == slack.len() {
            *best = (at.clone(), cur);
            return;
        }
        for j in 0..6 {
            if used[j] || !slack[i][j].is_finite() {
                continue;
            }
            used[j] = true;
            at.push(j);
            go(i + 1, slack, used, cur.min(slack[i][j]), at, best);
            at.pop();
            used[j] = false;
        }
    }
    let mut best = (Vec::new(), f64::NEG_INFINITY);
    go(0, slack, &mut [false; 6], f64::INFINITY, &mut Vec::new(), &mut best);
    best
}

/// Routes a lane's inputs again, each to the pin that leaves the lane the
/// most slack by the graph's delays. `used` counts the nets on each node.
/// The predicted gain in the lane's least slack, if it was rerouted.
pub fn reassign(
    graph: &RoutingGraph,
    requests: &[NetRequest],
    routes: &mut [NetRoute],
    used: &mut [u16],
    blocked: &FxHashSet<u32>,
    lane: &Lane,
) -> Option<f64> {
    let pins: Vec<u32> = lane.pins.iter().flatten().copied().collect();
    // Each input's branch into its sink, freed for the search.
    let mut branches = Vec::new();
    for &(r, sink, _) in &lane.inputs {
        let (edges, junction) = branch(graph, &requests[r], &routes[r], sink)?;
        branches.push((edges, junction));
    }
    let saved: Vec<NetRoute> = lane.inputs.iter().map(|&(r, ..)| routes[r].clone()).collect();
    // Each input's delay to its pin now (the branch's first edge is the
    // virtual one from the pin).
    let mut old = Vec::new();
    for (&(r, ..), (edges, _)) in lane.inputs.iter().zip(&branches) {
        let delays = tree_delays(graph, &routes[r], requests[r].source);
        old.push(*delays.get(&edge_src(graph, *edges.first()?))?);
    }
    let restore = |routes: &mut [NetRoute], used: &mut [u16]| {
        for (&(r, ..), route) in lane.inputs.iter().zip(&saved) {
            for &e in &routes[r].edges {
                used[graph.dst[e as usize] as usize] -= 1;
            }
            routes[r] = route.clone();
            for &e in &routes[r].edges {
                used[graph.dst[e as usize] as usize] += 1;
            }
        }
    };
    for (&(r, ..), (edges, _)) in lane.inputs.iter().zip(&branches) {
        let removed: FxHashSet<u32> = edges.iter().copied().collect();
        for &e in edges {
            used[graph.dst[e as usize] as usize] -= 1;
        }
        routes[r].edges.retain(|e| !removed.contains(e));
    }
    // The box around the lane and its inputs' junctions.
    let (lx, ly) = (graph.x[pins[0] as usize], graph.y[pins[0] as usize]);
    let (mut x0, mut x1, mut y0, mut y1) = (lx, lx, ly, ly);
    for (_, j) in &branches {
        let (x, y) = (graph.x[*j as usize], graph.y[*j as usize]);
        (x0, x1, y0, y1) = (x0.min(x), x1.max(x), y0.min(y), y1.max(y));
    }
    let (x0, x1, y0, y1) = (x0 - WINDOW, x1 + WINDOW, y0 - WINDOW, y1 + WINDOW);
    let inside = |n: u32| {
        let k = n as usize;
        (x0..=x1).contains(&graph.x[k]) && (y0..=y1).contains(&graph.y[k])
    };
    // A branch leaves the net's tree from its source or a node in the box
    // other than a sink.
    let sources = |routes: &[NetRoute], r: usize| -> FxHashMap<u32, f64> {
        let req = &requests[r];
        tree_delays(graph, &routes[r], req.source)
            .into_iter()
            .filter(|&(n, _)| n == req.source || inside(n) && !req.sinks.contains(&n))
            .collect()
    };
    // Each input's slack on each free pin, by the delays of its fastest
    // path there now.
    let mut slack = Vec::new();
    for (i, &(r, sink, s)) in lane.inputs.iter().enumerate() {
        let free =
            |n: u32| used[n as usize] == 0 && inside(n) && graph.cost[n as usize].is_finite();
        let open: Vec<u32> = pins.iter().copied().filter(|&p| used[p as usize] == 0).collect();
        let dist = search(graph, &free, &sources(routes, r), &open);
        let mut row = [f64::NAN; 6];
        for (j, pin) in lane.pins.iter().enumerate() {
            let Some(pin) = *pin else { continue };
            if into_sink(graph, blocked, pin, sink).is_none() {
                continue;
            }
            if let Some(&(d, _)) = dist.get(&pin).filter(|_| open.contains(&pin)) {
                row[j] = s + old[i] - d;
            }
        }
        slack.push(row);
    }
    let before = lane.inputs.iter().map(|i| i.2).fold(f64::INFINITY, f64::min);
    let (at, after) = assign(&slack);
    if at.len() != lane.inputs.len() || after < before + MIN_GAIN {
        restore(routes, used);
        return None;
    }
    // Commit, the least slack first, each path searched again around the
    // ones before it.
    let mut order: Vec<usize> = (0..lane.inputs.len()).collect();
    order.sort_by(|&a, &b| slack[a][at[a]].total_cmp(&slack[b][at[b]]));
    let mut least = f64::INFINITY;
    for i in order {
        let (r, sink, s) = lane.inputs[i];
        let pin = lane.pins[at[i]].expect("assigned pins exist");
        let free =
            |n: u32| used[n as usize] == 0 && inside(n) && graph.cost[n as usize].is_finite();
        let dist = search(graph, &free, &sources(routes, r), &[pin]);
        let (Some(&(d, _)), Some(last)) = (dist.get(&pin), into_sink(graph, blocked, pin, sink))
        else {
            restore(routes, used);
            return None;
        };
        least = least.min(s + old[i] - d);
        let mut path = path_to(graph, &dist, pin);
        path.push(last);
        for &e in &path {
            used[graph.dst[e as usize] as usize] += 1;
        }
        routes[r].edges.extend(path);
        reorder(graph, &mut routes[r], requests[r].source);
    }
    if least < before + MIN_GAIN {
        restore(routes, used);
        return None;
    }
    Some(least - before)
}
