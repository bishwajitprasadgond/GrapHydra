//! Rule-based query planner for `MATCH ... WHERE`.
//!
//! For every pattern the planner considers each node (and each single-hop
//! relationship) as a starting point, estimates its cardinality from exact
//! index statistics, starts from the most selective one and expands along
//! adjacency lists in both directions. `WHERE` conjuncts are pushed down to
//! the earliest step at which all their variables are bound. Index lookups
//! only ever *narrow* candidate sets: the original predicate is still
//! evaluated, so results never depend on which access path was chosen.

use std::collections::BTreeMap;
use std::ops::Bound;

use crate::engine::expr::{CExpr, Compiler, Func, Scope};
use crate::engine::result::{PlanStep, QueryPlan};
use crate::engine::rv::Rv;
use crate::error::{GraphError, Result};
use crate::indexes::IndexKey;
use crate::language::ast::{BinOp, Expr, Pattern};
use crate::model::{IndexKind, IndexTarget};
use crate::store::{Direction, Slot, Store};
use crate::value::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VarKind {
    Node,
    Edge,
    EdgeList,
    Path,
}

#[derive(Debug, Clone)]
pub(crate) struct VarInfo {
    /// `None` for anonymous pattern elements.
    pub(crate) name: Option<String>,
    pub(crate) kind: VarKind,
}

/// Variables of a statement; their index is the row slot.
#[derive(Debug, Clone, Default)]
pub(crate) struct VarTable {
    pub(crate) vars: Vec<VarInfo>,
}

impl VarTable {
    pub(crate) fn lookup(&self, name: &str) -> Option<usize> {
        self.vars.iter().position(|v| v.name.as_deref() == Some(name))
    }

    pub(crate) fn kind_of(&self, name: &str) -> Option<VarKind> {
        self.lookup(name).map(|i| self.vars[i].kind)
    }

    pub(crate) fn add(&mut self, name: Option<&str>, kind: VarKind) -> Result<usize> {
        if let Some(n) = name
            && let Some(i) = self.lookup(n)
        {
            let existing = self.vars[i].kind;
            if existing == kind && kind == VarKind::Node {
                return Ok(i);
            }
            return Err(GraphError::semantic(if existing == kind {
                format!("variable `{n}` is bound more than once")
            } else {
                format!("variable `{n}` is used as both {existing:?} and {kind:?}")
            }));
        }
        self.vars.push(VarInfo { name: name.map(str::to_owned), kind });
        Ok(self.vars.len() - 1)
    }

    pub(crate) fn len(&self) -> usize {
        self.vars.len()
    }

    /// Named variables in declaration order (for `RETURN *`).
    pub(crate) fn named(&self) -> Vec<(usize, &str)> {
        self.vars.iter().enumerate().filter_map(|(i, v)| v.name.as_deref().map(|n| (i, n))).collect()
    }
}

impl Scope for VarTable {
    fn resolve(&self, name: &str) -> Option<CExpr> {
        self.lookup(name).map(CExpr::Var)
    }
}

#[derive(Debug, Clone)]
struct NodeElem {
    slot: usize,
    labels: Vec<String>,
}

#[derive(Debug, Clone)]
struct RelElem {
    slot: usize,
    types: Vec<String>,
    dir: Direction,
    /// `Some((min, max))` for variable-length relationships.
    range: Option<(u32, u32)>,
    /// Per-edge constant property constraints (variable-length only).
    edge_props: Vec<(String, Rv)>,
}

#[derive(Debug, Clone)]
struct BoundPattern {
    nodes: Vec<NodeElem>,
    rels: Vec<RelElem>,
    path_slot: Option<usize>,
    shortest: bool,
}

#[derive(Debug, Clone)]
pub(crate) enum NodeAccess {
    Bound,
    All,
    Label(u32),
    Slots(Vec<Slot>),
}

#[derive(Debug, Clone)]
pub(crate) enum EdgeAccess {
    Types(Vec<u32>),
    Slots(Vec<Slot>),
}

/// Edge-type filter: `None` = any type. Always sorted and de-duplicated.
pub(crate) type TypeFilter = Option<Vec<u32>>;

#[derive(Debug, Clone)]
pub(crate) enum Step {
    Filter(CExpr),
    NodeScan {
        slot: usize,
        access: NodeAccess,
        labels: Vec<u32>,
    },
    EdgeScan {
        slot: usize,
        access: EdgeAccess,
        types: TypeFilter,
        left: usize,
        right: usize,
        dir: Direction,
        left_labels: Vec<u32>,
        right_labels: Vec<u32>,
        left_bound: bool,
        right_bound: bool,
    },
    Expand {
        from: usize,
        edge: usize,
        to: usize,
        dir: Direction,
        types: TypeFilter,
        to_labels: Vec<u32>,
        to_bound: bool,
    },
    ExpandVar {
        from: usize,
        edges: usize,
        to: usize,
        dir: Direction,
        types: TypeFilter,
        to_labels: Vec<u32>,
        to_bound: bool,
        min: u32,
        max: u32,
        reversed: bool,
        edge_props: Vec<(String, Rv)>,
    },
    Shortest {
        from: usize,
        edges: usize,
        to: usize,
        dir: Direction,
        types: TypeFilter,
        to_labels: Vec<u32>,
        to_bound: bool,
        min: u32,
        max: u32,
        reversed: bool,
    },
    BindPath {
        slot: usize,
        start: usize,
        /// Relationship slots in pattern order; `true` = edge list.
        rels: Vec<(usize, bool)>,
    },
}

/// Executable plan for the MATCH part of a statement.
#[derive(Debug, Clone)]
pub(crate) struct MatchPlan {
    pub(crate) steps: Vec<Step>,
    /// True when the pattern can never match (unknown label, empty lookup...).
    pub(crate) impossible: bool,
    pub(crate) explain: QueryPlan,
}

/// Largest candidate set for a shortest-path target that is bound before the
/// search (one BFS per candidate); larger sets use a single exhaustive BFS.
const SHORTEST_TARGET_BIND_LIMIT: u64 = 16;

struct Conjunct {
    expr: CExpr,
    deps: Vec<usize>,
    placed: bool,
    text: String,
}

/// Bind pattern variables into the table (shared with CREATE clauses).
fn bind_patterns(
    patterns: &[Pattern],
    vars: &mut VarTable,
    compiler: &mut Compiler<'_>,
    max_depth: u32,
) -> Result<(Vec<BoundPattern>, Vec<CExpr>)> {
    let mut out = Vec::new();
    // Inline property constraints, compiled after all variables are known.
    let mut inline: Vec<(usize, String, Expr)> = Vec::new();
    for p in patterns {
        let mut bp = BoundPattern { nodes: Vec::new(), rels: Vec::new(), path_slot: None, shortest: p.shortest };
        let add_node = |n: &crate::language::ast::NodePattern,
                        vars: &mut VarTable,
                        inline: &mut Vec<(usize, String, Expr)>|
         -> Result<NodeElem> {
            let slot = vars.add(n.var.as_deref(), VarKind::Node)?;
            for (k, e) in &n.props {
                inline.push((slot, k.clone(), e.clone()));
            }
            Ok(NodeElem { slot, labels: n.labels.clone() })
        };
        bp.nodes.push(add_node(&p.start, vars, &mut inline)?);
        for (rel, node) in &p.steps {
            let kind = if rel.length.is_some() { VarKind::EdgeList } else { VarKind::Edge };
            let slot = vars.add(rel.var.as_deref(), kind)?;
            let range = match rel.length {
                None => None,
                Some((min, max)) => {
                    let Some(max) = max else {
                        return Err(GraphError::semantic(format!(
                            "variable-length relationship needs an upper bound (e.g. *{min}..{max_depth}); \
                             unbounded traversal is disabled"
                        )));
                    };
                    if min > max {
                        return Err(GraphError::semantic(format!("invalid path length *{min}..{max}")));
                    }
                    if max > max_depth {
                        return Err(GraphError::LimitExceeded(format!(
                            "path length {max} exceeds max_traversal_depth {max_depth}"
                        )));
                    }
                    Some((min, max))
                }
            };
            let mut edge_props = Vec::new();
            for (k, e) in &rel.props {
                if range.is_some() {
                    if !e.is_constant() {
                        return Err(GraphError::semantic(
                            "properties on variable-length relationships must be literals or parameters",
                        ));
                    }
                    let c = compiler.compile(e, vars)?;
                    edge_props.push((k.clone(), c.const_value().cloned().unwrap_or(Rv::Null)));
                } else {
                    inline.push((slot, k.clone(), e.clone()));
                }
            }
            bp.rels.push(RelElem { slot, types: rel.types.clone(), dir: rel.direction, range, edge_props });
            bp.nodes.push(add_node(node, vars, &mut inline)?);
        }
        if p.shortest {
            if bp.rels.len() != 1 {
                return Err(GraphError::semantic("shortestPath() needs exactly one relationship: (a)-[*..n]->(b)"));
            }
            let r = &mut bp.rels[0];
            match r.range {
                Some((min, _)) if min <= 1 => {}
                Some(_) => return Err(GraphError::semantic("shortestPath() supports a minimum length of 0 or 1")),
                None => r.range = Some((1, 1)),
            }
            vars.vars[r.slot].kind = VarKind::EdgeList;
            if !r.edge_props.is_empty() {
                return Err(GraphError::semantic("shortestPath() does not support relationship properties"));
            }
        }
        if let Some(pv) = &p.path_var {
            bp.path_slot = Some(vars.add(Some(pv), VarKind::Path)?);
        }
        out.push(bp);
    }
    let mut compiled = Vec::new();
    for (slot, key, e) in inline {
        let value = compiler.compile(&e, vars)?;
        let target = CExpr::Prop(Box::new(CExpr::Var(slot)), key);
        compiled.push(CExpr::Bin(BinOp::Eq, Box::new(target), Box::new(value)));
    }
    Ok((out, compiled))
}

fn split_and(e: &Expr, out: &mut Vec<Expr>) {
    match e {
        Expr::Binary(BinOp::And, a, b) => {
            split_and(a, out);
            split_and(b, out);
        }
        other => out.push(other.clone()),
    }
}

fn describe_expr(e: &CExpr, vars: &VarTable) -> String {
    let var = |i: usize| vars.vars[i].name.clone().unwrap_or_else(|| format!("_{i}"));
    match e {
        CExpr::Const(v) => match v.to_value() {
            Some(v) => v.to_string(),
            None => "<entity>".into(),
        },
        CExpr::Var(i) => var(*i),
        CExpr::Prop(t, k) => format!("{}.{k}", describe_expr(t, vars)),
        CExpr::Bin(op, a, b) => format!("{} {} {}", describe_expr(a, vars), op_text(*op), describe_expr(b, vars)),
        CExpr::Not(a) => format!("NOT {}", describe_expr(a, vars)),
        CExpr::IsNull(a, neg) => format!("{} IS {}NULL", describe_expr(a, vars), if *neg { "NOT " } else { "" }),
        CExpr::Call(f, args) => format!(
            "{}({})",
            format!("{f:?}").to_lowercase(),
            args.iter().map(|a| describe_expr(a, vars)).collect::<Vec<_>>().join(", ")
        ),
        _ => "<expr>".into(),
    }
}

fn op_text(op: BinOp) -> &'static str {
    match op {
        BinOp::Or => "OR",
        BinOp::Xor => "XOR",
        BinOp::And => "AND",
        BinOp::Eq => "=",
        BinOp::Neq => "<>",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::In => "IN",
        BinOp::Contains => "CONTAINS",
        BinOp::StartsWith => "STARTS WITH",
        BinOp::EndsWith => "ENDS WITH",
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
        BinOp::Pow => "^",
    }
}

/// Predicate shapes usable for index access.
enum Sargable<'a> {
    Eq(&'a str, &'a Rv),
    In(&'a str, &'a [Rv]),
    Range(&'a str, BinOp, &'a Rv),
    Prefix(&'a str, &'a str),
    Text(&'a str, &'a str),
}

fn prop_of(x: &CExpr, slot: usize) -> Option<&str> {
    match x {
        CExpr::Prop(t, k) if matches!(**t, CExpr::Var(s) if s == slot) => Some(k.as_str()),
        CExpr::Call(Func::Id, args) if matches!(args.as_slice(), [CExpr::Var(s)] if *s == slot) => Some("id"),
        _ => None,
    }
}

/// Recognise `slot.key <op> constant` forms.
fn sargable(e: &CExpr, slot: usize) -> Option<Sargable<'_>> {
    match e {
        CExpr::Bin(op, a, b) => {
            let flipped = |op: BinOp| match op {
                BinOp::Lt => BinOp::Gt,
                BinOp::Le => BinOp::Ge,
                BinOp::Gt => BinOp::Lt,
                BinOp::Ge => BinOp::Le,
                o => o,
            };
            let (key, c, op) = match (prop_of(a, slot), b.const_value(), prop_of(b, slot), a.const_value()) {
                (Some(k), Some(c), _, _) => (k, c, *op),
                (_, _, Some(k), Some(c)) if *op != BinOp::In && *op != BinOp::StartsWith => (k, c, flipped(*op)),
                _ => return None,
            };
            match (op, c) {
                (BinOp::Eq, c) => Some(Sargable::Eq(key, c)),
                (BinOp::In, Rv::List(items)) => Some(Sargable::In(key, items)),
                (BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge, c) => Some(Sargable::Range(key, op, c)),
                (BinOp::StartsWith, Rv::Str(p)) => Some(Sargable::Prefix(key, p)),
                _ => None,
            }
        }
        CExpr::Call(Func::TextMatch, args) => match args.as_slice() {
            [target, CExpr::Const(Rv::Str(q))] => prop_of(target, slot).map(|k| Sargable::Text(k, q.as_str())),
            _ => None,
        },
        _ => None,
    }
}

fn index_key(v: &Rv) -> Option<IndexKey> {
    v.to_value().and_then(|v| IndexKey::from_value(&v))
}

struct Candidate<A> {
    access: A,
    estimate: u64,
    description: String,
}

pub(crate) struct Planner<'a> {
    pub(crate) store: &'a Store,
    pub(crate) disable_indexes: bool,
}

impl Planner<'_> {
    /// Access paths for a record set using `id` and property indexes.
    fn index_candidates(
        &self,
        target: IndexTarget,
        slot: usize,
        name: &str,
        conjuncts: &[Conjunct],
        notes: &mut Vec<String>,
    ) -> Vec<Candidate<Vec<Slot>>> {
        let store = self.store;
        let mut out = Vec::new();
        if self.disable_indexes {
            return out;
        }
        let id_lookup = |id: &Rv| -> Option<Slot> {
            let Rv::Str(s) = id else { return None };
            match target {
                IndexTarget::Node => store.node_slot(s),
                IndexTarget::Edge => store.edge_slot(s),
            }
        };
        // Range bounds per property: (lower, upper)
        let mut ranges: BTreeMap<&str, (Bound<IndexKey>, Bound<IndexKey>, Vec<String>)> = BTreeMap::new();
        for c in conjuncts {
            let Some(s) = sargable(&c.expr, slot) else { continue };
            match s {
                Sargable::Eq("id", v) => {
                    let slots: Vec<Slot> = id_lookup(v).into_iter().collect();
                    out.push(Candidate {
                        estimate: slots.len() as u64,
                        access: slots,
                        // `c.text` is the whole condition, e.g. `h.id = "host-001"`.
                        description: format!("IdLookup {}", c.text),
                    });
                }
                Sargable::In("id", items) => {
                    let mut slots: Vec<Slot> = items.iter().filter_map(id_lookup).collect();
                    slots.sort_unstable();
                    slots.dedup();
                    out.push(Candidate {
                        estimate: slots.len() as u64,
                        access: slots,
                        description: format!("IdLookup {name}.id IN [{} ids]", items.len()),
                    });
                }
                Sargable::Eq(key, v) => {
                    let idx = store.find_index(target, key, &[IndexKind::Hash, IndexKind::Sorted]);
                    match (idx, v) {
                        (_, Rv::Null) => out.push(Candidate {
                            access: Vec::new(),
                            estimate: 0,
                            description: format!("Empty ({name}.{key} = null is never true)"),
                        }),
                        (Some(idx), v) => {
                            if let Some(k) = index_key(v) {
                                let slots = idx.lookup_eq(&k).unwrap_or_default();
                                out.push(Candidate {
                                    estimate: slots.len() as u64,
                                    access: slots,
                                    description: format!("IndexSeek {} ({name}.{key} = ...)", idx.def),
                                });
                            }
                        }
                        (None, _) => notes.push(format!(
                            "no index on {target}s({key}); `CREATE INDEX ON {}S({key})` would avoid a scan for {name}",
                            target.to_string().to_uppercase()
                        )),
                    }
                }
                Sargable::In(key, items) => {
                    let idx = store.find_index(target, key, &[IndexKind::Hash, IndexKind::Sorted]);
                    let all_scalar = items.iter().all(|i| i.is_null() || index_key(i).is_some());
                    if let (Some(idx), true) = (idx, all_scalar) {
                        let mut slots: Vec<Slot> = Vec::new();
                        for k in items.iter().filter_map(index_key) {
                            slots.extend(idx.lookup_eq(&k).unwrap_or_default());
                        }
                        slots.sort_unstable();
                        slots.dedup();
                        out.push(Candidate {
                            estimate: slots.len() as u64,
                            access: slots,
                            description: format!("IndexSeek {} ({name}.{key} IN [{} values])", idx.def, items.len()),
                        });
                    }
                }
                Sargable::Range(key, op, v) => {
                    if key == "id" {
                        continue;
                    }
                    let Some(k) = index_key(v) else {
                        if v.is_null() {
                            out.push(Candidate {
                                access: Vec::new(),
                                estimate: 0,
                                description: format!("Empty (comparison of {name}.{key} with null)"),
                            });
                        }
                        continue;
                    };
                    let entry = ranges.entry(key).or_insert((Bound::Unbounded, Bound::Unbounded, Vec::new()));
                    entry.2.push(c.text.clone());
                    let tighter_lower = |cur: &Bound<IndexKey>, new: Bound<IndexKey>| match (cur, &new) {
                        (Bound::Unbounded, _) => new,
                        (Bound::Included(a) | Bound::Excluded(a), Bound::Included(b) | Bound::Excluded(b)) => {
                            if !a.same_band(b) || b > a || (b == a && matches!(new, Bound::Excluded(_))) {
                                new
                            } else {
                                cur.clone()
                            }
                        }
                        (_, Bound::Unbounded) => cur.clone(),
                    };
                    let tighter_upper = |cur: &Bound<IndexKey>, new: Bound<IndexKey>| match (cur, &new) {
                        (Bound::Unbounded, _) => new,
                        (Bound::Included(a) | Bound::Excluded(a), Bound::Included(b) | Bound::Excluded(b)) => {
                            if !a.same_band(b) || b < a || (b == a && matches!(new, Bound::Excluded(_))) {
                                new
                            } else {
                                cur.clone()
                            }
                        }
                        (_, Bound::Unbounded) => cur.clone(),
                    };
                    match op {
                        BinOp::Gt => entry.0 = tighter_lower(&entry.0, Bound::Excluded(k)),
                        BinOp::Ge => entry.0 = tighter_lower(&entry.0, Bound::Included(k)),
                        BinOp::Lt => entry.1 = tighter_upper(&entry.1, Bound::Excluded(k)),
                        _ => entry.1 = tighter_upper(&entry.1, Bound::Included(k)),
                    }
                }
                Sargable::Prefix(key, prefix) => {
                    if let Some(idx) = store.find_index(target, key, &[IndexKind::Sorted]) {
                        let slots = idx.lookup_prefix(prefix).unwrap_or_default();
                        out.push(Candidate {
                            estimate: slots.len() as u64,
                            access: slots,
                            description: format!("IndexPrefixScan {} ({name}.{key} STARTS WITH ...)", idx.def),
                        });
                    }
                }
                Sargable::Text(key, q) => {
                    if let Some(idx) = store.find_index(target, key, &[IndexKind::Text]) {
                        let slots = idx.text().map(|t| t.lookup_all(q)).unwrap_or_default();
                        out.push(Candidate {
                            estimate: slots.len() as u64,
                            access: slots,
                            description: format!("TextIndexSeek {} (text_match({name}.{key}, ...))", idx.def),
                        });
                    } else {
                        notes.push(format!("no text index on {target}s({key}); text_match on {name} scans records"));
                    }
                }
            }
        }
        for (key, (lo, hi, texts)) in ranges {
            if let Some(idx) = store.find_index(target, key, &[IndexKind::Sorted]) {
                // Mixed-band bounds can never both hold; keep the lower band only.
                let hi = match (&lo, &hi) {
                    (Bound::Included(a) | Bound::Excluded(a), Bound::Included(b) | Bound::Excluded(b))
                        if !a.same_band(b) =>
                    {
                        Bound::Unbounded
                    }
                    _ => hi,
                };
                let slots = idx.lookup_range(lo.as_ref(), hi.as_ref()).unwrap_or_default();
                out.push(Candidate {
                    estimate: slots.len() as u64,
                    access: slots,
                    description: format!("IndexRangeScan {} ({})", idx.def, texts.join(" AND ")),
                });
            } else if store.find_index(target, key, &[IndexKind::Hash]).is_some() {
                notes.push(format!("hash index on {target}s({key}) cannot serve range predicates; use a SORTED index"));
            }
        }
        out
    }
}

/// Plan the MATCH patterns + WHERE of a query. Pattern variables are added
/// to `vars`.
pub(crate) fn plan_match(
    planner: &Planner<'_>,
    patterns: &[Pattern],
    where_: Option<&Expr>,
    vars: &mut VarTable,
    params: &BTreeMap<String, Value>,
) -> Result<MatchPlan> {
    let store = planner.store;
    let mut compiler = Compiler { params, aggs: None };
    let (bound, inline) = bind_patterns(patterns, vars, &mut compiler, store.limits.max_traversal_depth)?;

    let mut conjuncts: Vec<Conjunct> = Vec::new();
    for c in inline {
        let mut deps = Vec::new();
        c.deps(&mut deps);
        let text = describe_expr(&c, vars);
        conjuncts.push(Conjunct { expr: c, deps, placed: false, text });
    }
    if let Some(w) = where_ {
        if w.contains_aggregate() {
            return Err(GraphError::semantic("aggregate functions are not allowed in WHERE"));
        }
        let mut parts = Vec::new();
        split_and(w, &mut parts);
        for p in parts {
            let c = compiler.compile(&p, vars)?;
            let mut deps = Vec::new();
            c.deps(&mut deps);
            let text = describe_expr(&c, vars);
            conjuncts.push(Conjunct { expr: c, deps, placed: false, text });
        }
    }

    let mut explain = QueryPlan::default();
    let mut steps: Vec<Step> = Vec::new();
    let mut bound_slots: Vec<bool> = vec![false; vars.len()];
    let mut impossible = false;

    let var_name = |i: usize| vars.vars[i].name.clone().unwrap_or_else(|| "_".into());
    let node_desc = |n: &NodeElem| {
        let labels: String = n.labels.iter().map(|l| format!(":{l}")).collect();
        format!("({}{labels})", var_name(n.slot))
    };

    // Constant predicates first.
    place_filters(&mut conjuncts, &bound_slots, &mut steps, &mut explain);

    let resolve_labels = |labels: &[String], impossible: &mut bool, notes: &mut Vec<String>| -> Vec<u32> {
        let mut ids = Vec::new();
        for l in labels {
            match store.labels.get(l) {
                Some(id) => ids.push(id),
                None => {
                    *impossible = true;
                    notes.push(format!("label :{l} does not exist; the pattern cannot match"));
                }
            }
        }
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    let resolve_types = |types: &[String], impossible: &mut bool, notes: &mut Vec<String>| -> TypeFilter {
        if types.is_empty() {
            return None;
        }
        let mut ids: Vec<u32> = types.iter().filter_map(|t| store.types.get(t)).collect();
        if ids.is_empty() {
            *impossible = true;
            notes.push(format!("relationship type(s) {} do not exist; the pattern cannot match", types.join("|")));
        }
        ids.sort_unstable();
        ids.dedup();
        Some(ids)
    };

    let mut remaining: Vec<usize> = (0..bound.len()).collect();
    while !remaining.is_empty() {
        // Choose the pattern whose best start is cheapest (bound vars cost 0).
        let mut best: Option<(usize, u64, StartChoice, String, Vec<String>)> = None;
        for (ri, &pi) in remaining.iter().enumerate() {
            let p = &bound[pi];
            let mut notes = Vec::new();
            let (choice, est, desc) = choose_start(planner, p, &conjuncts, &bound_slots, vars, &mut notes);
            if best.as_ref().is_none_or(|b| est < b.1) {
                best = Some((ri, est, choice, desc, notes));
            }
        }
        let (ri, est, choice, desc, notes) = best.expect("remaining is non-empty");
        let pi = remaining.remove(ri);
        let p = &bound[pi];
        explain.notes.extend(notes);

        let labels_of =
            |n: &NodeElem, impossible: &mut bool, notes: &mut Vec<String>| resolve_labels(&n.labels, impossible, notes);

        // Starting step.
        let (mut left_i, mut right_i) = match choice {
            StartChoice::Node(i, access) => {
                let n = &p.nodes[i];
                let labels = labels_of(n, &mut impossible, &mut explain.notes);
                let estimate = Some(est);
                let detail = format!("{} via {desc}", node_desc(n));
                explain.steps.push(PlanStep { operator: "NodeScan".into(), detail, estimated_rows: estimate });
                steps.push(Step::NodeScan { slot: n.slot, access, labels });
                bound_slots[n.slot] = true;
                (i, i)
            }
            StartChoice::Rel(j, access) => {
                let r = &p.rels[j];
                let (ln, rn) = (&p.nodes[j], &p.nodes[j + 1]);
                let types = resolve_types(&r.types, &mut impossible, &mut explain.notes);
                let left_labels = labels_of(ln, &mut impossible, &mut explain.notes);
                let right_labels = labels_of(rn, &mut impossible, &mut explain.notes);
                explain.steps.push(PlanStep {
                    operator: "EdgeScan".into(),
                    detail: format!("{}-[{}]-{} via {desc}", node_desc(ln), var_name(r.slot), node_desc(rn)),
                    estimated_rows: Some(est),
                });
                steps.push(Step::EdgeScan {
                    slot: r.slot,
                    access,
                    types,
                    left: ln.slot,
                    right: rn.slot,
                    dir: r.dir,
                    left_labels,
                    right_labels,
                    left_bound: bound_slots[ln.slot],
                    right_bound: bound_slots[rn.slot],
                });
                bound_slots[r.slot] = true;
                bound_slots[ln.slot] = true;
                bound_slots[rn.slot] = true;
                (j, j + 1)
            }
        };
        place_filters(&mut conjuncts, &bound_slots, &mut steps, &mut explain);

        // For shortestPath, bind a selective target up front so the BFS can
        // stop as soon as it reaches it instead of exploring everything.
        if p.shortest {
            let other = if left_i == 0 { p.nodes.len() - 1 } else { 0 };
            let n = &p.nodes[other];
            if !bound_slots[n.slot] {
                let mut notes = Vec::new();
                let best = planner
                    .index_candidates(IndexTarget::Node, n.slot, &var_name(n.slot), &conjuncts, &mut notes)
                    .into_iter()
                    .min_by_key(|c| c.estimate);
                if let Some(c) = best.filter(|c| c.estimate <= SHORTEST_TARGET_BIND_LIMIT) {
                    let labels = resolve_labels(&n.labels, &mut impossible, &mut explain.notes);
                    explain.steps.push(PlanStep {
                        operator: "NodeScan".into(),
                        detail: format!("{} via {} (shortest-path target)", node_desc(n), c.description),
                        estimated_rows: Some(c.estimate),
                    });
                    steps.push(Step::NodeScan { slot: n.slot, access: NodeAccess::Slots(c.access), labels });
                    bound_slots[n.slot] = true;
                    place_filters(&mut conjuncts, &bound_slots, &mut steps, &mut explain);
                }
            }
        }

        // Expand to the right, then to the left.
        while right_i + 1 < p.nodes.len() {
            let r = &p.rels[right_i];
            let to = &p.nodes[right_i + 1];
            push_expand(
                &mut ExpandCtx {
                    steps: &mut steps,
                    explain: &mut explain,
                    bound_slots: &mut bound_slots,
                    impossible: &mut impossible,
                    store,
                    vars,
                    shortest: p.shortest,
                },
                &p.nodes[right_i],
                r,
                to,
                false,
                &resolve_labels,
                &resolve_types,
            );
            right_i += 1;
            place_filters(&mut conjuncts, &bound_slots, &mut steps, &mut explain);
        }
        while left_i > 0 {
            let r = &p.rels[left_i - 1];
            let to = &p.nodes[left_i - 1];
            push_expand(
                &mut ExpandCtx {
                    steps: &mut steps,
                    explain: &mut explain,
                    bound_slots: &mut bound_slots,
                    impossible: &mut impossible,
                    store,
                    vars,
                    shortest: p.shortest,
                },
                &p.nodes[left_i],
                r,
                to,
                true,
                &resolve_labels,
                &resolve_types,
            );
            left_i -= 1;
            place_filters(&mut conjuncts, &bound_slots, &mut steps, &mut explain);
        }
        if let Some(ps) = p.path_slot {
            steps.push(Step::BindPath {
                slot: ps,
                start: p.nodes[0].slot,
                rels: p.rels.iter().map(|r| (r.slot, r.range.is_some() || p.shortest)).collect(),
            });
            explain.steps.push(PlanStep { operator: "BindPath".into(), detail: var_name(ps), estimated_rows: None });
            bound_slots[ps] = true;
            place_filters(&mut conjuncts, &bound_slots, &mut steps, &mut explain);
        }
    }
    // Anything left (should not happen: all deps are pattern vars).
    for c in conjuncts.iter_mut().filter(|c| !c.placed) {
        c.placed = true;
        explain.steps.push(PlanStep { operator: "Filter".into(), detail: c.text.clone(), estimated_rows: None });
        steps.push(Step::Filter(c.expr.clone()));
    }
    if impossible {
        explain.notes.push("plan short-circuits: no rows can match".into());
    }
    Ok(MatchPlan { steps, impossible, explain })
}

enum StartChoice {
    Node(usize, NodeAccess),
    Rel(usize, EdgeAccess),
}

fn choose_start(
    planner: &Planner<'_>,
    p: &BoundPattern,
    conjuncts: &[Conjunct],
    bound_slots: &[bool],
    vars: &VarTable,
    notes: &mut Vec<String>,
) -> (StartChoice, u64, String) {
    let store = planner.store;
    let var_name = |i: usize| vars.vars[i].name.clone().unwrap_or_else(|| "_".into());
    let mut best: Option<(StartChoice, u64, String)> = None;
    let mut considered: Vec<String> = Vec::new();
    let mut consider = |choice: StartChoice, est: u64, desc: String, considered: &mut Vec<String>| {
        considered.push(format!("{desc} ~{est}"));
        if best.as_ref().is_none_or(|b| est < b.1) {
            best = Some((choice, est, desc));
        }
    };
    // Shortest-path patterns may only start at an endpoint.
    let node_positions: Vec<usize> = if p.shortest { vec![0, p.nodes.len() - 1] } else { (0..p.nodes.len()).collect() };
    for i in node_positions {
        let n = &p.nodes[i];
        if bound_slots[n.slot] {
            consider(
                StartChoice::Node(i, NodeAccess::Bound),
                0,
                format!("bound {}", var_name(n.slot)),
                &mut considered,
            );
            continue;
        }
        let name = var_name(n.slot);
        for c in planner.index_candidates(IndexTarget::Node, n.slot, &name, conjuncts, notes) {
            consider(StartChoice::Node(i, NodeAccess::Slots(c.access)), c.estimate, c.description, &mut considered);
        }
        if !planner.disable_indexes {
            for l in &n.labels {
                match store.labels.get(l) {
                    Some(id) => consider(
                        StartChoice::Node(i, NodeAccess::Label(id)),
                        store.label_count(id) as u64,
                        format!("LabelScan :{l}"),
                        &mut considered,
                    ),
                    None => consider(
                        StartChoice::Node(i, NodeAccess::Slots(Vec::new())),
                        0,
                        format!("Empty (label :{l} does not exist)"),
                        &mut considered,
                    ),
                }
            }
        }
        consider(
            StartChoice::Node(i, NodeAccess::All),
            store.node_count() as u64,
            "AllNodesScan".into(),
            &mut considered,
        );
    }
    if !p.shortest {
        for (j, r) in p.rels.iter().enumerate() {
            if r.range.is_some() {
                continue;
            }
            let both = u64::from(r.dir == Direction::Both) + 1;
            let name = var_name(r.slot);
            for c in planner.index_candidates(IndexTarget::Edge, r.slot, &name, conjuncts, notes) {
                consider(
                    StartChoice::Rel(j, EdgeAccess::Slots(c.access)),
                    c.estimate * both,
                    c.description,
                    &mut considered,
                );
            }
            if !r.types.is_empty() && !planner.disable_indexes {
                let ids: Vec<u32> = r.types.iter().filter_map(|t| store.types.get(t)).collect();
                let est: u64 = ids.iter().map(|t| store.type_count(*t) as u64).sum();
                consider(
                    StartChoice::Rel(j, EdgeAccess::Types(ids)),
                    est * both,
                    format!("EdgeTypeScan :{}", r.types.join("|")),
                    &mut considered,
                );
            }
        }
    }
    let (choice, est, desc) = best.expect("at least AllNodesScan is considered");
    if considered.len() > 1 {
        notes.push(format!("start chosen: {desc}; considered: {}", considered.join(", ")));
    }
    (choice, est, desc)
}

struct ExpandCtx<'a, 'b> {
    steps: &'a mut Vec<Step>,
    explain: &'a mut QueryPlan,
    bound_slots: &'a mut Vec<bool>,
    impossible: &'a mut bool,
    store: &'b Store,
    vars: &'b VarTable,
    shortest: bool,
}

#[allow(clippy::too_many_arguments)]
fn push_expand(
    cx: &mut ExpandCtx<'_, '_>,
    from: &NodeElem,
    r: &RelElem,
    to: &NodeElem,
    reversed: bool,
    resolve_labels: &dyn Fn(&[String], &mut bool, &mut Vec<String>) -> Vec<u32>,
    resolve_types: &dyn Fn(&[String], &mut bool, &mut Vec<String>) -> TypeFilter,
) {
    let _ = cx.store;
    let dir = if reversed { r.dir.reverse() } else { r.dir };
    let types = resolve_types(&r.types, cx.impossible, &mut cx.explain.notes);
    let to_labels = resolve_labels(&to.labels, cx.impossible, &mut cx.explain.notes);
    let to_bound = cx.bound_slots[to.slot];
    let name = |i: usize| cx.vars.vars[i].name.clone().unwrap_or_else(|| "_".into());
    let arrow = match dir {
        Direction::Outgoing => "->",
        Direction::Incoming => "<-",
        Direction::Both => "--",
    };
    let types_text = if r.types.is_empty() { String::new() } else { format!(":{}", r.types.join("|")) };
    match (cx.shortest, r.range) {
        (true, range) => {
            let (min, max) = range.unwrap_or((1, 1));
            cx.explain.steps.push(PlanStep {
                operator: "ShortestPath".into(),
                detail: format!(
                    "({}) {arrow}[{types_text}*{min}..{max}] ({}){}",
                    name(from.slot),
                    name(to.slot),
                    if to_bound { " [bound target]" } else { "" }
                ),
                estimated_rows: None,
            });
            cx.steps.push(Step::Shortest {
                from: from.slot,
                edges: r.slot,
                to: to.slot,
                dir,
                types,
                to_labels,
                to_bound,
                min,
                max,
                reversed,
            });
        }
        (false, Some((min, max))) => {
            cx.explain.steps.push(PlanStep {
                operator: "VarExpand".into(),
                detail: format!(
                    "({}) {arrow}[{}{types_text}*{min}..{max}] ({})",
                    name(from.slot),
                    name(r.slot),
                    name(to.slot)
                ),
                estimated_rows: None,
            });
            cx.steps.push(Step::ExpandVar {
                from: from.slot,
                edges: r.slot,
                to: to.slot,
                dir,
                types,
                to_labels,
                to_bound,
                min,
                max,
                reversed,
                edge_props: r.edge_props.clone(),
            });
        }
        (false, None) => {
            cx.explain.steps.push(PlanStep {
                operator: if to_bound { "ExpandInto" } else { "Expand" }.into(),
                detail: format!("({}) {arrow}[{}{types_text}] ({})", name(from.slot), name(r.slot), name(to.slot)),
                estimated_rows: None,
            });
            cx.steps.push(Step::Expand { from: from.slot, edge: r.slot, to: to.slot, dir, types, to_labels, to_bound });
        }
    }
    cx.bound_slots[r.slot] = true;
    cx.bound_slots[to.slot] = true;
}

fn place_filters(conjuncts: &mut [Conjunct], bound: &[bool], steps: &mut Vec<Step>, explain: &mut QueryPlan) {
    for c in conjuncts.iter_mut() {
        if !c.placed && c.deps.iter().all(|d| bound[*d]) {
            c.placed = true;
            explain.steps.push(PlanStep { operator: "Filter".into(), detail: c.text.clone(), estimated_rows: None });
            steps.push(Step::Filter(c.expr.clone()));
        }
    }
}
