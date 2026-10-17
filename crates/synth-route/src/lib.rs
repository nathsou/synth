//! A negotiated-congestion router (PathFinder, McMurchie & Ebeling 1995)
//! with A* search, over a device routing graph in compressed sparse row form.
//!
//! Each routing node can carry one net. Nets are routed one after another;
//! overused nodes get more expensive each iteration (present and history
//! congestion costs) until every node is used by at most one net. Only nets
//! touching overused nodes are ripped up and rerouted after the first pass.
//!
//! The cost of entering a node blends congestion and delay according to the
//! criticality of the connection being routed, so timing-critical
//! connections prefer fast paths.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// A routing graph: nodes with positions and base costs, directed edges
/// with intrinsic delays.
#[derive(Debug, Clone, Copy)]
pub struct Graph<'a> {
    /// `edges` leaving node `n` are indices `offsets[n]..offsets[n + 1]`.
    pub offsets: &'a [u32],
    pub edge_dst: &'a [u32],
    /// Delay of each edge (arbitrary units, e.g. ps).
    pub edge_delay: &'a [f32],
    pub node_x: &'a [i16],
    pub node_y: &'a [i16],
    /// Cost of using each node, independent of congestion (> 0).
    pub node_cost: &'a [f32],
}

impl Graph<'_> {
    pub fn num_nodes(&self) -> usize {
        self.node_x.len()
    }
}

/// One net to route: a source node and its sink nodes.
#[derive(Debug, Clone)]
pub struct NetRequest {
    pub source: u32,
    pub sinks: Vec<u32>,
    /// Timing criticality of each sink in `[0, 1]` (empty = all zero).
    pub criticality: Vec<f32>,
}

/// Router tuning knobs.
#[derive(Debug, Clone)]
pub struct RouteOptions {
    pub max_iterations: usize,
    /// Initial present-congestion factor and its growth per iteration.
    pub present_factor: f32,
    pub present_growth: f32,
    pub history_factor: f32,
    /// Weight of the A* distance estimate (cost per tile of distance).
    pub astar_cost_per_tile: f32,
    /// Delay per tile used in the A* estimate for critical connections.
    pub astar_delay_per_tile: f32,
    /// Scale converting delay units into cost units.
    pub delay_scale: f32,
    /// Extra tiles around a net's bounding box that searches may use.
    pub bbox_margin: i16,
}

impl Default for RouteOptions {
    fn default() -> Self {
        RouteOptions {
            max_iterations: 200,
            present_factor: 0.5,
            present_growth: 1.6,
            history_factor: 0.4,
            astar_cost_per_tile: 0.1,
            astar_delay_per_tile: 0.0,
            delay_scale: 0.001,
            bbox_margin: 3,
        }
    }
}

/// The routing of one net: the edges of its tree, in the order they were
/// added (each edge's source is the net source or the destination of an
/// earlier edge).
#[derive(Debug, Clone, Default)]
pub struct NetRoute {
    pub edges: Vec<u32>,
}

/// Router failure.
#[derive(Debug, Clone)]
pub enum RouteError {
    /// A sink is not reachable from its source at all.
    Unroutable { net: usize, sink: u32 },
    /// Congestion could not be resolved within the iteration limit. From
    /// the iteration with the least overuse: the overused nodes and the nets
    /// using them.
    Congested { overused_nodes: usize, nodes: Vec<u32>, nets: Vec<usize> },
}

impl std::fmt::Display for RouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouteError::Unroutable { net, sink } => {
                write!(f, "net {net}: sink node {sink} is unreachable")
            }
            RouteError::Congested { overused_nodes, .. } => {
                write!(f, "routing congestion not resolved ({overused_nodes} nodes overused)")
            }
        }
    }
}

impl std::error::Error for RouteError {}

#[derive(Copy, Clone, PartialEq)]
struct QueueEntry {
    f: f32,
    g: f32,
    node: u32,
}

impl Eq for QueueEntry {}

impl Ord for QueueEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Min-heap on f; break ties on node index for determinism.
        other.f.partial_cmp(&self.f).unwrap_or(Ordering::Equal).then(other.node.cmp(&self.node))
    }
}

impl PartialOrd for QueueEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Per-node search and congestion state.
struct State {
    occupancy: Vec<u16>,
    history: Vec<f32>,
    /// Best cost found in the current search, valid when `stamp == search_gen`.
    best: Vec<f32>,
    /// Edge used to reach the node in the current search (`u32::MAX` for roots).
    via: Vec<u32>,
    stamp: Vec<u32>,
    search_gen: u32,
    /// Nodes belonging to the tree of the net being routed.
    in_tree: Vec<u32>,
    tree_gen: u32,
    /// Reverse edge lookup: source node of each edge.
    edge_src: Vec<u32>,
}

/// Routes `nets` on `graph`. Returns one route per net.
pub fn route(
    graph: Graph<'_>,
    nets: &[NetRequest],
    opts: &RouteOptions,
) -> Result<(Vec<NetRoute>, RouteStats), RouteError> {
    let n = graph.num_nodes();
    let mut edge_src = vec![0u32; graph.edge_dst.len()];
    for node in 0..n {
        for e in graph.offsets[node]..graph.offsets[node + 1] {
            edge_src[e as usize] = node as u32;
        }
    }
    let mut st = State {
        occupancy: vec![0; n],
        history: vec![0.0; n],
        best: vec![0.0; n],
        via: vec![u32::MAX; n],
        stamp: vec![0; n],
        search_gen: 0,
        in_tree: vec![0; n],
        tree_gen: 0,
        edge_src,
    };
    let mut routes: Vec<NetRoute> = vec![NetRoute::default(); nets.len()];
    let mut pres_fac = opts.present_factor;
    let mut stats = RouteStats::default();

    // Sources and sinks of other nets are reserved: never route through them.
    let mut reserved = vec![u32::MAX; n];
    for (i, net) in nets.iter().enumerate() {
        reserved[net.source as usize] = i as u32;
        for &s in &net.sinks {
            reserved[s as usize] = i as u32;
        }
    }

    let mut best: Option<(Vec<u32>, Vec<usize>)> = None;
    let mut to_route: Vec<usize> = (0..nets.len()).collect();
    // Route nets with many sinks first: they are hardest to fit later.
    to_route.sort_by_key(|&i| std::cmp::Reverse(nets[i].sinks.len()));
    for iter in 0..opts.max_iterations {
        stats.iterations = iter + 1;
        for &i in &to_route {
            // Rip up.
            for &e in &routes[i].edges {
                let d = graph.edge_dst[e as usize] as usize;
                st.occupancy[d] -= 1;
            }
            routes[i].edges.clear();
            route_net(
                &graph,
                &nets[i],
                i as u32,
                &reserved,
                &mut st,
                opts,
                pres_fac,
                &mut routes[i],
            )?;
            for &e in &routes[i].edges {
                let d = graph.edge_dst[e as usize] as usize;
                st.occupancy[d] += 1;
            }
        }
        let overused: Vec<usize> = (0..n).filter(|&k| st.occupancy[k] > 1).collect();
        stats.overused_history.push(overused.len());
        if best.as_ref().is_none_or(|b: &(Vec<u32>, Vec<usize>)| overused.len() < b.0.len()) {
            let set: std::collections::HashSet<usize> = overused.iter().copied().collect();
            let users = (0..nets.len())
                .filter(|&i| {
                    routes[i]
                        .edges
                        .iter()
                        .any(|&e| set.contains(&(graph.edge_dst[e as usize] as usize)))
                })
                .collect();
            best = Some((overused.iter().map(|&k| k as u32).collect(), users));
        }
        if overused.is_empty() {
            stats.wire_nodes = routes.iter().map(|r| r.edges.len()).sum();
            return Ok((routes, stats));
        }
        for &k in &overused {
            st.history[k] += opts.history_factor * (st.occupancy[k] - 1) as f32;
        }
        // Bounded so that costs stay finite on hopeless nets.
        pres_fac = (pres_fac * opts.present_growth).min(1e6);
        // Reroute only nets using overused nodes.
        let mut is_over = vec![false; n];
        for &k in &overused {
            is_over[k] = true;
        }
        to_route = (0..nets.len())
            .filter(|&i| {
                routes[i].edges.iter().any(|&e| is_over[graph.edge_dst[e as usize] as usize])
            })
            .collect();
    }
    let (nodes, nets) = best.unwrap_or_default();
    let overused_nodes = nodes.len();
    Err(RouteError::Congested { overused_nodes, nodes, nets })
}

/// Statistics of a routing run.
#[derive(Debug, Clone, Default)]
pub struct RouteStats {
    pub iterations: usize,
    pub overused_history: Vec<usize>,
    pub wire_nodes: usize,
}

#[allow(clippy::too_many_arguments)]
fn route_net(
    graph: &Graph<'_>,
    net: &NetRequest,
    net_id: u32,
    reserved: &[u32],
    st: &mut State,
    opts: &RouteOptions,
    pres_fac: f32,
    out: &mut NetRoute,
) -> Result<(), RouteError> {
    st.tree_gen += 1;
    let tg = st.tree_gen;
    st.in_tree[net.source as usize] = tg;
    // Bounding box of the net plus a margin.
    let (mut x0, mut y0, mut x1, mut y1) =
        (graph.node_x[net.source as usize], graph.node_y[net.source as usize], 0, 0);
    x1 = x1.max(x0);
    y1 = y1.max(y0);
    for &s in &net.sinks {
        x0 = x0.min(graph.node_x[s as usize]);
        x1 = x1.max(graph.node_x[s as usize]);
        y0 = y0.min(graph.node_y[s as usize]);
        y1 = y1.max(graph.node_y[s as usize]);
    }
    let mut margin = opts.bbox_margin;

    // Route the farthest sinks first.
    let mut order: Vec<usize> = (0..net.sinks.len()).collect();
    let sx = graph.node_x[net.source as usize];
    let sy = graph.node_y[net.source as usize];
    order.sort_by_key(|&k| {
        let s = net.sinks[k] as usize;
        std::cmp::Reverse((graph.node_x[s] - sx).abs() + (graph.node_y[s] - sy).abs())
    });
    let mut tree_nodes: Vec<u32> = vec![net.source];
    let mut heap = BinaryHeap::new();
    for k in order {
        let sink = net.sinks[k];
        if st.in_tree[sink as usize] == tg {
            continue;
        }
        let crit = net.criticality.get(k).copied().unwrap_or(0.0).clamp(0.0, 0.99);
        let tx = graph.node_x[sink as usize];
        let ty = graph.node_y[sink as usize];
        loop {
            st.search_gen += 1;
            let search_gen = st.search_gen;
            heap.clear();
            let est = |node: usize| {
                let d = ((graph.node_x[node] - tx).abs() + (graph.node_y[node] - ty).abs()) as f32;
                d * ((1.0 - crit) * opts.astar_cost_per_tile
                    + crit * opts.astar_delay_per_tile * opts.delay_scale)
            };
            for &t in &tree_nodes {
                st.best[t as usize] = 0.0;
                st.via[t as usize] = u32::MAX;
                st.stamp[t as usize] = search_gen;
                heap.push(QueueEntry { f: est(t as usize), g: 0.0, node: t });
            }
            let (bx0, by0, bx1, by1) = (x0 - margin, y0 - margin, x1 + margin, y1 + margin);
            let mut found = false;
            while let Some(QueueEntry { g, node, .. }) = heap.pop() {
                if node == sink {
                    found = true;
                    break;
                }
                if st.stamp[node as usize] == search_gen && g > st.best[node as usize] {
                    continue;
                }
                for e in graph.offsets[node as usize]..graph.offsets[node as usize + 1] {
                    let d = graph.edge_dst[e as usize];
                    let du = d as usize;
                    let r = reserved[du];
                    if r != u32::MAX && r != net_id {
                        continue;
                    }
                    if st.in_tree[du] == tg {
                        continue;
                    }
                    let (nx, ny) = (graph.node_x[du], graph.node_y[du]);
                    if d != sink && (nx < bx0 || nx > bx1 || ny < by0 || ny > by1) {
                        continue;
                    }
                    let occ = st.occupancy[du] as f32;
                    let present = 1.0 + pres_fac * occ;
                    let congestion = (graph.node_cost[du] + st.history[du]) * present;
                    let delay = graph.edge_delay[e as usize] * opts.delay_scale;
                    let cost = (1.0 - crit) * congestion + crit * delay;
                    let ng = g + cost;
                    if st.stamp[du] != search_gen || ng < st.best[du] {
                        st.stamp[du] = search_gen;
                        st.best[du] = ng;
                        st.via[du] = e;
                        heap.push(QueueEntry { f: ng + est(du), g: ng, node: d });
                    }
                }
            }
            if found {
                break;
            }
            // Retry with a larger box before giving up.
            if margin > 1000 {
                return Err(RouteError::Unroutable { net: net_id as usize, sink });
            }
            margin = if margin < 8 { 8 } else { margin * 4 };
        }
        // Trace back to the tree.
        let mut path = Vec::new();
        let mut cur = sink;
        while st.in_tree[cur as usize] != tg {
            let e = st.via[cur as usize];
            path.push(e);
            cur = st.edge_src[e as usize];
        }
        for &e in path.iter().rev() {
            let d = graph.edge_dst[e as usize];
            st.in_tree[d as usize] = tg;
            tree_nodes.push(d);
            out.edges.push(e);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    type GridData = (Vec<u32>, Vec<u32>, Vec<f32>, Vec<i16>, Vec<i16>, Vec<f32>);

    /// A small grid graph: nodes at (x, y), edges to 4 neighbours.
    fn grid(w: i16, h: i16) -> GridData {
        let id = |x: i16, y: i16| (y * w + x) as u32;
        let (mut offsets, mut dst, mut delay, mut xs, mut ys) =
            (vec![0], vec![], vec![], vec![], vec![]);
        for y in 0..h {
            for x in 0..w {
                xs.push(x);
                ys.push(y);
                for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                    let (nx, ny) = (x + dx, y + dy);
                    if nx >= 0 && ny >= 0 && nx < w && ny < h {
                        dst.push(id(nx, ny));
                        delay.push(100.0);
                    }
                }
                offsets.push(dst.len() as u32);
            }
        }
        let cost = vec![1.0; xs.len()];
        (offsets, dst, delay, xs, ys, cost)
    }

    #[test]
    fn routes_two_overlapping_nets() {
        let (o, d, dl, x, y, c) = grid(5, 5);
        let g = Graph {
            offsets: &o,
            edge_dst: &d,
            edge_delay: &dl,
            node_x: &x,
            node_y: &y,
            node_cost: &c,
        };
        // Both nets want the middle row; the outer one must detour.
        let nets = vec![
            NetRequest { source: 10, sinks: vec![14], criticality: vec![] },
            NetRequest { source: 11, sinks: vec![13], criticality: vec![] },
        ];
        let (routes, stats) = route(g, &nets, &RouteOptions::default()).unwrap();
        assert!(stats.iterations >= 1);
        let nodes = |r: &NetRoute| {
            r.edges.iter().map(|&e| d[e as usize]).collect::<std::collections::HashSet<_>>()
        };
        assert!(nodes(&routes[0]).is_disjoint(&nodes(&routes[1])));
        assert!(nodes(&routes[0]).contains(&14));
        assert!(nodes(&routes[1]).contains(&13));
    }

    #[test]
    fn multi_sink_tree() {
        let (o, d, dl, x, y, c) = grid(6, 6);
        let g = Graph {
            offsets: &o,
            edge_dst: &d,
            edge_delay: &dl,
            node_x: &x,
            node_y: &y,
            node_cost: &c,
        };
        let nets = vec![NetRequest { source: 0, sinks: vec![5, 30, 35], criticality: vec![] }];
        let (routes, _) = route(g, &nets, &RouteOptions::default()).unwrap();
        // A tree: every node appears once.
        let mut seen = std::collections::HashSet::new();
        for &e in &routes[0].edges {
            assert!(seen.insert(d[e as usize]));
        }
        assert!(seen.contains(&5) && seen.contains(&30) && seen.contains(&35));
    }
}
