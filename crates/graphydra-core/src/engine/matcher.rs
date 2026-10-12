//! Pattern-matching executor.
//!
//! Executes [`Step`]s depth-first, binding row slots and pushing complete
//! rows into a sink. The sink can stop execution early (for `LIMIT`).
//! Within one statement an edge is bound at most once per row
//! (relationship isomorphism), which also guarantees termination of
//! variable-length expansion on cyclic graphs.

use std::collections::{HashMap, VecDeque};
use std::ops::ControlFlow;

use crate::engine::expr::{Ctx, eval_predicate, walk_path};
use crate::engine::plan::{EdgeAccess, NodeAccess, Step};
use crate::engine::result::{Budget, QueryStats};
use crate::engine::rv::Rv;
use crate::error::{GraphError, Result};
use crate::store::{Direction, Slot, Store};

/// Sink return value: `true` to continue, `false` to stop.
pub(crate) type Sink<'s> = dyn FnMut(&[Rv]) -> Result<bool> + 's;

pub(crate) struct Matcher<'a> {
    pub(crate) store: &'a Store,
    pub(crate) steps: &'a [Step],
    pub(crate) budget: &'a mut Budget,
    pub(crate) stats: &'a mut QueryStats,
    used: Vec<Slot>,
}

/// Break value for adjacency callbacks: `None` = stop requested.
type Brk = Option<GraphError>;

fn lift(r: Result<bool>) -> ControlFlow<Brk> {
    match r {
        Ok(true) => ControlFlow::Continue(()),
        Ok(false) => ControlFlow::Break(None),
        Err(e) => ControlFlow::Break(Some(e)),
    }
}

fn lower(c: ControlFlow<Brk>) -> Result<bool> {
    match c {
        ControlFlow::Continue(()) => Ok(true),
        ControlFlow::Break(None) => Ok(false),
        ControlFlow::Break(Some(e)) => Err(e),
    }
}

impl<'a> Matcher<'a> {
    pub(crate) fn new(store: &'a Store, steps: &'a [Step], budget: &'a mut Budget, stats: &'a mut QueryStats) -> Self {
        Matcher { store, steps, budget, stats, used: Vec::new() }
    }

    /// Run all steps starting from `row` (normally all-null).
    pub(crate) fn run(&mut self, row: &mut Vec<Rv>, sink: &mut Sink<'_>) -> Result<()> {
        self.budget.check()?;
        self.step(0, row, sink).map(|_| ())
    }

    fn node_ok(&self, slot: Slot, labels: &[u32]) -> bool {
        self.store.node_has_labels(slot, labels)
    }

    fn visit_node(
        &mut self,
        next: usize,
        slot: usize,
        s: Slot,
        labels: &[u32],
        row: &mut Vec<Rv>,
        sink: &mut Sink<'_>,
    ) -> Result<bool> {
        self.budget.visit(1)?;
        self.stats.nodes_scanned += 1;
        if !self.node_ok(s, labels) {
            return Ok(true);
        }
        row[slot] = Rv::Node(s);
        let r = self.step(next, row, sink);
        row[slot] = Rv::Null;
        r
    }

    fn step(&mut self, i: usize, row: &mut Vec<Rv>, sink: &mut Sink<'_>) -> Result<bool> {
        let steps: &'a [Step] = self.steps;
        let Some(step) = steps.get(i) else {
            return sink(row);
        };
        let store: &'a Store = self.store;
        match step {
            Step::Filter(e) => {
                if eval_predicate(e, &Ctx::new(store, row))? {
                    self.step(i + 1, row, sink)
                } else {
                    Ok(true)
                }
            }
            Step::NodeScan { slot, access, labels } => {
                let slot = *slot;
                match access {
                    NodeAccess::Bound => match row[slot] {
                        Rv::Node(s) if self.node_ok(s, labels) => self.step(i + 1, row, sink),
                        _ => Ok(true),
                    },
                    NodeAccess::All => {
                        for s in store.node_slots_iter() {
                            if !self.visit_node(i + 1, slot, s, labels, row, sink)? {
                                return Ok(false);
                            }
                        }
                        Ok(true)
                    }
                    NodeAccess::Label(l) => {
                        self.stats.index_lookups += 1;
                        if let Some(members) = store.label_members(*l) {
                            for &s in members {
                                if !self.visit_node(i + 1, slot, s, labels, row, sink)? {
                                    return Ok(false);
                                }
                            }
                        }
                        Ok(true)
                    }
                    NodeAccess::Slots(slots) => {
                        self.stats.index_lookups += 1;
                        for &s in slots {
                            if !self.visit_node(i + 1, slot, s, labels, row, sink)? {
                                return Ok(false);
                            }
                        }
                        Ok(true)
                    }
                }
            }
            Step::EdgeScan {
                slot,
                access,
                types,
                left,
                right,
                dir,
                left_labels,
                right_labels,
                left_bound,
                right_bound,
            } => {
                let candidates: Box<dyn Iterator<Item = Slot>> = match access {
                    EdgeAccess::Types(ts) => {
                        self.stats.index_lookups += 1;
                        let mut all: Vec<Slot> =
                            ts.iter().filter_map(|t| store.type_members(*t)).flat_map(|m| m.iter().copied()).collect();
                        all.sort_unstable();
                        Box::new(all.into_iter())
                    }
                    EdgeAccess::Slots(v) => {
                        self.stats.index_lookups += 1;
                        Box::new(v.clone().into_iter())
                    }
                };
                for e in candidates {
                    self.budget.visit(1)?;
                    self.stats.edges_traversed += 1;
                    let Some(rec) = store.edge(e) else { continue };
                    if types.as_ref().is_some_and(|ts| ts.binary_search(&rec.ty).is_err()) || self.used.contains(&e) {
                        continue;
                    }
                    let mut orientations: Vec<(Slot, Slot)> = Vec::with_capacity(2);
                    let forward = (rec.src, rec.tgt);
                    let backward = (rec.tgt, rec.src);
                    match (dir, rec.directed) {
                        (Direction::Outgoing, true) => orientations.push(forward),
                        (Direction::Incoming, true) => orientations.push(backward),
                        _ => {
                            orientations.push(forward);
                            if rec.src != rec.tgt {
                                orientations.push(backward);
                            }
                        }
                    }
                    for (l, r) in orientations {
                        if *left_bound && row[*left] != Rv::Node(l) || *right_bound && row[*right] != Rv::Node(r) {
                            continue;
                        }
                        if !self.node_ok(l, left_labels) || !self.node_ok(r, right_labels) {
                            continue;
                        }
                        // A node variable used on both sides must bind consistently.
                        if left == right && l != r {
                            continue;
                        }
                        let (old_l, old_r) = (row[*left].clone(), row[*right].clone());
                        row[*left] = Rv::Node(l);
                        row[*right] = Rv::Node(r);
                        row[*slot] = Rv::Edge(e);
                        self.used.push(e);
                        let res = self.step(i + 1, row, sink);
                        self.used.pop();
                        row[*slot] = Rv::Null;
                        row[*left] = old_l;
                        row[*right] = old_r;
                        if !res? {
                            return Ok(false);
                        }
                    }
                }
                Ok(true)
            }
            Step::Expand { from, edge, to, dir, types, to_labels, to_bound } => {
                let Rv::Node(src) = row[*from] else { return Ok(true) };
                let (edge, to, to_bound) = (*edge, *to, *to_bound);
                let flow = store.for_each_adjacent(src, *dir, types.as_deref(), |e, nbr| {
                    if let Err(err) = self.budget.visit(1) {
                        return ControlFlow::Break(Some(err));
                    }
                    self.stats.edges_traversed += 1;
                    if self.used.contains(&e) {
                        return ControlFlow::Continue(());
                    }
                    if to_bound {
                        if row[to] != Rv::Node(nbr) {
                            return ControlFlow::Continue(());
                        }
                    } else if !self.node_ok(nbr, to_labels) {
                        return ControlFlow::Continue(());
                    }
                    if !to_bound {
                        row[to] = Rv::Node(nbr);
                    }
                    row[edge] = Rv::Edge(e);
                    self.used.push(e);
                    let r = self.step(i + 1, row, sink);
                    self.used.pop();
                    row[edge] = Rv::Null;
                    if !to_bound {
                        row[to] = Rv::Null;
                    }
                    lift(r)
                });
                lower(flow)
            }
            Step::ExpandVar { from, edges, to, dir, types, to_labels, to_bound, min, max, reversed, edge_props } => {
                let Rv::Node(src) = row[*from] else { return Ok(true) };
                let spec = VarSpec {
                    next: i + 1,
                    edges: *edges,
                    to: *to,
                    dir: *dir,
                    types: types.as_deref(),
                    to_labels,
                    to_bound: *to_bound,
                    min: *min,
                    max: *max,
                    reversed: *reversed,
                    edge_props,
                };
                let mut path = Vec::new();
                self.var_expand(&spec, src, &mut path, row, sink)
            }
            Step::Shortest { from, edges, to, dir, types, to_labels, to_bound, min, max, reversed } => {
                let Rv::Node(src) = row[*from] else { return Ok(true) };
                let target = if *to_bound {
                    match row[*to] {
                        Rv::Node(t) => Some(t),
                        _ => return Ok(true),
                    }
                } else {
                    None
                };
                let found = self.bfs(src, target, *dir, types.as_deref(), *max)?;
                for (node, mut path) in found {
                    if (path.len() as u32) < *min || (!*to_bound && !self.node_ok(node, to_labels)) {
                        continue;
                    }
                    if *reversed {
                        path.reverse();
                    }
                    if !*to_bound {
                        row[*to] = Rv::Node(node);
                    }
                    row[*edges] = Rv::List(path.iter().map(|e| Rv::Edge(*e)).collect());
                    let len = self.used.len();
                    self.used.extend_from_slice(&path);
                    let r = self.step(i + 1, row, sink);
                    self.used.truncate(len);
                    row[*edges] = Rv::Null;
                    if !*to_bound {
                        row[*to] = Rv::Null;
                    }
                    if !r? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Step::BindPath { slot, start, rels } => {
                let Rv::Node(s) = row[*start] else { return Ok(true) };
                let mut edges = Vec::new();
                for (r, is_list) in rels {
                    match (&row[*r], is_list) {
                        (Rv::Edge(e), false) => edges.push(*e),
                        (Rv::List(items), true) => {
                            edges
                                .extend(items.iter().filter_map(|v| if let Rv::Edge(e) = v { Some(*e) } else { None }));
                        }
                        _ => return Ok(true),
                    }
                }
                row[*slot] = Rv::Path(Box::new(walk_path(store, s, &edges)));
                let r = self.step(i + 1, row, sink);
                row[*slot] = Rv::Null;
                r
            }
        }
    }

    fn var_expand(
        &mut self,
        spec: &VarSpec<'_>,
        cur: Slot,
        path: &mut Vec<Slot>,
        row: &mut Vec<Rv>,
        sink: &mut Sink<'_>,
    ) -> Result<bool> {
        let depth = path.len() as u32;
        if depth >= spec.min {
            let target_ok =
                if spec.to_bound { row[spec.to] == Rv::Node(cur) } else { self.node_ok(cur, spec.to_labels) };
            if target_ok {
                let mut edges = path.clone();
                if spec.reversed {
                    edges.reverse();
                }
                row[spec.edges] = Rv::List(edges.into_iter().map(Rv::Edge).collect());
                if !spec.to_bound {
                    row[spec.to] = Rv::Node(cur);
                }
                let r = self.step(spec.next, row, sink);
                row[spec.edges] = Rv::Null;
                if !spec.to_bound {
                    row[spec.to] = Rv::Null;
                }
                if !r? {
                    return Ok(false);
                }
            }
        }
        if depth >= spec.max {
            return Ok(true);
        }
        let store = self.store;
        let flow = store.for_each_adjacent(cur, spec.dir, spec.types, |e, nbr| {
            if let Err(err) = self.budget.visit(1) {
                return ControlFlow::Break(Some(err));
            }
            self.stats.edges_traversed += 1;
            if self.used.contains(&e) {
                return ControlFlow::Continue(());
            }
            if !spec.edge_props.is_empty() {
                let rec = store.edge(e).expect("adjacent edge exists");
                let ok = spec.edge_props.iter().all(|(k, v)| {
                    let actual = rec.props.get(k).map_or(Rv::Null, Rv::from_value);
                    actual.query_eq(v) == Some(true)
                });
                if !ok {
                    return ControlFlow::Continue(());
                }
            }
            self.used.push(e);
            path.push(e);
            let r = self.var_expand(spec, nbr, path, row, sink);
            path.pop();
            self.used.pop();
            lift(r)
        });
        lower(flow)
    }

    /// Breadth-first search from `src`. Returns (node, edge path) pairs in
    /// BFS order: either just the path to `target`, or one shortest path to
    /// every reachable node (including `src` itself with an empty path).
    fn bfs(
        &mut self,
        src: Slot,
        target: Option<Slot>,
        dir: Direction,
        types: Option<&[u32]>,
        max: u32,
    ) -> Result<Vec<(Slot, Vec<Slot>)>> {
        let store = self.store;
        let mut parent: HashMap<Slot, (Slot, Slot)> = HashMap::new();
        let mut order: Vec<(Slot, u32)> = vec![(src, 0)];
        let mut queue: VecDeque<(Slot, u32)> = VecDeque::from([(src, 0)]);
        let mut found_target = target == Some(src);
        while let Some((node, depth)) = queue.pop_front() {
            if found_target {
                break;
            }
            if depth >= max {
                continue;
            }
            let flow = store.for_each_adjacent(node, dir, types, |e, nbr| {
                if let Err(err) = self.budget.visit(1) {
                    return ControlFlow::Break(Some(err));
                }
                self.stats.edges_traversed += 1;
                if nbr == src || parent.contains_key(&nbr) || self.used.contains(&e) {
                    return ControlFlow::Continue(());
                }
                parent.insert(nbr, (node, e));
                order.push((nbr, depth + 1));
                queue.push_back((nbr, depth + 1));
                if target == Some(nbr) {
                    found_target = true;
                    return ControlFlow::Break(None);
                }
                ControlFlow::Continue(())
            });
            if let ControlFlow::Break(Some(e)) = flow {
                return Err(e);
            }
        }
        let path_to = |mut n: Slot| -> Vec<Slot> {
            let mut edges = Vec::new();
            while let Some(&(p, e)) = parent.get(&n) {
                edges.push(e);
                n = p;
            }
            edges.reverse();
            edges
        };
        Ok(match target {
            Some(t) if found_target => vec![(t, path_to(t))],
            Some(_) => Vec::new(),
            None => order.into_iter().map(|(n, _)| (n, path_to(n))).collect(),
        })
    }
}

struct VarSpec<'a> {
    next: usize,
    edges: usize,
    to: usize,
    dir: Direction,
    types: Option<&'a [u32]>,
    to_labels: &'a [u32],
    to_bound: bool,
    min: u32,
    max: u32,
    reversed: bool,
    edge_props: &'a [(String, Rv)],
}
