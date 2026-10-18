//! Device CSR construction and configuration-bearing site pass-throughs.
use crate::device::Device;

/// Only explicitly modelled site pass-throughs are allowed. X-Ray `hint` pips
/// are not wires: many cross LUTs or muxes that require configuration.
pub(crate) fn pseudo_config(kind: &str, src: &str, dst: &str) -> Option<Vec<String>> {
    for side in ["L", "R"] {
        if kind.starts_with(&format!("{side}IOI3")) {
            for i in 0..2 {
                if src == format!("IOI_OLOGIC{i}_D1") && dst == format!("{side}IOI_OLOGIC{i}_OQ") {
                    return Some(vec![
                        format!("OLOGIC_Y{i}.OMUX.D1"),
                        format!("OLOGIC_Y{i}.OQUSED"),
                        format!("OLOGIC_Y{i}.OSERDES.DATA_RATE_TQ.BUF"),
                    ]);
                }
                if src == format!("{side}IOI_ILOGIC{i}_D") && dst == format!("IOI_ILOGIC{i}_O") {
                    return Some(vec![
                        format!("IDELAY_Y{i}.IDELAY_TYPE_FIXED"),
                        format!("ILOGIC_Y{i}.IDELMUXE3.P1"),
                        format!("ILOGIC_Y{i}.ISERDES.MODE.MASTER"),
                        format!("ILOGIC_Y{i}.ISERDES.NUM_CE.N1"),
                        format!("ILOGIC_Y{i}.ZINV_D"),
                    ]);
                }
                if src == format!("IOI_OLOGIC{i}_T1") && dst == format!("{side}IOI_OLOGIC{i}_TQ") {
                    return Some(vec![format!("OLOGIC_Y{i}.ZINV_T1")]);
                }
            }
        }
        if kind.starts_with(&format!("{side}IOB33"))
            && matches!((src, dst), ("IOB_O0", "IOB_O_OUT0") | ("IOB_T0", "IOB_T_OUT0"))
        {
            return Some(Vec::new());
        }
    }
    if kind.starts_with("CLK_HROW_") {
        for (x, s) in ["L", "R"].iter().enumerate() {
            for i in 0..12 {
                if src == format!("CLK_HROW_CK_MUX_OUT_{s}{i}")
                    && dst == format!("CLK_HROW_CK_HCLK_OUT_{s}{i}")
                {
                    return Some(vec![
                        format!("BUFHCE.BUFHCE_X{x}Y{i}.IN_USE"),
                        format!("BUFHCE.BUFHCE_X{x}Y{i}.ZINV_CE"),
                    ]);
                }
            }
        }
    }
    None
}

#[derive(Debug)]
pub struct RoutingGraph {
    pub offsets: Vec<u32>,
    pub dst: Vec<u32>,
    pub delay: Vec<f32>,
    /// Tile and template pip index for each graph edge.
    pub pip: Vec<(u32, u32)>,
    pub x: Vec<i16>,
    pub y: Vec<i16>,
    pub cost: Vec<f32>,
    pub missing_timing: usize,
}
impl RoutingGraph {
    pub fn new(db: &Device) -> Result<Self, String> {
        let n = db.node_x.len();
        let mut offsets = vec![0u32; n + 1];
        for t in &db.tiles {
            for p in &db.templates[t.template].pips {
                let src = t.nodes[p.src as usize];
                let dst = t.nodes[p.dst as usize];
                if src != dst {
                    offsets[src as usize + 1] += 1;
                }
            }
        }
        for i in 0..n {
            offsets[i + 1] =
                offsets[i + 1].checked_add(offsets[i]).ok_or("too many routing edges")?;
        }
        let m = offsets[n] as usize;
        let mut graph = Self {
            offsets: offsets.clone(),
            dst: vec![0; m],
            delay: vec![0.0; m],
            pip: vec![(0, 0); m],
            x: db.node_x.clone(),
            y: db.node_y.clone(),
            cost: vec![1.0; n],
            missing_timing: 0,
        };
        for (i, t) in db.tiles.iter().enumerate() {
            for (j, p) in db.templates[t.template].pips.iter().enumerate() {
                let src = t.nodes[p.src as usize] as usize;
                let dst = t.nodes[p.dst as usize];
                if src == dst as usize {
                    continue;
                }
                let e = offsets[src] as usize;
                offsets[src] += 1;
                graph.dst[e] = dst;
                graph.pip[e] = (i as u32, j as u32);
                // Unknown timing gets a routing heuristic, never a reported timing arc.
                graph.delay[e] = p.delay.unwrap_or(if p.fixed { 0.0 } else { 100.0 });
                if p.delay.is_none() && !p.fixed {
                    graph.missing_timing += 1;
                }
            }
        }
        Ok(graph)
    }
    pub fn graph(&self) -> synth_route::Graph<'_> {
        synth_route::Graph {
            offsets: &self.offsets,
            edge_dst: &self.dst,
            edge_delay: &self.delay,
            node_x: &self.x,
            node_y: &self.y,
            node_cost: &self.cost,
        }
    }
    pub fn features(
        &self,
        db: &Device,
        routes: &[synth_route::NetRoute],
    ) -> std::collections::BTreeSet<String> {
        let mut f = std::collections::BTreeSet::new();
        for r in routes {
            for &e in &r.edges {
                let (t, p) = self.pip[e as usize];
                let tile = &db.tiles[t as usize];
                for feature in &db.templates[tile.template].pips[p as usize].config {
                    // The single-site tile's logical Y0 uses the upper half
                    // of the two-site bit layout at a clock-region boundary.
                    let feature = if tile.kind.ends_with("_SING")
                        && tile.bits.values().any(|r| r.alias.as_ref().is_some_and(|a| a.1 == 0))
                    {
                        feature.replace("_Y0.", "_Y1.")
                    } else {
                        feature.clone()
                    };
                    f.insert(format!("{}.{feature}", tile.name));
                }
            }
        }
        let mut used = std::collections::BTreeSet::new();
        for r in routes {
            for &e in &r.edges {
                let (ti, pi) = self.pip[e as usize];
                let t = &db.tiles[ti as usize];
                let p = &db.templates[t.template].pips[pi as usize];
                used.insert((ti as usize, p.src as usize));
            }
        }
        let mut global = std::collections::BTreeMap::<String, (i16, i16)>::new();
        let mut regional =
            std::collections::BTreeMap::<i16, std::collections::BTreeSet<String>>::new();
        for (ti, t) in db.tiles.iter().enumerate() {
            let template = &db.templates[t.template];
            for w in 0..t.nodes.len() {
                if !used.contains(&(ti, w)) {
                    continue;
                }
                let wire = &template.wires[w];
                if t.kind.starts_with("CLK_HROW_") {
                    if let Some(lane) = wire.strip_prefix("CLK_HROW_R_CK_").filter(|s| {
                        s.starts_with("GCLK") && s[4..].bytes().all(|b| b.is_ascii_digit())
                    }) {
                        let entry = global.entry(lane.into()).or_insert((t.y, t.y));
                        entry.0 = entry.0.min(t.y);
                        entry.1 = entry.1.max(t.y);
                    }
                    if wire.starts_with("CLK_HROW_CK_IN_") {
                        f.insert(format!("{}.{wire}_ACTIVE", t.name));
                    }
                }
                if (t.kind.starts_with("HCLK_L") || t.kind.starts_with("HCLK_R"))
                    && let Some(lane) =
                        wire.strip_prefix("HCLK_CK_").filter(|s| s.starts_with("BUFHCLK"))
                {
                    f.insert(format!("{}.ENABLE_BUFFER.{wire}", t.name));
                    regional.entry(t.y).or_default().insert(lane.into());
                }
                if t.kind.starts_with("HCLK_CMT")
                    && wire.starts_with("HCLK_CMT_CCIO")
                    && wire[13..].bytes().all(|b| b.is_ascii_digit())
                {
                    f.insert(format!("{}.{wire}_ACTIVE", t.name));
                    f.insert(format!("{}.{wire}_USED", t.name));
                }
            }
        }
        for t in &db.tiles {
            if t.kind == "CLK_BUFG_BOT_R" || t.kind == "CLK_BUFG_TOP_R" {
                for (lane, (lo, hi)) in &mut global {
                    let n = lane[4..].parse::<u32>().unwrap();
                    if (n < 16) == (t.kind == "CLK_BUFG_BOT_R") {
                        *lo = (*lo).min(t.y);
                        *hi = (*hi).max(t.y);
                    }
                }
            }
        }
        for t in &db.tiles {
            if t.kind.starts_with("CLK_HROW_") {
                for (lane, &(lo, hi)) in &global {
                    if (lo..=hi).contains(&t.y) {
                        f.insert(format!("{}.CLK_HROW_R_CK_{lane}_ACTIVE", t.name));
                    }
                }
            }
            if t.kind == "CLK_BUFG_REBUF" {
                for lane in global.keys() {
                    for direction in ["ABOVE", "BELOW"] {
                        f.insert(format!("{}.{lane}_ENABLE_{direction}", t.name));
                    }
                }
            }
            if t.kind.starts_with("HCLK_CMT")
                && let Some(lanes) = regional.get(&t.y)
            {
                for lane in lanes {
                    f.insert(format!("{}.HCLK_CMT_CK_{lane}_USED", t.name));
                }
            }
        }

        f
    }
}

/// Check every routed endpoint and exclusive node ownership before generating
/// configuration. This is independent of PathFinder's occupancy bookkeeping.
pub fn verify(
    graph: synth_route::Graph<'_>,
    requests: &[synth_route::NetRequest],
    routes: &[synth_route::NetRoute],
) -> Result<(), String> {
    use std::collections::BTreeSet;
    if requests.len() != routes.len() {
        return Err("route count differs from net requests".into());
    }
    let mut owner = vec![None; graph.num_nodes()];
    for (net, (request, route)) in requests.iter().zip(routes).enumerate() {
        let mut reached = BTreeSet::from([request.source]);
        for &edge in &route.edges {
            let dst = *graph.edge_dst.get(edge as usize).ok_or("invalid routed edge")?;
            let src = (graph.offsets.partition_point(|&n| n <= edge) - 1) as u32;
            if !reached.contains(&src) || !reached.insert(dst) {
                return Err(format!("net {net}: route is not a source-connected tree"));
            }
        }
        if request.sinks.iter().any(|n| !reached.contains(n)) {
            return Err(format!("net {net}: route does not reach every requested sink"));
        }
        for node in reached {
            let slot = owner.get_mut(node as usize).ok_or("routed node out of range")?;
            if let Some(other) = slot {
                return Err(format!("nets {other} and {net} share routing node {node}"));
            }
            *slot = Some(net);
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    #[test]
    fn rejects_disconnected_and_shorted_routes() {
        use synth_route::{Graph, NetRequest, NetRoute};
        let g = Graph {
            offsets: &[0, 1, 2, 2],
            edge_dst: &[1, 2],
            edge_delay: &[0., 0.],
            node_x: &[0; 3],
            node_y: &[0; 3],
            node_cost: &[1.; 3],
        };
        let n = NetRequest { source: 0, sinks: vec![2], criticality: vec![] };
        let route = NetRoute { edges: vec![0, 1] };
        assert!(super::verify(g, std::slice::from_ref(&n), std::slice::from_ref(&route)).is_ok());
        assert!(
            super::verify(g, std::slice::from_ref(&n), &[NetRoute { edges: vec![1] }]).is_err()
        );
        assert!(
            super::verify(g, std::slice::from_ref(&n), &[NetRoute { edges: vec![0] }]).is_err()
        );
        assert!(
            super::verify(g, &[n.clone(), n], &[route.clone(), route])
                .unwrap_err()
                .contains("share routing node")
        );
    }
}
