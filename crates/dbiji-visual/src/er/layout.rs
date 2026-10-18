//! Automatic layout of ER diagrams (layered, Sugiyama style).
//!
//! Pipeline for every connected component of the FK graph:
//! 1. graph parent -> child (referenced -> referencing table) with petgraph,
//!    self references and duplicate edges removed;
//! 2. cycle breaking with petgraph's greedy feedback arc set (reversed edges);
//! 3. longest-path layering, sources pulled next to their children;
//! 4. width-bounded layer splitting so wide layers (hub tables with many
//!    children) wrap instead of producing extreme aspect ratios;
//! 5. dummy nodes for edges spanning several layers;
//! 6. crossing reduction with alternating barycenter sweeps plus adjacent
//!    transpositions, keeping the best ordering found;
//! 7. coordinate assignment honoring real box sizes: within a layer boxes are
//!    placed by weighted isotonic regression (pool adjacent violators) towards
//!    the barycenter of their neighbours, which guarantees the minimum gap;
//!    layers are spaced by their thickest box.
//!
//! Components are packed in shelves (largest first); isolated tables form a
//! grid placed after them. Everything iterates in table-key order, so the
//! result is deterministic and independent of the input order.

use super::diagram::ErSettings;
use super::geometry::table_geometry;
use super::model::ErModel;
use egui::{Pos2, Rect, Vec2, pos2, vec2};
use petgraph::algo::greedy_feedback_arc_set;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::unionfind::UnionFind;
use petgraph::visit::EdgeRef;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Direction in which layers follow each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LayoutDirection {
    /// Referenced tables on the left, referencing tables to their right
    /// (relations attach to the box sides, so this routes most cleanly).
    #[default]
    LeftRight,
    /// Referenced tables on top, referencing tables below.
    TopDown,
}

/// Parameters of [`auto_layout`] and [`layout_incremental`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutOptions {
    /// Display settings that determine box sizes.
    pub settings: ErSettings,
    /// Layer direction.
    pub direction: LayoutDirection,
    /// Gap between consecutive layers.
    pub layer_spacing: f32,
    /// Gap between boxes of the same layer.
    pub node_spacing: f32,
    /// Gap between packed components.
    pub component_spacing: f32,
    /// Number of crossing reduction sweeps.
    pub sweeps: usize,
    /// Desired width / height ratio of the whole diagram.
    pub aspect_ratio: f32,
}

impl Default for LayoutOptions {
    fn default() -> Self {
        LayoutOptions {
            settings: ErSettings::default(),
            direction: LayoutDirection::LeftRight,
            layer_spacing: 90.0,
            node_spacing: 30.0,
            component_spacing: 80.0,
            sweeps: 12,
            aspect_ratio: 1.6,
        }
    }
}

impl LayoutOptions {
    /// Default options for boxes drawn with `settings`.
    pub fn from_settings(settings: ErSettings) -> Self {
        LayoutOptions { settings, ..Default::default() }
    }
}

/// A laid out group of tables with positions relative to its own origin.
struct Block {
    /// Global node indices.
    nodes: Vec<usize>,
    /// Top-left corner of each node, relative to the block.
    pos: Vec<Pos2>,
    size: Vec2,
}

/// Computes the position (top-left corner) of every table of `model`.
pub fn auto_layout(model: &ErModel, options: &LayoutOptions) -> BTreeMap<String, Pos2> {
    let mut idx: Vec<usize> = (0..model.tables.len()).collect();
    idx.sort_by(|&a, &b| model.tables[a].key.cmp(&model.tables[b].key));
    idx.dedup_by(|a, b| model.tables[*a].key == model.tables[*b].key);
    let keys: Vec<&str> = idx.iter().map(|&i| model.tables[i].key.as_str()).collect();
    let sizes: Vec<Vec2> = idx.iter().map(|&i| table_geometry(&model.tables[i], &options.settings).size).collect();
    let index: HashMap<&str, usize> = keys.iter().enumerate().map(|(i, k)| (*k, i)).collect();
    let n = keys.len();

    let mut edges: BTreeSet<(usize, usize)> = BTreeSet::new();
    for r in &model.relations {
        if let (Some(&child), Some(&parent)) = (index.get(r.from_table.as_str()), index.get(r.to_table.as_str()))
            && child != parent {
                edges.insert((parent, child));
            }
    }

    let mut uf = UnionFind::<usize>::new(n);
    for &(a, b) in &edges {
        uf.union(a, b);
    }
    let mut comps: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for v in 0..n {
        comps.entry(uf.find(v)).or_default().push(v);
    }
    let mut comp_edges: BTreeMap<usize, Vec<(usize, usize)>> = BTreeMap::new();
    for &(a, b) in &edges {
        comp_edges.entry(uf.find(a)).or_default().push((a, b));
    }

    let mut blocks = Vec::new();
    let mut isolated = Vec::new();
    for (root, nodes) in comps {
        if nodes.len() == 1 {
            isolated.extend(nodes);
        } else {
            let e = comp_edges.remove(&root).unwrap_or_default();
            blocks.push(layered_block(&nodes, &e, &sizes, options));
        }
    }
    blocks.sort_by(|a, b| b.nodes.len().cmp(&a.nodes.len()).then_with(|| a.nodes.first().cmp(&b.nodes.first())));
    if !isolated.is_empty() {
        blocks.push(grid_block(&isolated, &sizes, options));
    }

    let offsets = pack_blocks(&blocks, options);
    let mut out = BTreeMap::new();
    for (block, offset) in blocks.iter().zip(offsets) {
        for (&v, p) in block.nodes.iter().zip(&block.pos) {
            if let Some(key) = keys.get(v) {
                out.insert((*key).to_string(), pos2((p.x + offset.x).round(), (p.y + offset.y).round()));
            }
        }
    }
    out
}

/// Positions for every table of `model`: tables that already have a
/// position in `positions` keep it (they are pinned); new tables are placed
/// near the tables they are related to, in free space; unrelated new tables
/// are laid out together to the right of the existing diagram. Positions of
/// tables that are no longer part of the model are dropped.
pub fn layout_incremental(
    model: &ErModel,
    positions: &BTreeMap<String, Pos2>,
    options: &LayoutOptions,
) -> BTreeMap<String, Pos2> {
    let keys: BTreeSet<&str> = model.tables.iter().map(|t| t.key.as_str()).collect();
    let mut out: BTreeMap<String, Pos2> =
        positions.iter().filter(|(k, _)| keys.contains(k.as_str())).map(|(k, p)| (k.clone(), *p)).collect();
    if out.is_empty() {
        return auto_layout(model, options);
    }
    let sizes: HashMap<&str, Vec2> =
        model.tables.iter().map(|t| (t.key.as_str(), table_geometry(t, &options.settings).size)).collect();
    let mut missing: BTreeSet<&str> = keys.iter().copied().filter(|k| !out.contains_key(*k)).collect();
    if missing.is_empty() {
        return out;
    }
    let mut neighbours: HashMap<&str, BTreeSet<&str>> = HashMap::new();
    for r in &model.relations {
        if r.from_table != r.to_table {
            neighbours.entry(r.from_table.as_str()).or_default().insert(r.to_table.as_str());
            neighbours.entry(r.to_table.as_str()).or_default().insert(r.from_table.as_str());
        }
    }
    let gap = options.node_spacing.max(10.0);
    let mut index = SpatialIndex::new(256.0);
    for (k, p) in &out {
        if let Some(s) = sizes.get(k.as_str()) {
            index.insert(Rect::from_min_size(*p, *s));
        }
    }

    // Place tables adjacent to placed ones first (breadth first, key order).
    loop {
        let next = missing
            .iter()
            .copied()
            .find(|k| neighbours.get(k).map(|ns| ns.iter().any(|n| out.contains_key(*n))).unwrap_or(false));
        let Some(key) = next else { break };
        missing.remove(key);
        let size = sizes.get(key).copied().unwrap_or(vec2(160.0, 60.0));
        let placed: Vec<Rect> = neighbours
            .get(key)
            .map(|ns| {
                ns.iter()
                    .filter_map(|n| Some(Rect::from_min_size(*out.get(*n)?, *sizes.get(n)?)))
                    .collect()
            })
            .unwrap_or_default();
        let pos = find_free_spot(&index, &placed, size, gap);
        index.insert(Rect::from_min_size(pos, size));
        out.insert(key.to_string(), pos);
    }

    // Unrelated new tables: lay them out on their own, right of everything.
    if !missing.is_empty() {
        let sub = ErModel {
            tables: model.tables.iter().filter(|t| missing.contains(t.key.as_str())).cloned().collect(),
            relations: model
                .relations
                .iter()
                .filter(|r| missing.contains(r.from_table.as_str()) && missing.contains(r.to_table.as_str()))
                .cloned()
                .collect(),
        };
        let sub_pos = auto_layout(&sub, options);
        let bounds = index.bounds().unwrap_or(Rect::from_min_size(Pos2::ZERO, Vec2::ZERO));
        let offset = vec2(bounds.max.x + options.component_spacing, bounds.min.y);
        for (k, p) in sub_pos {
            out.insert(k, pos2((p.x + offset.x).round(), (p.y + offset.y).round()));
        }
    }
    out
}

// ---------------------------------------------------------------- layering

/// A table in the layered graph.
#[derive(Clone, Copy)]
struct LNode {
    /// Size along the layer (stacking direction).
    along: f32,
    /// Size across layers.
    across: f32,
    layer: usize,
}

fn layered_block(nodes: &[usize], edges: &[(usize, usize)], sizes: &[Vec2], opt: &LayoutOptions) -> Block {
    let m = nodes.len();
    let local: HashMap<usize, usize> = nodes.iter().enumerate().map(|(i, &g)| (g, i)).collect();
    let horizontal = opt.direction == LayoutDirection::LeftRight;
    let along_of = |s: Vec2| if horizontal { s.y } else { s.x };
    let across_of = |s: Vec2| if horizontal { s.x } else { s.y };
    let size_of = |l: usize| nodes.get(l).and_then(|&g| sizes.get(g)).copied().unwrap_or(vec2(160.0, 60.0));

    // 1-2: graph and cycle breaking.
    let mut g: DiGraph<(), ()> = DiGraph::with_capacity(m, edges.len());
    for _ in 0..m {
        g.add_node(());
    }
    for &(a, b) in edges {
        if let (Some(&la), Some(&lb)) = (local.get(&a), local.get(&b)) {
            g.add_edge(NodeIndex::new(la), NodeIndex::new(lb), ());
        }
    }
    let reversed: BTreeSet<_> = greedy_feedback_arc_set(&g).map(|e| e.id()).collect();
    let mut dag: Vec<(usize, usize)> = g
        .edge_references()
        .map(|e| {
            let (s, t) = (e.source().index(), e.target().index());
            if reversed.contains(&e.id()) { (t, s) } else { (s, t) }
        })
        .filter(|(s, t)| s != t)
        .collect();
    dag.sort_unstable();
    dag.dedup();

    let mut children = vec![Vec::new(); m];
    let mut parents = vec![Vec::new(); m];
    for &(a, b) in &dag {
        children[a].push(b);
        parents[b].push(a);
    }
    let topo = topological_order(m, &children, &parents);

    // 3: longest path layering, then pull sources next to their children.
    let mut layer = vec![0usize; m];
    for &v in &topo {
        for &c in &children[v] {
            layer[c] = layer[c].max(layer[v] + 1);
        }
    }
    for &v in topo.iter().rev() {
        if parents[v].is_empty()
            && let Some(min_child) = children[v].iter().map(|&c| layer[c]).min() {
                layer[v] = min_child.saturating_sub(1).max(layer[v]);
            }
    }

    // 4: width-bounded splitting.
    let area: f32 = (0..m)
        .map(|v| {
            let s = size_of(v);
            (s.x + opt.node_spacing) * (s.y + opt.node_spacing)
        })
        .sum();
    let aspect = if opt.aspect_ratio.is_finite() && opt.aspect_ratio > 0.05 { opt.aspect_ratio } else { 1.6 };
    let target = if horizontal { (area / aspect).sqrt() } else { (area * aspect).sqrt() };
    let max_node = (0..m).map(|v| along_of(size_of(v))).fold(0.0, f32::max);
    let max_extent = target.max(max_node) * 1.15;
    let mut min_layer = layer.clone();
    let mut guard = 0usize;
    let mut start = 0usize;
    loop {
        guard += 1;
        if guard > m * m + 16 {
            break;
        }
        let num_layers = layer.iter().copied().max().map_or(0, |l| l + 1);
        let mut by_layer: Vec<Vec<usize>> = vec![Vec::new(); num_layers];
        for v in 0..m {
            by_layer[layer[v]].push(v);
        }
        let extent = |vs: &[usize]| -> f32 {
            vs.iter().map(|&v| along_of(size_of(v))).sum::<f32>() + opt.node_spacing * vs.len().saturating_sub(1) as f32
        };
        let Some(l) = (start..num_layers).find(|&l| extent(&by_layer[l]) > max_extent) else { break };
        let mut cand = by_layer[l].clone();
        let prev: BTreeSet<usize> = if l > 0 { by_layer[l - 1].iter().copied().collect() } else { BTreeSet::new() };
        cand.sort_by_key(|&v| {
            let ups = parents[v].iter().filter(|p| prev.contains(p)).count();
            (children[v].is_empty(), std::cmp::Reverse(ups), v)
        });
        let mut acc = 0.0;
        let mut kept = 0usize;
        for &v in &cand {
            let add = along_of(size_of(v)) + if kept == 0 { 0.0 } else { opt.node_spacing };
            if kept == 0 || acc + add <= max_extent {
                acc += add;
                kept += 1;
            } else {
                min_layer[v] = l + 1;
            }
        }
        for &v in &topo {
            let from_parents = parents[v].iter().map(|&p| layer[p] + 1).max().unwrap_or(0);
            layer[v] = min_layer[v].max(from_parents);
        }
        start = l + 1;
    }

    // 5: neighbour lists. Long edges connect their end nodes directly: the
    // relation routing attaches to box sides and does not use bend points,
    // so dummy nodes would only inflate wrapped layers.
    let ln: Vec<LNode> = (0..m)
        .map(|v| {
            let s = size_of(v);
            LNode { along: along_of(s), across: across_of(s), layer: layer[v] }
        })
        .collect();
    let up: Vec<Vec<usize>> = parents.clone();
    let down: Vec<Vec<usize>> = children.clone();
    let up_adj: Vec<Vec<usize>> =
        (0..m).map(|v| up[v].iter().copied().filter(|&u| ln[u].layer + 1 == ln[v].layer).collect()).collect();
    let down_adj: Vec<Vec<usize>> =
        (0..m).map(|v| down[v].iter().copied().filter(|&u| ln[v].layer + 1 == ln[u].layer).collect()).collect();
    let num_layers = ln.iter().map(|n| n.layer).max().map_or(1, |l| l + 1);

    // 6: ordering. Barycenters use normalised ranks so neighbours in
    // non-adjacent layers of different sizes are comparable.
    let mut layers: Vec<Vec<usize>> = vec![Vec::new(); num_layers];
    let mut rank = vec![0.0f32; m];
    let mut position = vec![0usize; m];
    for (l, layer) in layers.iter_mut().enumerate() {
        let mut items: Vec<(f32, usize)> = (0..m)
            .filter(|&v| ln[v].layer == l)
            .map(|v| (if l == 0 { v as f32 } else { barycenter(&up[v], &rank).unwrap_or(f32::MAX) }, v))
            .collect();
        items.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        *layer = items.into_iter().map(|(_, v)| v).collect();
        set_ranks(layer, &mut rank, &mut position);
    }
    let mut best = layers.clone();
    let mut best_crossings = total_crossings(&layers, &down_adj, &position);
    for sweep in 0..opt.sweeps {
        if best_crossings == 0 {
            break;
        }
        if sweep % 2 == 0 {
            for layer in layers.iter_mut().skip(1) {
                reorder_layer(layer, &up, &mut rank, &mut position);
            }
        } else {
            let last = layers.len().saturating_sub(1);
            for layer in layers[..last].iter_mut().rev() {
                reorder_layer(layer, &down, &mut rank, &mut position);
            }
        }
        transpose(&mut layers, &up_adj, &down_adj, &mut rank, &mut position);
        let c = total_crossings(&layers, &down_adj, &position);
        if c < best_crossings {
            best_crossings = c;
            best = layers.clone();
        }
    }
    let layers = best;
    for layer in &layers {
        set_ranks(layer, &mut rank, &mut position);
    }

    // 7: coordinates along the layers.
    let gap = opt.node_spacing;
    let mut center = vec![0.0f32; m];
    for layer in &layers {
        let mut acc = 0.0;
        for (i, &v) in layer.iter().enumerate() {
            if i > 0 {
                acc += gap;
            }
            center[v] = acc + ln[v].along * 0.5;
            acc += ln[v].along;
        }
        let half = acc * 0.5;
        for &v in layer {
            center[v] -= half;
        }
    }
    for iter in 0..10 {
        let use_up = iter % 2 == 0;
        let order: Vec<usize> =
            if use_up { (1..num_layers).collect() } else { (0..num_layers.saturating_sub(1)).rev().collect() };
        let nb = if use_up { &up } else { &down };
        for l in order {
            let layer = &layers[l];
            let desired: Vec<f32> = layer
                .iter()
                .map(|&v| {
                    if nb[v].is_empty() {
                        center[v]
                    } else {
                        nb[v].iter().map(|&u| center[u]).sum::<f32>() / nb[v].len() as f32
                    }
                })
                .collect();
            let weights: Vec<f32> = layer.iter().map(|&v| if nb[v].is_empty() { 0.05 } else { 1.0 }).collect();
            let widths: Vec<f32> = layer.iter().map(|&v| ln[v].along).collect();
            let gaps = vec![gap; layer.len().saturating_sub(1)];
            let placed = place_in_layer(&widths, &desired, &weights, &gaps);
            for (&v, c) in layer.iter().zip(placed) {
                center[v] = c;
            }
        }
    }

    // Across: layers spaced by their thickest box.
    let mut offsets = vec![0.0f32; num_layers];
    let mut acc = 0.0;
    for l in 0..num_layers {
        offsets[l] = acc;
        let thickness = layers[l].iter().map(|&v| ln[v].across).fold(0.0, f32::max);
        acc += thickness + opt.layer_spacing;
    }

    let mut pos = vec![Pos2::ZERO; m];
    for (v, node) in ln.iter().enumerate() {
        let a = center[v] - node.along * 0.5;
        let c = offsets[node.layer];
        pos[v] = if horizontal { pos2(c, a) } else { pos2(a, c) };
    }
    normalize(nodes, pos, size_of)
}

fn set_ranks(layer: &[usize], rank: &mut [f32], position: &mut [usize]) {
    let len = layer.len().max(1) as f32;
    for (i, &v) in layer.iter().enumerate() {
        position[v] = i;
        rank[v] = (i as f32 + 0.5) / len;
    }
}


/// Shifts positions so the block starts at the origin and computes its size.
fn normalize(nodes: &[usize], mut pos: Vec<Pos2>, size_of: impl Fn(usize) -> Vec2) -> Block {
    let mut min = pos2(f32::INFINITY, f32::INFINITY);
    let mut max = pos2(f32::NEG_INFINITY, f32::NEG_INFINITY);
    for (l, p) in pos.iter().enumerate() {
        let s = size_of(l);
        min = min.min(*p);
        max = max.max(*p + s);
    }
    if !min.x.is_finite() {
        return Block { nodes: nodes.to_vec(), pos, size: Vec2::ZERO };
    }
    for p in &mut pos {
        *p = pos2(p.x - min.x, p.y - min.y);
    }
    Block { nodes: nodes.to_vec(), pos, size: max - min }
}

/// Kahn's algorithm, smallest index first (deterministic). Nodes on cycles
/// (cannot happen after cycle breaking) are appended in index order.
fn topological_order(m: usize, children: &[Vec<usize>], parents: &[Vec<usize>]) -> Vec<usize> {
    let mut indeg: Vec<usize> = parents.iter().map(Vec::len).collect();
    let mut ready: BTreeSet<usize> = (0..m).filter(|&v| indeg[v] == 0).collect();
    let mut out = Vec::with_capacity(m);
    let mut done = vec![false; m];
    while let Some(v) = ready.pop_first() {
        out.push(v);
        done[v] = true;
        for &c in &children[v] {
            indeg[c] = indeg[c].saturating_sub(1);
            if indeg[c] == 0 && !done[c] {
                ready.insert(c);
            }
        }
    }
    if out.len() < m {
        out.extend((0..m).filter(|&v| !done[v]));
    }
    out
}

/// Mean normalised rank of the neighbours.
fn barycenter(neighbours: &[usize], rank: &[f32]) -> Option<f32> {
    if neighbours.is_empty() {
        return None;
    }
    Some(neighbours.iter().map(|&u| rank[u]).sum::<f32>() / neighbours.len() as f32)
}

/// Sorts a layer by the barycenter of the given neighbours; nodes without
/// neighbours keep their rank.
fn reorder_layer(layer: &mut [usize], neighbours: &[Vec<usize>], rank: &mut [f32], position: &mut [usize]) {
    let mut items: Vec<(f32, usize, usize)> = layer
        .iter()
        .map(|&v| (barycenter(&neighbours[v], rank).unwrap_or(rank[v]), position[v], v))
        .collect();
    items.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    for (i, (_, _, v)) in items.into_iter().enumerate() {
        layer[i] = v;
    }
    set_ranks(layer, rank, position);
}

/// Crossings between consecutive layers `l` and `l + 1` (Fenwick tree count of inversions).
fn crossings_between(upper: &[usize], lower_len: usize, down: &[Vec<usize>], position: &[usize]) -> usize {
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for &u in upper {
        for &v in &down[u] {
            pairs.push((position[u], position[v]));
        }
    }
    pairs.sort_unstable();
    let mut tree = vec![0usize; lower_len + 1];
    let mut count = 0usize;
    for (seen, &(_, b)) in pairs.iter().enumerate() {
        if b >= lower_len {
            continue; // not an edge into the next layer
        }
        // number of earlier edges whose lower end is > b
        let mut i = b + 1;
        let mut le = 0;
        while i > 0 {
            le += tree[i];
            i &= i - 1;
        }
        count += seen.saturating_sub(le);
        let mut i = b + 1;
        while i <= lower_len {
            tree[i] += 1;
            i += i & i.wrapping_neg();
        }
    }
    count
}

fn total_crossings(layers: &[Vec<usize>], down: &[Vec<usize>], position: &[usize]) -> usize {
    layers.windows(2).map(|w| crossings_between(&w[0], w[1].len(), down, position)).sum()
}

/// Crossings between the edges of `u` and `v` (u left of v) towards one neighbour layer.
fn pair_crossings(u: usize, v: usize, neighbours: &[Vec<usize>], position: &[usize]) -> usize {
    let mut c = 0;
    for &a in &neighbours[u] {
        for &b in &neighbours[v] {
            if position[a] > position[b] {
                c += 1;
            }
        }
    }
    c
}

/// Swaps adjacent nodes while that reduces crossings with the adjacent
/// layers (`up` / `down` only contain neighbours in adjacent layers).
fn transpose(
    layers: &mut [Vec<usize>],
    up: &[Vec<usize>],
    down: &[Vec<usize>],
    rank: &mut [f32],
    position: &mut [usize],
) {
    for _pass in 0..4 {
        let mut improved = false;
        for layer in layers.iter_mut() {
            for i in 0..layer.len().saturating_sub(1) {
                let (u, v) = (layer[i], layer[i + 1]);
                let before = pair_crossings(u, v, up, position) + pair_crossings(u, v, down, position);
                let after = pair_crossings(v, u, up, position) + pair_crossings(v, u, down, position);
                if after < before {
                    layer.swap(i, i + 1);
                    position[u] = i + 1;
                    position[v] = i;
                    improved = true;
                }
            }
            set_ranks(layer, rank, position);
        }
        if !improved {
            break;
        }
    }
}

/// Places boxes of one layer (in order) as close as possible to their
/// desired centres while keeping `gaps[i]` between box `i` and `i + 1`:
/// weighted isotonic regression via pool adjacent violators.
pub(crate) fn place_in_layer(widths: &[f32], desired: &[f32], weights: &[f32], gaps: &[f32]) -> Vec<f32> {
    let n = widths.len();
    if n == 0 {
        return Vec::new();
    }
    // offsets o_i so that c_i - o_i must be non-decreasing
    let mut offs = Vec::with_capacity(n);
    let mut acc = 0.0f32;
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            let gap = gaps.get(i - 1).copied().unwrap_or(0.0).max(0.0);
            acc += (widths[i - 1] + w) * 0.5 + gap;
        }
        offs.push(acc);
    }
    // blocks: (weight sum, weighted value sum, count)
    let mut blocks: Vec<(f32, f32, usize)> = Vec::with_capacity(n);
    for (i, off) in offs.iter().enumerate() {
        let w = weights.get(i).copied().unwrap_or(1.0).max(1e-3);
        let t = desired.get(i).copied().unwrap_or(0.0) - off;
        blocks.push((w, w * t, 1));
        while blocks.len() >= 2 {
            let (w2, s2, c2) = blocks[blocks.len() - 1];
            let (w1, s1, c1) = blocks[blocks.len() - 2];
            if s1 / w1 > s2 / w2 {
                blocks.pop();
                blocks.pop();
                blocks.push((w1 + w2, s1 + s2, c1 + c2));
            } else {
                break;
            }
        }
    }
    let mut out = Vec::with_capacity(n);
    let mut i = 0;
    for (w, s, c) in blocks {
        let y = s / w;
        for _ in 0..c {
            out.push(y + offs[i]);
            i += 1;
        }
    }
    out
}

// ---------------------------------------------------------------- packing

fn grid_block(nodes: &[usize], sizes: &[Vec2], opt: &LayoutOptions) -> Block {
    let gap = opt.node_spacing;
    let get = |g: usize| sizes.get(g).copied().unwrap_or(vec2(160.0, 60.0));
    let area: f32 = nodes.iter().map(|&g| (get(g).x + gap) * (get(g).y + gap)).sum();
    let max_w = nodes.iter().map(|&g| get(g).x).fold(0.0, f32::max);
    let limit = (area * opt.aspect_ratio.max(0.1)).sqrt().max(max_w);
    let (mut x, mut y, mut row_h) = (0.0f32, 0.0f32, 0.0f32);
    let mut pos = Vec::with_capacity(nodes.len());
    let mut size = Vec2::ZERO;
    for &g in nodes {
        let s = get(g);
        if x > 0.0 && x + s.x > limit {
            x = 0.0;
            y += row_h + gap;
            row_h = 0.0;
        }
        pos.push(pos2(x, y));
        size = size.max(vec2(x + s.x, y + s.y));
        x += s.x + gap;
        row_h = row_h.max(s.y);
    }
    Block { nodes: nodes.to_vec(), pos, size }
}

fn pack_blocks(blocks: &[Block], opt: &LayoutOptions) -> Vec<Vec2> {
    let gap = opt.component_spacing;
    let area: f32 = blocks.iter().map(|b| (b.size.x + gap) * (b.size.y + gap)).sum();
    let max_w = blocks.iter().map(|b| b.size.x).fold(0.0, f32::max);
    let limit = (area * opt.aspect_ratio.max(0.1)).sqrt().max(max_w);
    let (mut x, mut y, mut row_h) = (0.0f32, 0.0f32, 0.0f32);
    let mut out = Vec::with_capacity(blocks.len());
    for b in blocks {
        if x > 0.0 && x + b.size.x > limit {
            x = 0.0;
            y += row_h + gap;
            row_h = 0.0;
        }
        out.push(vec2(x, y));
        x += b.size.x + gap;
        row_h = row_h.max(b.size.y);
    }
    out
}

// ------------------------------------------------------- incremental helpers

/// Uniform grid of rectangles for overlap queries.
struct SpatialIndex {
    cell: f32,
    rects: Vec<Rect>,
    cells: HashMap<(i32, i32), Vec<usize>>,
}

impl SpatialIndex {
    fn new(cell: f32) -> Self {
        SpatialIndex { cell, rects: Vec::new(), cells: HashMap::new() }
    }

    fn cell_range(&self, r: Rect) -> (i32, i32, i32, i32) {
        let f = |v: f32| (v / self.cell).floor().clamp(-1e6, 1e6) as i32;
        (f(r.min.x), f(r.min.y), f(r.max.x), f(r.max.y))
    }

    fn insert(&mut self, r: Rect) {
        let id = self.rects.len();
        self.rects.push(r);
        let (x0, y0, x1, y1) = self.cell_range(r);
        for cx in x0..=x1 {
            for cy in y0..=y1 {
                self.cells.entry((cx, cy)).or_default().push(id);
            }
        }
    }

    fn overlaps(&self, r: Rect) -> bool {
        let (x0, y0, x1, y1) = self.cell_range(r);
        for cx in x0..=x1 {
            for cy in y0..=y1 {
                if let Some(ids) = self.cells.get(&(cx, cy))
                    && ids.iter().any(|&i| self.rects.get(i).is_some_and(|o| o.intersects(r))) {
                        return true;
                    }
            }
        }
        false
    }

    fn bounds(&self) -> Option<Rect> {
        self.rects.iter().copied().reduce(|a, b| a.union(b))
    }
}

/// Free position for a box of `size` close to the boxes in `near`.
fn find_free_spot(index: &SpatialIndex, near: &[Rect], size: Vec2, gap: f32) -> Pos2 {
    let free = |p: Pos2| !index.overlaps(Rect::from_min_size(p, size).expand((gap - 0.5).max(0.0)));
    let anchor = if near.is_empty() {
        Pos2::ZERO
    } else {
        let c = near.iter().fold(Vec2::ZERO, |acc, r| acc + r.center().to_vec2()) / near.len() as f32;
        (c - size * 0.5).to_pos2()
    };
    // Candidates hugging the related boxes first.
    let mut candidates: Vec<Pos2> = Vec::new();
    for r in near {
        for k in 0..6 {
            let dy = k as f32 * (size.y + gap) * if k % 2 == 0 { 0.5 } else { -0.5 };
            candidates.push(pos2(r.max.x + gap * 2.0, r.min.y + dy));
            candidates.push(pos2(r.min.x - gap * 2.0 - size.x, r.min.y + dy));
            let dx = k as f32 * (size.x + gap) * if k % 2 == 0 { 0.5 } else { -0.5 };
            candidates.push(pos2(r.min.x + dx, r.max.y + gap * 2.0));
            candidates.push(pos2(r.min.x + dx, r.min.y - gap * 2.0 - size.y));
        }
    }
    candidates.sort_by(|a, b| a.distance_sq(anchor).total_cmp(&b.distance_sq(anchor)));
    if let Some(p) = candidates.into_iter().find(|p| free(*p)) {
        return p;
    }
    // Spiral search on a coarse grid.
    let step = 40.0;
    for ring in 1..200 {
        let r = ring as f32 * step;
        let n = ring * 8;
        for i in 0..n {
            let a = i as f32 / n as f32 * std::f32::consts::TAU;
            let p = pos2(anchor.x + r * a.cos(), anchor.y + r * a.sin());
            if free(p) {
                return pos2(p.x.round(), p.y.round());
            }
        }
    }
    let b = index.bounds().unwrap_or(Rect::from_min_size(Pos2::ZERO, Vec2::ZERO));
    pos2(b.max.x + gap * 2.0, b.min.y)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::er::model::{ErColumn, ErRelation, ErTable, ErTableKind};

    /// Deterministic pseudo random generator for test schemas.
    pub(crate) struct Lcg(u64);
    impl Lcg {
        pub(crate) fn new(seed: u64) -> Self {
            Lcg(seed.wrapping_mul(6364136223846793005).wrapping_add(1))
        }
        pub(crate) fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        pub(crate) fn below(&mut self, n: u64) -> u64 {
            if n == 0 { 0 } else { self.next() % n }
        }
    }

    /// Schema with `tables` tables (~10 columns each) and random FKs, including
    /// cycles, self references and isolated tables.
    pub(crate) fn generated_model(tables: usize, seed: u64) -> ErModel {
        let mut rng = Lcg::new(seed);
        let mut m = ErModel::default();
        for i in 0..tables {
            let mut t = ErTable::new(Some("bench"), format!("table_{i:03}"), ErTableKind::Table);
            t.columns.push(ErColumn::new("id", "bigint").primary_key());
            for c in 0..9 {
                let ty = ["varchar(120)", "integer", "timestamp with time zone", "numeric(12,2)", "boolean", "text"]
                    [rng.below(6) as usize];
                t.columns.push(ErColumn::new(format!("column_{c}_{}", rng.below(1000)), ty));
            }
            m.tables.push(t);
        }
        for i in 1..tables {
            // most tables reference 1-2 earlier tables; every 13th is isolated
            if i % 13 == 0 {
                continue;
            }
            let refs = 1 + rng.below(2) as usize;
            for k in 0..refs {
                let parent = rng.below(i as u64) as usize;
                let col = format!("ref_{k}_id");
                if let Some(t) = m.tables.get_mut(i) {
                    t.columns.push(ErColumn::new(col.clone(), "bigint").foreign_key());
                }
                m.relations.push(ErRelation {
                    name: format!("fk_{i}_{k}"),
                    from_table: format!("bench.table_{i:03}"),
                    from_columns: vec![col],
                    to_table: format!("bench.table_{parent:03}"),
                    to_columns: vec!["id".into()],
                    identifying: false,
                    optional: true,
                });
            }
        }
        // a few cycles and self references
        if tables > 10 {
            m.relations.push(ErRelation {
                name: "cycle".into(),
                from_table: "bench.table_001".into(),
                from_columns: vec!["id".into()],
                to_table: format!("bench.table_{:03}", tables - 1),
                to_columns: vec!["id".into()],
                identifying: false,
                optional: false,
            });
            m.relations.push(ErRelation {
                name: "self".into(),
                from_table: "bench.table_005".into(),
                from_columns: vec!["id".into()],
                to_table: "bench.table_005".into(),
                to_columns: vec!["id".into()],
                identifying: false,
                optional: true,
            });
        }
        m
    }

    fn rects(model: &ErModel, pos: &BTreeMap<String, Pos2>, settings: &ErSettings) -> Vec<(String, Rect)> {
        model
            .tables
            .iter()
            .map(|t| (t.key.clone(), Rect::from_min_size(pos[&t.key], table_geometry(t, settings).size)))
            .collect()
    }

    fn assert_no_overlaps(rs: &[(String, Rect)]) {
        for i in 0..rs.len() {
            for j in i + 1..rs.len() {
                let (a, b) = (&rs[i], &rs[j]);
                assert!(!a.1.shrink(0.5).intersects(b.1.shrink(0.5)), "{} {:?} overlaps {} {:?}", a.0, a.1, b.0, b.1);
            }
        }
    }

    #[test]
    fn layout_has_no_overlaps_and_is_deterministic() {
        for (n, seed) in [(1, 1), (2, 2), (12, 3), (60, 4), (200, 5)] {
            let model = generated_model(n, seed);
            for direction in [LayoutDirection::LeftRight, LayoutDirection::TopDown] {
                let opts = LayoutOptions { direction, ..Default::default() };
                let a = auto_layout(&model, &opts);
                assert_eq!(a.len(), n);
                let b = auto_layout(&model, &opts);
                assert_eq!(a, b, "layout must be deterministic");
                assert_no_overlaps(&rects(&model, &a, &opts.settings));
                // independent of input order
                let mut shuffled = model.clone();
                shuffled.tables.reverse();
                shuffled.relations.reverse();
                assert_eq!(auto_layout(&shuffled, &opts), a, "layout must not depend on input order");
                for p in a.values() {
                    assert!(p.x.is_finite() && p.y.is_finite() && p.x >= 0.0 && p.y >= 0.0);
                }
            }
        }
    }

    #[test]
    fn parents_come_before_children() {
        // a <- b <- c chain (c references b references a)
        let mut m = ErModel::default();
        for name in ["a", "b", "c"] {
            m.tables.push(ErTable::new(None, name, ErTableKind::Table).with_column(ErColumn::new("id", "int").primary_key()));
        }
        for (from, to) in [("b", "a"), ("c", "b")] {
            m.relations.push(ErRelation {
                name: format!("{from}_{to}"),
                from_table: from.into(),
                from_columns: vec!["id".into()],
                to_table: to.into(),
                to_columns: vec!["id".into()],
                identifying: false,
                optional: false,
            });
        }
        let lr = auto_layout(&m, &LayoutOptions::default());
        assert!(lr["a"].x < lr["b"].x && lr["b"].x < lr["c"].x, "{lr:?}");
        let td = auto_layout(&m, &LayoutOptions { direction: LayoutDirection::TopDown, ..Default::default() });
        assert!(td["a"].y < td["b"].y && td["b"].y < td["c"].y, "{td:?}");
    }

    #[test]
    fn hub_layers_wrap_and_aspect_is_reasonable() {
        let mut m = ErModel::default();
        m.tables.push(ErTable::new(None, "hub", ErTableKind::Table).with_column(ErColumn::new("id", "int").primary_key()));
        for i in 0..80 {
            let name = format!("child_{i:02}");
            m.tables.push(
                ErTable::new(None, name.clone(), ErTableKind::Table)
                    .with_column(ErColumn::new("id", "int").primary_key())
                    .with_column(ErColumn::new("hub_id", "int").foreign_key()),
            );
            m.relations.push(ErRelation {
                name: format!("fk_{i}"),
                from_table: name,
                from_columns: vec!["hub_id".into()],
                to_table: "hub".into(),
                to_columns: vec!["id".into()],
                identifying: false,
                optional: false,
            });
        }
        let opts = LayoutOptions::default();
        let pos = auto_layout(&m, &opts);
        let rs = rects(&m, &pos, &opts.settings);
        assert_no_overlaps(&rs);
        let bounds = rs.iter().map(|(_, r)| *r).reduce(|a, b| a.union(b)).unwrap();
        let ratio = bounds.width() / bounds.height();
        assert!((0.4..=4.0).contains(&ratio), "aspect ratio {ratio} of {bounds:?}");
        // the hub stays left of every child
        assert!(m.tables.iter().skip(1).all(|t| pos[&t.key].x > pos["hub"].x));
    }

    #[test]
    fn isolated_and_disconnected_tables_are_packed() {
        let mut m = ErModel::default();
        for i in 0..30 {
            m.tables.push(ErTable::new(None, format!("t{i:02}"), ErTableKind::Table).with_column(ErColumn::new("id", "int")));
        }
        let opts = LayoutOptions::default();
        let pos = auto_layout(&m, &opts);
        let rs = rects(&m, &pos, &opts.settings);
        assert_no_overlaps(&rs);
        let bounds = rs.iter().map(|(_, r)| *r).reduce(|a, b| a.union(b)).unwrap();
        assert!(bounds.width() < 6.0 * bounds.height() && bounds.height() < 6.0 * bounds.width());
    }

    #[test]
    fn crossing_reduction_untangles_simple_case() {
        // parents p0, p1; children c0 -> p1, c1 -> p0: optimal order has no crossing
        let mut m = ErModel::default();
        for name in ["p0", "p1", "c0", "c1"] {
            m.tables.push(ErTable::new(None, name, ErTableKind::Table).with_column(ErColumn::new("id", "int")));
        }
        for (from, to) in [("c0", "p1"), ("c1", "p0")] {
            m.relations.push(ErRelation {
                name: format!("{from}_{to}"),
                from_table: from.into(),
                from_columns: vec!["id".into()],
                to_table: to.into(),
                to_columns: vec!["id".into()],
                identifying: false,
                optional: false,
            });
        }
        // two components here; join them with a shared root so they are layered together
        m.tables.push(ErTable::new(None, "root", ErTableKind::Table).with_column(ErColumn::new("id", "int")));
        for p in ["p0", "p1"] {
            m.relations.push(ErRelation {
                name: format!("{p}_root"),
                from_table: p.into(),
                from_columns: vec!["id".into()],
                to_table: "root".into(),
                to_columns: vec!["id".into()],
                identifying: false,
                optional: false,
            });
        }
        let pos = auto_layout(&m, &LayoutOptions::default());
        let (p0, p1, c0, c1) = (pos["p0"].y, pos["p1"].y, pos["c0"].y, pos["c1"].y);
        assert_eq!(p0 < p1, c1 < c0, "edges p0-c1 and p1-c0 must not cross: {pos:?}");
    }

    #[test]
    fn pav_respects_gaps_and_targets() {
        let out = place_in_layer(&[10.0, 10.0, 10.0], &[0.0, 0.0, 0.0], &[1.0, 1.0, 1.0], &[5.0, 5.0]);
        assert_eq!(out.len(), 3);
        assert!((out[1] - out[0] - 15.0).abs() < 1e-4 && (out[2] - out[1] - 15.0).abs() < 1e-4);
        assert!((out[1]).abs() < 1e-4, "centred on the common target: {out:?}");
        let out = place_in_layer(&[10.0, 10.0], &[0.0, 100.0], &[1.0, 1.0], &[5.0]);
        assert_eq!(out, vec![0.0, 100.0]);
        // pair specific gaps
        let out = place_in_layer(&[10.0, 10.0, 10.0], &[0.0, 0.0, 0.0], &[1.0, 1.0, 1.0], &[2.0, 20.0]);
        assert!((out[1] - out[0] - 12.0).abs() < 1e-4 && (out[2] - out[1] - 30.0).abs() < 1e-4);
        assert!(place_in_layer(&[], &[], &[], &[]).is_empty());
    }

    #[test]
    fn incremental_keeps_pinned_and_avoids_overlaps() {
        let model = generated_model(40, 9);
        let opts = LayoutOptions::default();
        let full = auto_layout(&model, &opts);
        // drop a few positions: they must be re-placed, everything else pinned
        let mut partial = full.clone();
        let removed: Vec<String> = partial.keys().step_by(7).cloned().collect();
        for k in &removed {
            partial.remove(k);
        }
        partial.insert("stale.table".into(), Pos2::new(1.0, 1.0));
        let inc = layout_incremental(&model, &partial, &opts);
        assert_eq!(inc.len(), model.tables.len());
        assert!(!inc.contains_key("stale.table"));
        for (k, p) in &partial {
            if k != "stale.table" {
                assert_eq!(inc[k], *p, "pinned table {k} moved");
            }
        }
        assert_no_overlaps(&rects(&model, &inc, &opts.settings));
        assert_eq!(layout_incremental(&model, &inc, &opts), inc, "deterministic and stable");
        // empty positions fall back to a full layout
        assert_eq!(layout_incremental(&model, &BTreeMap::new(), &opts), full);
    }

    #[test]
    fn crossing_counter() {
        // upper [0,1], lower [2,3]; edges 0-3 and 1-2 cross once
        let down = vec![vec![3], vec![2], vec![], vec![]];
        let position = vec![0, 1, 0, 1];
        assert_eq!(crossings_between(&[0, 1], 2, &down, &position), 1);
        let down = vec![vec![2], vec![3], vec![], vec![]];
        assert_eq!(crossings_between(&[0, 1], 2, &down, &position), 0);
    }
}
