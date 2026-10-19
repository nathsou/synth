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
use std::sync::Mutex;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicUsize};

use synth_core::{FxHashMap, FxHashSet};

mod lookahead;
use lookahead::Lookahead;

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
    /// When positive, the A* estimate is instead a table of the least cost
    /// and delay to a sink by distance, searched on the graph up to this
    /// many tiles away (for graphs whose wires span many positions).
    pub lookahead_radius: i16,
    /// Weight of the lookahead estimate: above 1 it may overestimate,
    /// trading route quality for search effort (VPR's `astar_fac`).
    pub lookahead_weight: f32,
    /// Scale converting delay units into cost units.
    pub delay_scale: f32,
    /// Extra tiles around a net's bounding box that searches may use (it
    /// grows for nets that stay on congested nodes).
    pub bbox_margin: i16,
    /// Bound each connection's search by its source and sink alone, plus
    /// the margin, rather than by the whole net (as nextpnr's router2):
    /// sinks of nets spanning the device then search near their own path.
    pub arc_bounding_boxes: bool,
    /// Iterations without a new best (least overuse) before giving up.
    pub stall_iterations: usize,
    /// Iterations between criticality updates while congested (0: never).
    pub retime_every: usize,
    /// Nets with a sink at least this critical are routed again after an
    /// update.
    pub reroute_criticality: f32,
    /// Rounds of rerouting critical nets after a legal routing (each lets
    /// them take other nets' nodes, which then negotiate anew).
    pub timing_rounds: usize,
    /// Iterations a timing round may take to become legal again.
    pub timing_round_iterations: usize,
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
            lookahead_radius: 0,
            lookahead_weight: 1.2,
            delay_scale: 0.001,
            bbox_margin: 3,
            arc_bounding_boxes: false,
            stall_iterations: 120,
            retime_every: 0,
            reroute_criticality: 0.95,
            timing_rounds: 0,
            timing_round_iterations: 20,
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
        // Min-heap on f; among equals the deepest first (with uniform costs
        // many paths tie, and only one needs exploring), then by node index
        // for determinism.
        other
            .f
            .partial_cmp(&self.f)
            .unwrap_or(Ordering::Equal)
            .then(self.g.partial_cmp(&other.g).unwrap_or(Ordering::Equal))
            .then(other.node.cmp(&self.node))
    }
}

impl PartialOrd for QueueEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// What a search reads of a node it reaches, together so that reaching a
/// node touches one cache line.
#[derive(Debug)]
struct Hot {
    x: i16,
    y: i16,
    /// Nets using the node. Nets routed at once use nodes in disjoint
    /// regions, so they never update the same count.
    occupancy: AtomicU16,
    /// Lookahead class.
    class: u16,
    /// The net whose source or sink the node is ([`FREE`] if none, or
    /// [`BLOCKED`]): no other net may use it.
    reserved: u32,
    /// Base cost plus history cost.
    cost: f32,
    /// All paths from the node stay at its position (like the inputs of a
    /// logic block): useless for reaching a sink anywhere else.
    confined: bool,
}

impl Hot {
    fn occupancy(&self) -> u16 {
        self.occupancy.load(Relaxed)
    }
}

const FREE: u32 = u32::MAX;
const BLOCKED: u32 = u32::MAX - 1;

/// A node's marks in the current search and tree.
#[derive(Debug, Clone, Copy, Default)]
struct Visit {
    /// Best cost found in the current search, valid when `stamp == search_gen`.
    best: f32,
    stamp: u32,
    /// Edge used to reach the node in the current search (`u32::MAX` for roots).
    via: u32,
    /// The node belongs to the tree of the net being routed when equal to
    /// `tree_gen`.
    tree: u32,
}

/// Per-node congestion state, shared by the searches of an iteration.
struct State {
    hot: Vec<Hot>,
    /// Exclusive or of the nets using each node: with one user, that net.
    users: Vec<AtomicU32>,
    /// Reverse edge lookup: source node of each edge.
    edge_src: Vec<u32>,
    lookahead: Option<Lookahead>,
}

/// Search state of one thread.
struct Scratch {
    visit: Vec<Visit>,
    search_gen: u32,
    tree_gen: u32,
    /// Delay from the source of the net being routed to its tree nodes.
    tree_delay: Vec<f32>,
}

impl Scratch {
    fn new(n: usize) -> Scratch {
        Scratch {
            visit: vec![Visit::default(); n],
            search_gen: 0,
            tree_gen: 0,
            tree_delay: vec![0.0; n],
        }
    }
}

/// A region of positions, inclusive.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Region {
    x0: i16,
    y0: i16,
    x1: i16,
    y1: i16,
}

impl Region {
    fn contains(&self, x: i16, y: i16) -> bool {
        (self.x0..=self.x1).contains(&x) && (self.y0..=self.y1).contains(&y)
    }
    fn within(&self, outer: &Region) -> bool {
        outer.contains(self.x0, self.y0) && outer.contains(self.x1, self.y1)
    }
}

/// What routing one net in an iteration reads.
struct Pass<'a> {
    graph: &'a Graph<'a>,
    nets: &'a [NetRequest],
    crit: &'a [Vec<f32>],
    margin: &'a [i16],
    opts: &'a RouteOptions,
    pres_fac: f32,
    iter: usize,
    /// Last iteration each net was (or is to be) routed in.
    routed: &'a [AtomicUsize],
}

impl Pass<'_> {
    /// The region spanned by net `i` and the nodes of its current route.
    fn region(&self, st: &State, i: usize, route: &NetRoute) -> Region {
        let net = &self.nets[i];
        let at = |k: u32| (self.graph.node_x[k as usize], self.graph.node_y[k as usize]);
        let (x, y) = at(net.source);
        let mut r = Region { x0: x, y0: y, x1: x, y1: y };
        for &k in &net.sinks {
            let (x, y) = at(k);
            r = Region { x0: r.x0.min(x), y0: r.y0.min(y), x1: r.x1.max(x), y1: r.y1.max(y) };
        }
        for &e in &route.edges {
            let h = &st.hot[self.graph.edge_dst[e as usize] as usize];
            r = Region {
                x0: r.x0.min(h.x),
                y0: r.y0.min(h.y),
                x1: r.x1.max(h.x),
                y1: r.y1.max(h.y),
            };
        }
        r
    }

    /// Rips up net `i` and routes it again within `clip`, adding the nets
    /// whose nodes it takes to `displaced`. Returns false, leaving the net
    /// unrouted, if it needs more than `clip`.
    #[allow(clippy::too_many_arguments)]
    fn reroute(
        &self,
        st: &State,
        sc: &mut Scratch,
        i: usize,
        route: &mut NetRoute,
        seed: &[u32],
        clip: Option<Region>,
        displaced: &mut Vec<usize>,
    ) -> Result<bool, RouteError> {
        let graph = self.graph;
        for &e in &route.edges {
            let d = graph.edge_dst[e as usize] as usize;
            st.hot[d].occupancy.fetch_sub(1, Relaxed);
            st.users[d].fetch_xor(i as u32, Relaxed);
        }
        route.edges.clear();
        let (net, crit) = (&self.nets[i], &self.crit[i]);
        if !route_net(
            graph,
            st,
            sc,
            net,
            crit,
            i as u32,
            self.opts,
            self.margin[i],
            self.pres_fac,
            seed,
            clip,
            route,
        )? {
            route.edges.clear();
            return Ok(false);
        }
        for &e in &route.edges {
            let d = graph.edge_dst[e as usize] as usize;
            if st.hot[d].occupancy.fetch_add(1, Relaxed) == 1 {
                let v = st.users[d].load(Relaxed) as usize;
                if self.routed[v].swap(self.iter, Relaxed) != self.iter {
                    displaced.push(v);
                }
            }
            st.users[d].fetch_xor(i as u32, Relaxed);
        }
        Ok(true)
    }
}

/// Nets routed at once: below this many, one after another.
const PARALLEL_NETS: usize = 64;

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
pub type Retime<'a> = &'a mut dyn FnMut(&[NetRoute]) -> Retimed;

/// What [`RouteHooks::retime`] finds of a set of routes.
#[derive(Debug, Clone, Default)]
pub struct Retimed {
    /// Sink criticalities, per net like [`NetRequest::criticality`].
    pub criticality: Vec<Vec<f32>>,
    /// Timing score of the routes (lower is better): of the legal routings
    /// found, the one with the lowest score is returned.
    pub score: f64,
}

/// What a caller can add to routing.
#[derive(Default)]
pub struct RouteHooks<'a> {
    /// Nodes no net may use.
    pub blocked: &'a [u32],
    /// Edges no net may use.
    pub blocked_edges: &'a [u32],
    /// A class for each node (empty: all alike), distinguishing nodes whose
    /// remaining cost to a sink differs at the same distance, like wires of
    /// different lengths, for [`RouteOptions::lookahead_radius`].
    pub node_class: &'a [u16],
    /// Called after every iteration with the iteration number, the number of
    /// nets routed in it and the overused nodes.
    pub progress: Option<Progress<'a>>,
    /// Recomputes sink criticalities and a timing score from the current
    /// routes; called every [`RouteOptions::retime_every`] iterations and on
    /// every legal routing. Nets with a critical sink are then routed again
    /// even if uncongested, for up to [`RouteOptions::timing_rounds`]
    /// rounds after the first legal routing; the best scoring legal routing
    /// is returned.
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
    hooks: RouteHooks<'_>,
) -> Result<(Vec<NetRoute>, RouteStats), RouteError> {
    route_with_initial(graph, nets, opts, hooks, &[])
}

/// Starts from candidate trees, indexed like `nets`. Invalid, incomplete,
/// blocked or overlapping trees are discarded. Accepted trees occupy wires
/// immediately, but may be ripped up by congestion or timing optimization.
pub fn route_with_initial(
    graph: Graph<'_>,
    nets: &[NetRequest],
    opts: &RouteOptions,
    mut hooks: RouteHooks<'_>,
    initial: &[NetRoute],
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
    let (preds, pred_edges) = predecessors(&graph);
    let confined = confined_nodes(&graph, &edge_src, &preds, &pred_edges);
    let lookahead = (opts.lookahead_radius > 0).then(|| {
        let searched = lookahead::Searched {
            graph: &graph,
            preds: &preds,
            pred_edges: &pred_edges,
            edge_src: &edge_src,
        };
        Lookahead::new(&searched, hooks.node_class, nets, opts.lookahead_radius)
    });
    let hot = (0..n)
        .map(|k| Hot {
            x: graph.node_x[k],
            y: graph.node_y[k],
            occupancy: AtomicU16::new(0),
            class: hooks.node_class.get(k).copied().unwrap_or(0),
            reserved: FREE,
            cost: graph.node_cost[k],
            confined: confined[k],
        })
        .collect();
    let mut st =
        State { hot, users: (0..n).map(|_| AtomicU32::new(0)).collect(), edge_src, lookahead };
    let threads = std::thread::available_parallelism().map_or(1, |t| t.get()).min(4);
    let mut scratch: Vec<Scratch> = (0..threads).map(|_| Scratch::new(n)).collect();
    let mut routes: Vec<NetRoute> = vec![NetRoute::default(); nets.len()];
    let mut pres_fac = opts.present_factor;
    let mut stats = RouteStats::default();

    // Sources and sinks of other nets are reserved: never route through them.
    for &b in hooks.blocked {
        st.hot[b as usize].reserved = BLOCKED;
    }
    for (i, net) in nets.iter().enumerate() {
        st.hot[net.source as usize].reserved = i as u32;
        for &s in &net.sinks {
            st.hot[s as usize].reserved = i as u32;
        }
    }

    let mut best: Option<(Vec<u32>, Vec<usize>)> = None;
    let mut best_iter = 0;
    // Search margin of each net: it widens every time the net is ripped up
    // again for congestion, so that persistent conflicts can detour.
    let mut margin = vec![opts.bbox_margin; nets.len()];
    let mut timing_rounds = 0;
    let mut legal: Option<(Vec<NetRoute>, RouteStats, f64)> = None;
    let mut seen = if initial.is_empty() { Vec::new() } else { vec![usize::MAX; n] };
    let mut to_route = Vec::new();
    // Partial trees of nets that gained sinks: their first route grows them.
    let mut seeds: Vec<Vec<u32>> = vec![Vec::new(); nets.len()];
    for (i, net) in nets.iter().enumerate() {
        let kept = initial.get(i).and_then(|r| prune_tree(graph, &st, net, i, &r.edges, &mut seen));
        let Some((edges, complete)) = kept else {
            to_route.push(i);
            continue;
        };
        for &e in &edges {
            let d = graph.edge_dst[e as usize] as usize;
            *st.hot[d].occupancy.get_mut() += 1;
            *st.users[d].get_mut() ^= i as u32;
        }
        if complete {
            stats.reused_nets += 1;
            routes[i] = NetRoute { edges };
        } else {
            stats.partly_reused_nets += 1;
            routes[i] = NetRoute { edges: edges.clone() };
            seeds[i] = edges;
            to_route.push(i);
        }
    }
    // Route nets with many sinks first: they are hardest to fit later.
    to_route.sort_by_key(|&i| std::cmp::Reverse(nets[i].sinks.len()));
    // Last iteration each net was (or is to be) routed in.
    let routed: Vec<AtomicUsize> = (0..nets.len()).map(|_| AtomicUsize::new(usize::MAX)).collect();
    for iter in 0..opts.max_iterations {
        stats.iterations = iter + 1;
        for &i in &to_route {
            routed[i].store(iter, Relaxed);
        }
        let pass = Pass {
            graph: &graph,
            nets,
            crit: &crit,
            margin: &margin,
            opts,
            pres_fac,
            iter,
            routed: &routed,
        };
        // Nets within one quadrant of the region of those to route are
        // routed at once with the others of their quadrant, as they use
        // disjoint nodes; then the rest one after another.
        let mut queue = if to_route.len() >= PARALLEL_NETS && threads > 1 {
            parallel(&pass, &st, &mut scratch, &mut routes, &mut seeds, &to_route)?
        } else {
            std::mem::take(&mut to_route)
        };
        // A net taking the node of another is routed again in this
        // iteration, now that it has company: it moves if it can, before the
        // first net comes back to the same choice (two nets could otherwise
        // swap nodes forever while the one that should move never sees the
        // other on its node).
        let mut k = 0;
        while k < queue.len() {
            let i = queue[k];
            k += 1;
            let seed = std::mem::take(&mut seeds[i]);
            pass.reroute(&st, &mut scratch[0], i, &mut routes[i], &seed, None, &mut queue)?;
        }
        let overused: Vec<usize> = (0..n).filter(|&k| st.hot[k].occupancy() > 1).collect();
        stats.overused_history.push(overused.len());
        if let Some(p) = hooks.progress.as_mut() {
            p(iter, routed.iter().filter(|r| r.load(Relaxed) == iter).count(), &overused);
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
            // A legal routing: with routed criticalities, try a few rounds
            // more for faster critical connections, keeping the best timed
            // legal routing.
            let Some(retime) = hooks.retime.as_mut() else {
                return Ok((routes, stats));
            };
            let r = retime(&routes);
            if legal.as_ref().is_none_or(|l| r.score < l.2) {
                legal = Some((routes.clone(), stats.clone(), r.score));
            }
            crit = r.criticality;
            to_route = (0..nets.len())
                .filter(|&i| crit[i].iter().any(|&c| c >= opts.reroute_criticality))
                .collect();
            if timing_rounds >= opts.timing_rounds || to_route.is_empty() {
                break;
            }
            timing_rounds += 1;
            best_iter = iter;
            // Critical nets may take others' nodes again, which then
            // negotiate anew.
            pres_fac = opts.present_factor;
            continue;
        }
        // No progress for a long while: give up (the caller may place again).
        if iter >= best_iter + opts.stall_iterations {
            break;
        }
        // A timing round that does not become legal soon is abandoned.
        if timing_rounds > 0 && iter >= best_iter + opts.timing_round_iterations {
            break;
        }
        for &k in &overused {
            let h = &mut st.hot[k];
            h.cost += opts.history_factor * (h.occupancy() - 1) as f32;
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
        // Each keeps the branches of its tree clear of overused nodes, and
        // routes again only the sinks they no longer reach.
        for &i in &to_route {
            seeds[i] = clear_branches(&graph, &st, &nets[i], &routes[i].edges, &is_over);
        }
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
            crit = retime(&routes).criticality;
            let mut listed = vec![false; nets.len()];
            to_route.iter().for_each(|&i| listed[i] = true);
            to_route.extend(
                (0..nets.len()).filter(|&i| {
                    !listed[i] && crit[i].iter().any(|&c| c >= opts.reroute_criticality)
                }),
            );
        }
    }
    if let Some((routes, stats, _)) = legal {
        return Ok((routes, stats));
    }
    let (nodes, nets) = best.unwrap_or_default();
    let overused_nodes = nodes.len();
    Err(RouteError::Congested { overused_nodes, nodes, nets })
}

/// Routes the nets of `to_route` concurrently where they lie in disjoint
/// regions: first those within one quadrant of the region all span, then
/// of those left, those within its left or right half and those within its
/// lower or upper half (as nextpnr's router2 partitions). A thread takes a
/// region at a time. Returns the nets left to route one after another:
/// those spanning both halves in order, then (in net order) those
/// displaced from a region or needing more than theirs.
fn parallel(
    pass: &Pass<'_>,
    st: &State,
    scratch: &mut [Scratch],
    routes: &mut Vec<NetRoute>,
    seeds: &mut [Vec<u32>],
    to_route: &[usize],
) -> Result<Vec<usize>, RouteError> {
    let regions: Vec<Region> = to_route.iter().map(|&i| pass.region(st, i, &routes[i])).collect();
    let median = |mut v: Vec<i16>| {
        v.sort_unstable();
        v[v.len() / 2]
    };
    let xs = median(regions.iter().map(|r| r.x0 + (r.x1 - r.x0) / 2).collect());
    let ys = median(regions.iter().map(|r| r.y0 + (r.y1 - r.y0) / 2).collect());
    let (lo, hi) = (i16::MIN, i16::MAX);
    let region = |x0, y0, x1, y1| Region { x0, y0, x1, y1 };
    let levels = [
        vec![
            region(lo, lo, xs - 1, ys - 1),
            region(xs, lo, hi, ys - 1),
            region(lo, ys, xs - 1, hi),
            region(xs, ys, hi, hi),
        ],
        vec![region(lo, lo, xs - 1, hi), region(xs, lo, hi, hi)],
        vec![region(lo, lo, hi, ys - 1), region(lo, ys, hi, hi)],
    ];
    let cells: Vec<Mutex<(NetRoute, Vec<u32>)>> =
        routes.drain(..).zip(seeds.iter_mut().map(std::mem::take)).map(Mutex::new).collect();
    let threads = scratch.len();
    let mut rest: Vec<(usize, Region)> = to_route.iter().copied().zip(regions).collect();
    let mut later = Vec::new();
    let mut failed = None;
    for parts in &levels {
        let mut buckets = vec![Vec::new(); parts.len()];
        rest.retain(|&(i, r)| match parts.iter().position(|q| r.within(q)) {
            Some(q) => {
                buckets[q].push(i);
                false
            }
            None => true,
        });
        let results: Vec<Result<Vec<usize>, RouteError>> = std::thread::scope(|s| {
            let workers: Vec<_> = scratch
                .iter_mut()
                .enumerate()
                .map(|(t, sc)| {
                    let (buckets, cells) = (&buckets, &cells);
                    s.spawn(move || {
                        let mut later = Vec::new();
                        for q in (t..parts.len()).step_by(threads) {
                            route_region(pass, st, sc, cells, &buckets[q], parts[q], &mut later)?;
                        }
                        Ok(later)
                    })
                })
                .collect();
            workers.into_iter().map(|w| w.join().expect("router thread panicked")).collect()
        });
        for r in results {
            match r {
                Ok(l) => later.extend(l),
                Err(e) => failed = failed.or(Some(e)),
            }
        }
        if failed.is_some() {
            break;
        }
    }
    for (i, cell) in cells.into_iter().enumerate() {
        let (route, seed) = cell.into_inner().expect("route lock");
        routes.push(route);
        seeds[i] = seed;
    }
    if let Some(e) = failed {
        return Err(e);
    }
    later.sort_unstable();
    let mut queue: Vec<usize> = rest.into_iter().map(|(i, _)| i).collect();
    queue.extend(later);
    Ok(queue)
}

/// Routes `nets` within `clip`, and the nets they displace that lie within
/// it; adds the others, and nets needing more than `clip`, to `later`.
fn route_region(
    pass: &Pass<'_>,
    st: &State,
    sc: &mut Scratch,
    cells: &[Mutex<(NetRoute, Vec<u32>)>],
    nets: &[usize],
    clip: Region,
    later: &mut Vec<usize>,
) -> Result<(), RouteError> {
    let mut queue = nets.to_vec();
    let mut k = 0;
    while k < queue.len() {
        let i = queue[k];
        k += 1;
        let mut displaced = Vec::new();
        {
            let mut cell = cells[i].lock().expect("route lock");
            let (route, seed) = &mut *cell;
            let kept = std::mem::take(seed);
            if !pass.reroute(st, sc, i, route, &kept, Some(clip), &mut displaced)? {
                *seed = kept;
                later.push(i);
            }
        }
        for v in displaced {
            let route = &cells[v].lock().expect("route lock").0;
            if pass.region(st, v, route).within(&clip) {
                queue.push(v);
            } else {
                later.push(v);
            }
        }
    }
    Ok(())
}

/// The edges of tree `edges` (in tree order) on paths from the source to
/// sinks of `net` that avoid the nodes in `avoid`, in tree order.
fn clear_branches(
    graph: &Graph<'_>,
    st: &State,
    net: &NetRequest,
    edges: &[u32],
    avoid: &[bool],
) -> Vec<u32> {
    let mut reached: FxHashSet<u32> = FxHashSet::default();
    reached.insert(net.source);
    let mut kept: Vec<u32> = Vec::new();
    for &e in edges {
        let (s, d) = (st.edge_src[e as usize], graph.edge_dst[e as usize]);
        if reached.contains(&s) && !avoid[d as usize] {
            reached.insert(d);
            kept.push(e);
        }
    }
    // Drop branches that lead to no sink, from the leaves up.
    let mut useful: FxHashSet<u32> =
        net.sinks.iter().copied().filter(|k| reached.contains(k)).collect();
    let mut keep = vec![false; kept.len()];
    for (j, &e) in kept.iter().enumerate().rev() {
        if useful.contains(&graph.edge_dst[e as usize]) {
            keep[j] = true;
            useful.insert(st.edge_src[e as usize]);
        }
    }
    kept.into_iter().zip(keep).filter_map(|(e, k)| k.then_some(e)).collect()
}

/// The part of candidate tree `edges` for net `i` that leads to its current
/// sinks, and whether it reaches them all; `None` if the tree is invalid,
/// blocked, overlaps wires already taken, or keeps nothing.
fn prune_tree(
    graph: Graph<'_>,
    st: &State,
    net: &NetRequest,
    i: usize,
    edges: &[u32],
    seen: &mut [usize],
) -> Option<(Vec<u32>, bool)> {
    seen[net.source as usize] = i;
    // The edge into each node of the tree.
    let mut parent: FxHashMap<u32, u32> = FxHashMap::default();
    for &e in edges {
        let e = e as usize;
        if e >= graph.edge_dst.len() {
            return None;
        }
        let (s, d) = (st.edge_src[e] as usize, graph.edge_dst[e] as usize);
        if seen[s] != i
            || seen[d] == i
            || st.hot[d].occupancy() != 0
            || ![FREE, i as u32].contains(&st.hot[d].reserved)
        {
            return None;
        }
        seen[d] = i;
        parent.insert(d as u32, e as u32);
    }
    // Keep the paths from the source to the sinks the tree reaches.
    let mut keep: FxHashSet<u32> = FxHashSet::default();
    let mut complete = true;
    for &s in &net.sinks {
        if seen[s as usize] != i {
            complete = false;
            continue;
        }
        let mut cur = s;
        while let Some(&e) = parent.get(&cur) {
            if !keep.insert(e) {
                break;
            }
            cur = st.edge_src[e as usize];
        }
    }
    let kept: Vec<u32> = edges.iter().copied().filter(|e| keep.contains(e)).collect();
    (!kept.is_empty()).then_some((kept, complete))
}

/// Statistics of a routing run.
#[derive(Debug, Clone, Default)]
pub struct RouteStats {
    /// Candidate trees accepted before routing (they may later be rerouted).
    pub reused_nets: usize,
    /// Candidate trees kept in part (pruned to current sinks, then grown).
    pub partly_reused_nets: usize,
    pub iterations: usize,
    pub overused_history: Vec<usize>,
    pub wire_nodes: usize,
}

/// Nodes from which every path stays at the node's position.
/// The edges entering each node `k`: `pred_edges[preds[k]..preds[k + 1]]`.
fn predecessors(graph: &Graph<'_>) -> (Vec<u32>, Vec<u32>) {
    let n = graph.num_nodes();
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
    (preds, pred_edges)
}

fn confined_nodes(
    graph: &Graph<'_>,
    edge_src: &[u32],
    preds: &[u32],
    pred_edges: &[u32],
) -> Vec<bool> {
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
    st: &State,
    sc: &mut Scratch,
    net: &NetRequest,
    criticality: &[f32],
    net_id: u32,
    opts: &RouteOptions,
    margin: i16,
    pres_fac: f32,
    seed: &[u32],
    clip: Option<Region>,
    out: &mut NetRoute,
) -> Result<bool, RouteError> {
    sc.tree_gen += 1;
    let tg = sc.tree_gen;
    sc.visit[net.source as usize].tree = tg;
    sc.tree_delay[net.source as usize] = 0.0;
    let mut tree_nodes: Vec<u32> = vec![net.source];
    // Start from a kept partial tree (in tree order).
    for &e in seed {
        let d = graph.edge_dst[e as usize];
        sc.tree_delay[d as usize] =
            sc.tree_delay[st.edge_src[e as usize] as usize] + graph.edge_delay[e as usize];
        sc.visit[d as usize].tree = tg;
        tree_nodes.push(d);
        out.edges.push(e);
    }
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
    let mut heap = BinaryHeap::new();
    for k in order {
        let sink = net.sinks[k];
        if sc.visit[sink as usize].tree == tg {
            continue;
        }
        // Never fully ignore congestion, or critical nets cannot be moved
        // off contested wires.
        let crit = criticality.get(k).copied().unwrap_or(0.0).clamp(0.0, opts.max_criticality);
        let tx = graph.node_x[sink as usize];
        let ty = graph.node_y[sink as usize];
        loop {
            sc.search_gen += 1;
            let search_gen = sc.search_gen;
            heap.clear();
            let lookahead = st.lookahead.as_ref();
            let est = |h: &Hot| {
                let (dx, dy) = (h.x - tx, h.y - ty);
                if let Some(table) = lookahead {
                    let (cost, delay) = table.at(h.class as usize, dx, dy);
                    return opts.lookahead_weight
                        * ((1.0 - crit) * cost + crit * delay * opts.delay_scale);
                }
                let d = (dx.abs() + dy.abs()) as f32;
                d * ((1.0 - crit) * opts.astar_cost_per_tile
                    + crit * opts.astar_delay_per_tile * opts.delay_scale)
            };
            // The search box: the net's, or the connection's alone.
            let (ax0, ay0, ax1, ay1) = if opts.arc_bounding_boxes {
                (sx.min(tx), sy.min(ty), sx.max(tx), sy.max(ty))
            } else {
                (x0, y0, x1, y1)
            };
            let (mut bx0, mut by0, mut bx1, mut by1) =
                (ax0 - margin, ay0 - margin, ax1 + margin, ay1 + margin);
            if let Some(c) = clip {
                (bx0, by0, bx1, by1) = (bx0.max(c.x0), by0.max(c.y0), bx1.min(c.x1), by1.min(c.y1));
            }
            let inside = |h: &Hot| h.x >= bx0 && h.x <= bx1 && h.y >= by0 && h.y <= by1;
            // A critical sink pays for the delay from the source to where
            // it joins the tree, so that it does not join at the end of a
            // long branch.
            for &t in &tree_nodes {
                if t != net.source && !inside(&st.hot[t as usize]) {
                    continue;
                }
                let g = crit * sc.tree_delay[t as usize] * opts.delay_scale;
                let v = &mut sc.visit[t as usize];
                (v.best, v.via, v.stamp) = (g, u32::MAX, search_gen);
                heap.push(QueueEntry { f: g + est(&st.hot[t as usize]), g, node: t });
            }
            let mut found = false;
            while let Some(QueueEntry { g, node, .. }) = heap.pop() {
                if node == sink {
                    found = true;
                    break;
                }
                let v = sc.visit[node as usize];
                if v.stamp == search_gen && g > v.best {
                    continue;
                }
                for e in graph.offsets[node as usize]..graph.offsets[node as usize + 1] {
                    let d = graph.edge_dst[e as usize];
                    let du = d as usize;
                    let h = &st.hot[du];
                    if h.reserved != FREE && h.reserved != net_id {
                        continue;
                    }
                    if d != sink
                        && (h.x < bx0
                            || h.x > bx1
                            || h.y < by0
                            || h.y > by1
                            || h.confined && (h.x, h.y) != (tx, ty))
                    {
                        continue;
                    }
                    let v = &mut sc.visit[du];
                    if v.tree == tg {
                        continue;
                    }
                    let present = 1.0 + pres_fac * h.occupancy() as f32;
                    let delay = graph.edge_delay[e as usize] * opts.delay_scale;
                    let ng = g + (1.0 - crit) * h.cost * present + crit * delay;
                    if v.stamp != search_gen || ng < v.best {
                        (v.stamp, v.best, v.via) = (search_gen, ng, e);
                        heap.push(QueueEntry { f: ng + est(h), g: ng, node: d });
                    }
                }
            }
            if found {
                break;
            }
            // Retry with a larger box before giving up.
            if clip.is_some() {
                return Ok(false);
            }
            if margin > 1000 {
                return Err(RouteError::Unroutable { net: net_id as usize, sink });
            }
            margin = if margin < 8 { 8 } else { margin * 4 };
        }
        // Trace back to the tree.
        let mut path = Vec::new();
        let mut cur = sink;
        while sc.visit[cur as usize].tree != tg {
            let e = sc.visit[cur as usize].via;
            path.push(e);
            cur = st.edge_src[e as usize];
        }
        for &e in path.iter().rev() {
            let d = graph.edge_dst[e as usize];
            sc.tree_delay[d as usize] =
                sc.tree_delay[st.edge_src[e as usize] as usize] + graph.edge_delay[e as usize];
            sc.visit[d as usize].tree = tg;
            tree_nodes.push(d);
            out.edges.push(e);
        }
    }
    Ok(true)
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

    #[test]
    fn incremental_trees_are_checked_and_repaired() {
        let (o, d, dl, x, y, c) = grid(4, 4);
        let g = Graph {
            offsets: &o,
            edge_dst: &d,
            edge_delay: &dl,
            node_x: &x,
            node_y: &y,
            node_cost: &c,
        };
        let nets = vec![NetRequest { source: 0, sinks: vec![3, 15], criticality: vec![] }];
        let opts = RouteOptions::default();
        let (old, _) = route(g, &nets, &opts).unwrap();
        let mut routed = Vec::new();
        let mut progress = |_: usize, count, _: &[usize]| routed.push(count);
        let hooks = RouteHooks { progress: Some(&mut progress), ..Default::default() };
        let (same, stats) = route_with_initial(g, &nets, &opts, hooks, &old).unwrap();
        assert_eq!(stats.reused_nets, 1);
        assert_eq!(same[0].edges, old[0].edges);
        assert_eq!(routed, vec![0]);

        let bad = [
            vec![u32::MAX],
            old[0].edges[..1].to_vec(),
            old[0].edges.iter().copied().chain([old[0].edges[0]]).collect(),
        ];
        for edges in bad {
            let (repaired, stats) =
                route_with_initial(g, &nets, &opts, RouteHooks::default(), &[NetRoute { edges }])
                    .unwrap();
            assert_eq!(stats.reused_nets, 0);
            assert_eq!(repaired[0].edges, old[0].edges);
        }
        let blocked = [old[0].edges[0]];
        let hooks = RouteHooks { blocked_edges: &blocked, ..Default::default() };
        let (repaired, stats) = route_with_initial(g, &nets, &opts, hooks, &old).unwrap();
        assert_eq!(stats.reused_nets, 0);
        assert!(!repaired[0].edges.contains(&blocked[0]));
    }

    #[test]
    fn partial_trees_keep_branches_to_remaining_sinks() {
        let (o, d, dl, x, y, c) = grid(5, 5);
        let g = Graph {
            offsets: &o,
            edge_dst: &d,
            edge_delay: &dl,
            node_x: &x,
            node_y: &y,
            node_cost: &c,
        };
        let opts = RouteOptions::default();
        let (old, _) =
            route(g, &[NetRequest { source: 0, sinks: vec![4, 20], criticality: vec![] }], &opts)
                .unwrap();
        // Sink 20 is gone, sink 24 is new: the branch to 4 stays as it was.
        let to_4: Vec<u32> = {
            let mut path = Vec::new();
            let mut cur = 4;
            while cur != 0 {
                let e = *old[0].edges.iter().find(|&&e| d[e as usize] == cur).unwrap();
                path.push(e);
                cur = (0..o.len() - 1).find(|&s| (o[s]..o[s + 1]).contains(&e)).unwrap() as u32;
            }
            path
        };
        let nets = vec![NetRequest { source: 0, sinks: vec![4, 24], criticality: vec![] }];
        let (new, stats) =
            route_with_initial(g, &nets, &opts, RouteHooks::default(), &old).unwrap();
        assert_eq!((stats.reused_nets, stats.partly_reused_nets), (0, 1));
        assert!(to_4.iter().all(|e| new[0].edges.contains(e)));
        assert!(!new[0].edges.iter().any(|&e| d[e as usize] == 20));
        assert!(new[0].edges.iter().any(|&e| d[e as usize] == 24));
        // With only sink 4 left, the pruned tree is complete.
        let nets = vec![NetRequest { source: 0, sinks: vec![4], criticality: vec![] }];
        let (new, stats) =
            route_with_initial(g, &nets, &opts, RouteHooks::default(), &old).unwrap();
        assert_eq!((stats.reused_nets, stats.partly_reused_nets), (1, 0));
        assert_eq!(new[0].edges.len(), to_4.len());
    }

    #[test]
    fn incremental_routes_respect_new_endpoint_reservations() {
        let (o, d, dl, x, y, c) = grid(5, 5);
        let g = Graph {
            offsets: &o,
            edge_dst: &d,
            edge_delay: &dl,
            node_x: &x,
            node_y: &y,
            node_cost: &c,
        };
        let mut nets = vec![NetRequest { source: 0, sinks: vec![4], criticality: vec![] }];
        let opts = RouteOptions::default();
        let (old, _) = route(g, &nets, &opts).unwrap();
        let source = d[old[0].edges[0] as usize];
        nets.push(NetRequest { source, sinks: vec![6], criticality: vec![] });
        let (repaired, stats) =
            route_with_initial(g, &nets, &opts, RouteHooks::default(), &old).unwrap();
        assert_eq!(stats.reused_nets, 0);
        assert!(!repaired[0].edges.iter().any(|&e| d[e as usize] == source));
        let mut occupied = std::collections::HashSet::new();
        for r in repaired {
            for e in r.edges {
                assert!(occupied.insert(d[e as usize]));
            }
        }
    }

    #[test]
    fn best_timed_legal_routing_is_kept() {
        let (o, d, dl, x, y, c) = grid(4, 4);
        let g = Graph {
            offsets: &o,
            edge_dst: &d,
            edge_delay: &dl,
            node_x: &x,
            node_y: &y,
            node_cost: &c,
        };
        let nets = vec![NetRequest { source: 0, sinks: vec![15], criticality: vec![0.0] }];
        // The first legal routing scores best: the timing round's routing,
        // scored worse, is not returned.
        let mut seen: Vec<Vec<u32>> = Vec::new();
        let mut retime = |routes: &[NetRoute]| {
            seen.push(routes[0].edges.clone());
            Retimed { criticality: vec![vec![1.0]], score: seen.len() as f64 }
        };
        let hooks = RouteHooks { retime: Some(&mut retime), ..RouteHooks::default() };
        let opts = RouteOptions { timing_rounds: 1, ..RouteOptions::default() };
        let (routes, _) = route_with(g, &nets, &opts, hooks).unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(routes[0].edges, seen[0]);
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
        // D and V were already routed before A and B were added. Import
        // their occupancy, then let the new nets displace both old trees.
        let (old, _) = route(g, &nets[2..], &opts).unwrap();
        let mut initial = vec![NetRoute::default(); 2];
        initial.extend(old);
        let (repaired, stats) =
            route_with_initial(g, &nets, &opts, RouteHooks::default(), &initial).unwrap();
        assert_eq!(stats.reused_nets, 2);
        assert!(repaired[3].edges.iter().any(|&e| d[e as usize] == r1));
        assert!(repaired[2].edges.iter().any(|&e| d[e as usize] == s3));
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
        let (preds, pred_edges) = predecessors(&g);
        assert_eq!(confined_nodes(&g, &src, &preds, &pred_edges), [false, false, true, true, true]);
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
