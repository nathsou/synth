//! A negotiated-congestion router (PathFinder, McMurchie & Ebeling 1995)
//! with A* search, over a device routing graph in compressed sparse row form.
//!
//! Each routing node can carry one net. Nets are routed one after another;
//! overused nodes get more expensive each iteration (present and history
//! congestion costs) until every node is used by at most one net. Only nets
//! touching overused nodes are ripped up and rerouted after the first pass,
//! and nets whose nodes others take in that pass.
//!
//! The cost of entering a node blends congestion and delay according to the
//! criticality of the connection being routed, so timing-critical
//! connections prefer fast paths, and join their net's tree near its source.

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
    /// Upper bound on the criticality used in costs.
    pub max_criticality: f32,
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
    /// Extra tiles around a net's bounding box that searches may use (it
    /// grows for nets that stay on congested nodes).
    pub bbox_margin: i16,
    /// Iterations without a new best (least overuse) before giving up.
    pub stall_iterations: usize,
    /// Iterations between criticality updates (0: never).
    pub retime_every: usize,
    /// Nets with a sink at least this critical are routed again after an
    /// update.
    pub reroute_criticality: f32,
}

impl Default for RouteOptions {
    fn default() -> Self {
        RouteOptions {
            max_iterations: 200,
            max_criticality: 0.9,
            present_factor: 0.5,
            present_growth: 1.6,
            history_factor: 0.4,
            astar_cost_per_tile: 0.1,
            astar_delay_per_tile: 0.0,
            delay_scale: 0.001,
            bbox_margin: 3,
            stall_iterations: 120,
            retime_every: 4,
            reroute_criticality: 0.95,
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
    /// using them (those on the first node first).
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
    /// Delay from the source of the net being routed to its tree nodes.
    tree_delay: Vec<f32>,
    /// Reverse edge lookup: source node of each edge.
    edge_src: Vec<u32>,
    /// Nodes all of whose paths stay at their position (like the inputs of
    /// a logic block): useless for reaching a sink anywhere else.
    confined: Vec<bool>,
}

/// Routes `nets` on `graph`. Returns one route per net.
pub fn route(
    graph: Graph<'_>,
    nets: &[NetRequest],
    opts: &RouteOptions,
) -> Result<(Vec<NetRoute>, RouteStats), RouteError> {
    route_with(graph, nets, opts, RouteHooks::default())
}

/// Progress callback: iteration, nets routed in it, overused nodes.
pub type Progress<'a> = &'a mut dyn FnMut(usize, usize, &[usize]);
/// Criticality update from the current routes.
pub type Retime<'a> = &'a mut dyn FnMut(&[NetRoute]) -> Vec<Vec<f32>>;

/// What a caller can add to routing.
#[derive(Default)]
pub struct RouteHooks<'a> {
    /// Nodes no net may use.
    pub blocked: &'a [u32],
    /// Edges no net may use.
    pub blocked_edges: &'a [u32],
    /// Called after every iteration with the iteration number, the number of
    /// nets routed in it and the overused nodes.
    pub progress: Option<Progress<'a>>,
    /// Recomputes sink criticalities (per net, like
    /// [`NetRequest::criticality`]) from the current routes; called every
    /// [`RouteOptions::retime_every`] iterations. Nets with a critical sink
    /// are then routed again even if uncongested.
    pub retime: Option<Retime<'a>>,
}

impl std::fmt::Debug for RouteHooks<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteHooks")
            .field("blocked", &self.blocked.len())
            .field("blocked_edges", &self.blocked_edges.len())
            .field("progress", &self.progress.is_some())
            .field("retime", &self.retime.is_some())
            .finish()
    }
}

/// [`route`] with caller hooks.
pub fn route_with(
    graph: Graph<'_>,
    nets: &[NetRequest],
    opts: &RouteOptions,
    mut hooks: RouteHooks<'_>,
) -> Result<(Vec<NetRoute>, RouteStats), RouteError> {
    let mut crit: Vec<Vec<f32>> = nets.iter().map(|n| n.criticality.clone()).collect();
    let n = graph.num_nodes();
    let mut edge_src = vec![0u32; graph.edge_dst.len()];
    for node in 0..n {
        for e in graph.offsets[node]..graph.offsets[node + 1] {
            edge_src[e as usize] = node as u32;
        }
    }
    // A blocked edge becomes a loop on its source node, which never
    // improves a search.
    let looped: Vec<u32>;
    let graph = if hooks.blocked_edges.is_empty() {
        graph
    } else {
        let mut dst = graph.edge_dst.to_vec();
        for &e in hooks.blocked_edges {
            dst[e as usize] = edge_src[e as usize];
        }
        looped = dst;
        Graph { edge_dst: &looped, ..graph }
    };
    let confined = confined_nodes(&graph, &edge_src);
    let mut st = State {
        occupancy: vec![0; n],
        history: vec![0.0; n],
        best: vec![0.0; n],
        via: vec![u32::MAX; n],
        stamp: vec![0; n],
        search_gen: 0,
        in_tree: vec![0; n],
        tree_gen: 0,
        tree_delay: vec![0.0; n],
        edge_src,
        confined,
    };
    let mut routes: Vec<NetRoute> = vec![NetRoute::default(); nets.len()];
    let mut pres_fac = opts.present_factor;
    let mut stats = RouteStats::default();

    // Sources and sinks of other nets are reserved: never route through them.
    let mut reserved = vec![u32::MAX; n];
    for &b in hooks.blocked {
        reserved[b as usize] = u32::MAX - 1;
    }
    for (i, net) in nets.iter().enumerate() {
        reserved[net.source as usize] = i as u32;
        for &s in &net.sinks {
            reserved[s as usize] = i as u32;
        }
    }

    let mut best: Option<(Vec<u32>, Vec<usize>)> = None;
    let mut best_iter = 0;
    // Search margin of each net: it widens every time the net is ripped up
    // again for congestion, so that persistent conflicts can detour.
    let mut margin = vec![opts.bbox_margin; nets.len()];
    let mut timing_rounds = 0;
    let mut legal: Option<(Vec<NetRoute>, RouteStats)> = None;
    let mut to_route: Vec<usize> = (0..nets.len()).collect();
    // Route nets with many sinks first: they are hardest to fit later.
    to_route.sort_by_key(|&i| std::cmp::Reverse(nets[i].sinks.len()));
    // Exclusive or of the nets using each node: with one user, that net.
    let mut users = vec![0u32; n];
    // Last iteration each net was (or is to be) routed in.
    let mut routed = vec![usize::MAX; nets.len()];
    for iter in 0..opts.max_iterations {
        stats.iterations = iter + 1;
        for &i in &to_route {
            routed[i] = iter;
        }
        // A net taking the node of another is routed again in this
        // iteration, now that it has company: it moves if it can, before the
        // first net comes back to the same choice (two nets could otherwise
        // swap nodes forever while the one that should move never sees the
        // other on its node).
        let mut k = 0;
        while k < to_route.len() {
            let i = to_route[k];
            k += 1;
            // Rip up.
            for &e in &routes[i].edges {
                let d = graph.edge_dst[e as usize] as usize;
                st.occupancy[d] -= 1;
                users[d] ^= i as u32;
            }
            routes[i].edges.clear();
            route_net(
                &graph,
                &nets[i],
                &crit[i],
                i as u32,
                &reserved,
                &mut st,
                opts,
                margin[i],
                pres_fac,
                &mut routes[i],
            )?;
            for &e in &routes[i].edges {
                let d = graph.edge_dst[e as usize] as usize;
                if st.occupancy[d] == 1 {
                    let v = users[d] as usize;
                    if routed[v] != iter {
                        routed[v] = iter;
                        to_route.push(v);
                    }
                }
                st.occupancy[d] += 1;
                users[d] ^= i as u32;
            }
        }
        let overused: Vec<usize> = (0..n).filter(|&k| st.occupancy[k] > 1).collect();
        stats.overused_history.push(overused.len());
        if let Some(p) = hooks.progress.as_mut() {
            p(iter, to_route.len(), &overused);
        }
        if best.as_ref().is_none_or(|b: &(Vec<u32>, Vec<usize>)| overused.len() < b.0.len()) {
            let set: std::collections::HashSet<usize> = overused.iter().copied().collect();
            let uses = |i: usize, f: &dyn Fn(usize) -> bool| {
                routes[i].edges.iter().any(|&e| f(graph.edge_dst[e as usize] as usize))
            };
            // Nets on the first overused node first, then the others.
            let first = overused.first().copied();
            let mut users: Vec<usize> =
                (0..nets.len()).filter(|&i| uses(i, &|d| Some(d) == first)).collect();
            let rest: Vec<usize> = (0..nets.len())
                .filter(|&i| !users.contains(&i) && uses(i, &|d| set.contains(&d)))
                .collect();
            users.extend(rest);
            best = Some((overused.iter().map(|&k| k as u32).collect(), users));
            best_iter = iter;
        }
        if overused.is_empty() {
            stats.wire_nodes = routes.iter().map(|r| r.edges.len()).sum();
            // A legal routing: with routed criticalities, try once or twice
            // more for faster critical connections, keeping this one should
            // that not become legal again.
            let Some(retime) = hooks.retime.as_mut().filter(|_| timing_rounds < 2) else {
                return Ok((routes, stats));
            };
            timing_rounds += 1;
            legal = Some((routes.clone(), stats.clone()));
            crit = retime(&routes);
            to_route = (0..nets.len())
                .filter(|&i| crit[i].iter().any(|&c| c >= opts.reroute_criticality))
                .collect();
            if to_route.is_empty() {
                return Ok((routes, stats));
            }
            best_iter = iter;
            continue;
        }
        // No progress for a long while: give up (the caller may place again).
        if iter >= best_iter + opts.stall_iterations {
            break;
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
        if iter >= 2 {
            for &i in &to_route {
                margin[i] = margin[i].saturating_add(1);
            }
        }
        // Criticalities from the routes so far; critical nets try again.
        if opts.retime_every > 0
            && iter % opts.retime_every == 0
            && let Some(retime) = hooks.retime.as_mut()
        {
            crit = retime(&routes);
            let mut listed = vec![false; nets.len()];
            to_route.iter().for_each(|&i| listed[i] = true);
            to_route.extend(
                (0..nets.len()).filter(|&i| {
                    !listed[i] && crit[i].iter().any(|&c| c >= opts.reroute_criticality)
                }),
            );
        }
    }
    if let Some(l) = legal {
        return Ok(l);
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

/// Nodes from which every path stays at the node's position.
fn confined_nodes(graph: &Graph<'_>, edge_src: &[u32]) -> Vec<bool> {
    let n = graph.num_nodes();
    let pos = |k: usize| (graph.node_x[k], graph.node_y[k]);
    // Nodes reaching another position, found backwards from the edges
    // leaving their position.
    let mut confined = vec![true; n];
    let mut stack = Vec::new();
    for (e, &d) in graph.edge_dst.iter().enumerate() {
        let s = edge_src[e] as usize;
        if pos(s) != pos(d as usize) && confined[s] {
            confined[s] = false;
            stack.push(s);
        }
    }
    let mut preds = vec![0u32; n + 1];
    for &d in graph.edge_dst {
        preds[d as usize + 1] += 1;
    }
    for k in 0..n {
        preds[k + 1] += preds[k];
    }
    let mut fill = preds.clone();
    let mut pred_edges = vec![0u32; graph.edge_dst.len()];
    for (e, &d) in graph.edge_dst.iter().enumerate() {
        pred_edges[fill[d as usize] as usize] = e as u32;
        fill[d as usize] += 1;
    }
    while let Some(k) = stack.pop() {
        for &e in &pred_edges[preds[k] as usize..preds[k + 1] as usize] {
            let s = edge_src[e as usize] as usize;
            if confined[s] {
                confined[s] = false;
                stack.push(s);
            }
        }
    }
    confined
}

#[allow(clippy::too_many_arguments)]
fn route_net(
    graph: &Graph<'_>,
    net: &NetRequest,
    criticality: &[f32],
    net_id: u32,
    reserved: &[u32],
    st: &mut State,
    opts: &RouteOptions,
    margin: i16,
    pres_fac: f32,
    out: &mut NetRoute,
) -> Result<(), RouteError> {
    st.tree_gen += 1;
    let tg = st.tree_gen;
    st.in_tree[net.source as usize] = tg;
    st.tree_delay[net.source as usize] = 0.0;
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
    let mut margin = margin;

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
        // Never fully ignore congestion, or critical nets cannot be moved
        // off contested wires.
        let crit = criticality.get(k).copied().unwrap_or(0.0).clamp(0.0, opts.max_criticality);
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
            // A critical sink pays for the delay from the source to where
            // it joins the tree, so that it does not join at the end of a
            // long branch.
            for &t in &tree_nodes {
                let g = crit * st.tree_delay[t as usize] * opts.delay_scale;
                st.best[t as usize] = g;
                st.via[t as usize] = u32::MAX;
                st.stamp[t as usize] = search_gen;
                heap.push(QueueEntry { f: g + est(t as usize), g, node: t });
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
                    if d != sink
                        && (nx < bx0
                            || nx > bx1
                            || ny < by0
                            || ny > by1
                            || st.confined[du] && (nx, ny) != (tx, ty))
                    {
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
            st.tree_delay[d as usize] =
                st.tree_delay[st.edge_src[e as usize] as usize] + graph.edge_delay[e as usize];
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

    #[test]
    fn blocked_edges_are_avoided() {
        let (o, d, dl, x, y, c) = grid(3, 3);
        let g = Graph {
            offsets: &o,
            edge_dst: &d,
            edge_delay: &dl,
            node_x: &x,
            node_y: &y,
            node_cost: &c,
        };
        // Block the straight way from node 0 to node 2 along the first row.
        let blocked: Vec<u32> = (o[0]..o[1]).filter(|&e| d[e as usize] == 1).collect();
        let nets = vec![NetRequest { source: 0, sinks: vec![2], criticality: vec![] }];
        let hooks = RouteHooks { blocked_edges: &blocked, ..RouteHooks::default() };
        let (routes, _) = route_with(g, &nets, &RouteOptions::default(), hooks).unwrap();
        assert!(routes[0].edges.iter().all(|e| !blocked.contains(e)));
        assert_eq!(d[*routes[0].edges.last().unwrap() as usize], 2);
    }

    /// Builds a graph with all nodes at one position from an edge list.
    fn edge_graph(n: usize, edges: &[(u32, u32)]) -> GridData {
        let mut offsets = vec![0u32];
        let mut dst = Vec::new();
        for s in 0..n as u32 {
            dst.extend(edges.iter().filter(|e| e.0 == s).map(|e| e.1));
            offsets.push(dst.len() as u32);
        }
        let delay = vec![100.0; dst.len()];
        (offsets, dst, delay, vec![0; n], vec![0; n], vec![1.0; n])
    }

    #[test]
    fn displaced_net_moves() {
        // Nets A and B need one of P1 and P3 each; V sits on P1 but could
        // take R1 from D, which has a long free detour. When A comes to P1,
        // V must see it there to move; rerouted only after A has left again
        // it would stay forever.
        let (s_a, s_b, s_d, s_v, p1, p3, r1, p2, s1, s2, s3) = (0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10);
        let (t_a, t_b, t_d, t_v) = (11, 12, 13, 14);
        let edges = [
            (s_a, p1),
            (s_a, p3),
            (s_b, p1),
            (s_b, p3),
            (p1, t_a),
            (p1, t_b),
            (p3, t_a),
            (p3, t_b),
            (s_v, p1),
            (p1, t_v),
            (s_v, r1),
            (r1, p2),
            (p2, t_v),
            (s_d, r1),
            (r1, t_d),
            (s_d, s1),
            (s1, s2),
            (s2, s3),
            (s3, t_d),
        ];
        let (o, d, dl, x, y, c) = edge_graph(15, &edges);
        let g = Graph {
            offsets: &o,
            edge_dst: &d,
            edge_delay: &dl,
            node_x: &x,
            node_y: &y,
            node_cost: &c,
        };
        let net = |source, sink| NetRequest { source, sinks: vec![sink], criticality: vec![] };
        let nets = vec![net(s_a, t_a), net(s_b, t_b), net(s_d, t_d), net(s_v, t_v)];
        let opts = RouteOptions { max_iterations: 30, ..RouteOptions::default() };
        let (routes, _) = route(g, &nets, &opts).unwrap();
        let uses = |i: usize, node: u32| routes[i].edges.iter().any(|&e| d[e as usize] == node);
        assert!(uses(3, r1) && uses(2, s3));
    }

    #[test]
    fn confined_nodes_lead_nowhere_else() {
        // 0 at (0, 0) feeds wire 1 at (1, 0), which feeds the input 2 of a
        // block there (and its logical input 3) or goes on to 4 at (2, 0).
        let (o, d, dl, _, _, c) = edge_graph(5, &[(0, 1), (1, 2), (2, 3), (3, 2), (1, 4)]);
        let (x, y) = (vec![0, 1, 1, 1, 2], vec![0; 5]);
        let g = Graph {
            offsets: &o,
            edge_dst: &d,
            edge_delay: &dl,
            node_x: &x,
            node_y: &y,
            node_cost: &c,
        };
        let src: Vec<u32> =
            (0..5u32).flat_map(|k| (o[k as usize]..o[k as usize + 1]).map(move |_| k)).collect();
        assert_eq!(confined_nodes(&g, &src), [false, false, true, true, true]);
    }

    #[test]
    fn critical_sink_joins_near_source() {
        // Source 0, far sink 4 at the end of the slow chain 1, 2, 3; the
        // critical sink 7 is one slow edge from 3 or three fast ones from
        // the source (5, 6).
        let edges = [(0, 1), (0, 5), (1, 2), (2, 3), (3, 4), (3, 7), (5, 6), (6, 7)];
        let (o, d, _, _, _, c) = edge_graph(8, &edges);
        let dl: Vec<f32> =
            edges.iter().map(|e| if e.1 >= 5 && e.0 != 3 { 400.0 } else { 1000.0 }).collect();
        let (x, y) = (vec![0, 1, 2, 3, 4, 0, 1, 2], vec![0, 0, 0, 0, 0, 1, 1, 1]);
        let g = Graph {
            offsets: &o,
            edge_dst: &d,
            edge_delay: &dl,
            node_x: &x,
            node_y: &y,
            node_cost: &c,
        };
        let nets = vec![NetRequest { source: 0, sinks: vec![4, 7], criticality: vec![0.0, 1.0] }];
        let (routes, _) = route(g, &nets, &RouteOptions::default()).unwrap();
        assert!(routes[0].edges.iter().any(|&e| d[e as usize] == 6));
    }
}
