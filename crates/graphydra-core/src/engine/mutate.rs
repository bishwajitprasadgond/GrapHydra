//! Compilation and execution of CREATE / SET / REMOVE / DELETE.

use std::collections::BTreeMap;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::engine::expr::{CExpr, Compiler, Ctx, eval};
use crate::engine::plan::{VarKind, VarTable};
use crate::engine::result::QueryStats;
use crate::engine::rv::Rv;
use crate::error::{GraphError, Result};
use crate::language::ast::{Pattern, RemoveItem, SetItem, UpdateClause};
use crate::model::{Edge, Node, RESERVED_ID_KEY};
use crate::ops::Op;
use crate::store::{Direction, Store};
use crate::value::{Properties, Value};

/// Write access to a graph inside a transaction.
pub(crate) trait Mutator {
    fn store(&self) -> &Store;
    fn apply(&mut self, op: Op) -> Result<()>;
}

#[derive(Debug, Clone)]
enum CreateNode {
    Existing,
    New { slot: usize, labels: Vec<String>, id: Option<CExpr>, props: Vec<(String, CExpr)> },
}

#[derive(Debug, Clone)]
struct CreateRel {
    slot: usize,
    edge_type: String,
    left: usize,
    right: usize,
    dir: Direction,
    id: Option<CExpr>,
    props: Vec<(String, CExpr)>,
}

#[derive(Debug, Clone)]
enum Update {
    Create { nodes: Vec<CreateNode>, rels: Vec<CreateRel> },
    SetProp { var: usize, key: String, value: CExpr },
    SetMap { var: usize, value: CExpr, replace: bool },
    AddLabel { var: usize, label: String },
    RemoveLabel { var: usize, label: String },
    Delete { detach: bool, exprs: Vec<CExpr> },
}

pub(crate) struct UpdatePlan {
    updates: Vec<Update>,
}

fn split_id(
    props: &[(String, crate::language::ast::Expr)],
    c: &mut Compiler<'_>,
    vars: &VarTable,
) -> Result<(Option<CExpr>, Vec<(String, CExpr)>)> {
    let mut id = None;
    let mut out = Vec::new();
    for (k, e) in props {
        let ce = c.compile(e, vars)?;
        if k == RESERVED_ID_KEY {
            id = Some(ce);
        } else {
            out.push((k.clone(), ce));
        }
    }
    Ok((id, out))
}

fn entity_var(vars: &VarTable, name: &str, clause: &str) -> Result<usize> {
    match vars.kind_of(name) {
        Some(VarKind::Node | VarKind::Edge) => Ok(vars.lookup(name).expect("kind_of found it")),
        Some(k) => Err(GraphError::semantic(format!("{clause} expects a node or edge, but `{name}` is a {k:?}"))),
        None => Err(GraphError::semantic(format!("variable `{name}` is not defined"))),
    }
}

impl UpdatePlan {
    /// Compile update clauses; variables introduced by CREATE are added to `vars`.
    pub(crate) fn compile(
        clauses: &[UpdateClause],
        vars: &mut VarTable,
        params: &BTreeMap<String, Value>,
    ) -> Result<Self> {
        let mut updates = Vec::new();
        for clause in clauses {
            match clause {
                UpdateClause::Create(patterns) => {
                    for p in patterns {
                        updates.push(compile_create(p, vars, params)?);
                    }
                }
                UpdateClause::Set(items) => {
                    let mut c = Compiler { params, aggs: None };
                    for item in items {
                        updates.push(match item {
                            SetItem::Property { var, key, value } => {
                                if key == RESERVED_ID_KEY {
                                    return Err(GraphError::semantic("ids are immutable; `id` cannot be SET"));
                                }
                                let v = entity_var(vars, var, "SET")?;
                                Update::SetProp { var: v, key: key.clone(), value: c.compile(value, vars)? }
                            }
                            SetItem::Merge { var, value, replace } => {
                                let v = entity_var(vars, var, "SET")?;
                                Update::SetMap { var: v, value: c.compile(value, vars)?, replace: *replace }
                            }
                            SetItem::Label { var, label } => {
                                if vars.kind_of(var) != Some(VarKind::Node) {
                                    return Err(GraphError::semantic(format!(
                                        "labels can only be set on nodes (`{var}`)"
                                    )));
                                }
                                Update::AddLabel { var: vars.lookup(var).expect("checked"), label: label.clone() }
                            }
                        });
                    }
                }
                UpdateClause::Remove(items) => {
                    for item in items {
                        updates.push(match item {
                            RemoveItem::Property { var, key } => {
                                if key == RESERVED_ID_KEY {
                                    return Err(GraphError::semantic("ids cannot be removed"));
                                }
                                let v = entity_var(vars, var, "REMOVE")?;
                                Update::SetProp { var: v, key: key.clone(), value: CExpr::Const(Rv::Null) }
                            }
                            RemoveItem::Label { var, label } => {
                                if vars.kind_of(var) != Some(VarKind::Node) {
                                    return Err(GraphError::semantic(format!(
                                        "labels can only be removed from nodes (`{var}`)"
                                    )));
                                }
                                Update::RemoveLabel { var: vars.lookup(var).expect("checked"), label: label.clone() }
                            }
                        });
                    }
                }
                UpdateClause::Delete { detach, exprs } => {
                    let mut c = Compiler { params, aggs: None };
                    let exprs = exprs.iter().map(|e| c.compile(e, vars)).collect::<Result<Vec<_>>>()?;
                    updates.push(Update::Delete { detach: *detach, exprs });
                }
            }
        }
        Ok(UpdatePlan { updates })
    }

    /// Apply all updates for one matched row.
    pub(crate) fn apply_row(&self, m: &mut dyn Mutator, row: &mut [Rv], stats: &mut QueryStats) -> Result<()> {
        for u in &self.updates {
            match u {
                Update::Create { nodes, rels } => {
                    for n in nodes {
                        let CreateNode::New { slot, labels, id, props } = n else { continue };
                        let ctx = Ctx::new(m.store(), row);
                        let id = match id {
                            Some(e) => id_value(eval(e, &ctx)?, "node")?,
                            None => generate_id(m.store(), "n", true),
                        };
                        let properties = eval_props(props, &ctx)?;
                        let node = Node { id: id.clone(), labels: labels.iter().cloned().collect(), properties };
                        m.apply(Op::AddNode { node })?;
                        stats.nodes_created += 1;
                        row[*slot] = Rv::Node(m.store().node_slot(&id).expect("just created"));
                    }
                    for r in rels {
                        let ctx = Ctx::new(m.store(), row);
                        let endpoint = |slot: usize| -> Result<String> {
                            match &row[slot] {
                                Rv::Node(s) => m
                                    .store()
                                    .node(*s)
                                    .map(|n| n.id.to_string())
                                    .ok_or_else(|| GraphError::semantic("cannot create an edge to a deleted node")),
                                _ => Err(GraphError::semantic("edge endpoints in CREATE must be nodes")),
                            }
                        };
                        let (l, rr) = (endpoint(r.left)?, endpoint(r.right)?);
                        let id = match &r.id {
                            Some(e) => id_value(eval(e, &ctx)?, "edge")?,
                            None => generate_id(m.store(), "e", false),
                        };
                        let properties = eval_props(&r.props, &ctx)?;
                        let (source, target, directed) = match r.dir {
                            Direction::Outgoing => (l, rr, true),
                            Direction::Incoming => (rr, l, true),
                            Direction::Both => (l, rr, false),
                        };
                        let edge = Edge {
                            id: id.clone(),
                            source,
                            target,
                            edge_type: r.edge_type.clone(),
                            directed,
                            properties,
                        };
                        m.apply(Op::AddEdge { edge })?;
                        stats.edges_created += 1;
                        row[r.slot] = Rv::Edge(m.store().edge_slot(&id).expect("just created"));
                    }
                }
                Update::SetProp { var, key, value } => {
                    let v = eval(value, &Ctx::new(m.store(), row))?;
                    let value = storable(v, key)?;
                    if let Some(op) = prop_op(m.store(), &row[*var], key, value)? {
                        m.apply(op)?;
                        stats.properties_set += 1;
                    }
                }
                Update::SetMap { var, value, replace } => {
                    let store = m.store();
                    let v = eval(value, &Ctx::new(store, row))?;
                    let map: BTreeMap<String, Rv> = match v {
                        Rv::Map(m) => m,
                        Rv::Node(s) => store.node(s).map(|n| props_rv(&n.props)).unwrap_or_default(),
                        Rv::Edge(s) => store.edge(s).map(|e| props_rv(&e.props)).unwrap_or_default(),
                        Rv::Null if *replace => BTreeMap::new(),
                        Rv::Null => continue,
                        other => return Err(GraphError::type_err(format!("SET {} requires a map", other.type_name()))),
                    };
                    let mut ops = Vec::new();
                    if *replace {
                        let existing: Vec<String> = match &row[*var] {
                            Rv::Node(s) => {
                                store.node(*s).map(|n| n.props.keys().cloned().collect()).unwrap_or_default()
                            }
                            Rv::Edge(s) => {
                                store.edge(*s).map(|e| e.props.keys().cloned().collect()).unwrap_or_default()
                            }
                            _ => Vec::new(),
                        };
                        for k in existing.into_iter().filter(|k| !map.contains_key(k)) {
                            if let Some(op) = prop_op(store, &row[*var], &k, Value::Null)? {
                                ops.push(op);
                            }
                        }
                    }
                    for (k, v) in map {
                        if k == RESERVED_ID_KEY {
                            return Err(GraphError::semantic(
                                "ids are immutable; maps passed to SET must not contain `id`",
                            ));
                        }
                        let value = storable(v, &k)?;
                        if let Some(op) = prop_op(store, &row[*var], &k, value)? {
                            ops.push(op);
                        }
                    }
                    for op in ops {
                        m.apply(op)?;
                        stats.properties_set += 1;
                    }
                }
                Update::AddLabel { var, label } | Update::RemoveLabel { var, label } => {
                    let adding = matches!(u, Update::AddLabel { .. });
                    match &row[*var] {
                        Rv::Null => {}
                        Rv::Node(s) => {
                            let Some(n) = m.store().node(*s) else {
                                return Err(GraphError::semantic("cannot change labels of a deleted node"));
                            };
                            let id = n.id.to_string();
                            let has = m.store().node_label_names(*s).iter().any(|l| l == label);
                            if adding && !has {
                                m.apply(Op::AddLabel { id, label: label.clone() })?;
                                stats.labels_added += 1;
                            } else if !adding && has {
                                m.apply(Op::RemoveLabel { id, label: label.clone() })?;
                                stats.labels_removed += 1;
                            }
                        }
                        other => {
                            return Err(GraphError::type_err(format!(
                                "labels apply to nodes, not {}",
                                other.type_name()
                            )));
                        }
                    }
                }
                Update::Delete { detach, exprs } => {
                    let mut targets = Vec::new();
                    for e in exprs {
                        collect_deletes(eval(e, &Ctx::new(m.store(), row))?, &mut targets)?;
                    }
                    // Edges first so that `DELETE n, r` works when r is n's only edge.
                    targets.sort_by_key(|t| matches!(t, Rv::Node(_)));
                    for t in targets {
                        match t {
                            Rv::Edge(s) => {
                                if let Some(e) = m.store().edge(s) {
                                    let id = e.id.to_string();
                                    m.apply(Op::DeleteEdge { id })?;
                                    stats.edges_deleted += 1;
                                }
                            }
                            Rv::Node(s) => {
                                let Some(n) = m.store().node(s) else { continue };
                                let id = n.id.to_string();
                                if *detach {
                                    for e in m.store().incident_edges(s) {
                                        let eid = m.store().edge(e).expect("incident edge is live").id.to_string();
                                        m.apply(Op::DeleteEdge { id: eid })?;
                                        stats.edges_deleted += 1;
                                    }
                                }
                                m.apply(Op::DeleteNode { id })?;
                                stats.nodes_deleted += 1;
                            }
                            _ => unreachable!("collect_deletes only yields entities"),
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn compile_create(p: &Pattern, vars: &mut VarTable, params: &BTreeMap<String, Value>) -> Result<Update> {
    if p.path_var.is_some() || p.shortest {
        return Err(GraphError::semantic("path variables and shortestPath() are not allowed in CREATE"));
    }
    let mut c = Compiler { params, aggs: None };
    let mut nodes = Vec::new();
    let mut node_slots = Vec::new();
    let all_nodes = std::iter::once(&p.start).chain(p.steps.iter().map(|(_, n)| n));
    for n in all_nodes {
        if let Some(name) = &n.var
            && let Some(kind) = vars.kind_of(name)
        {
            if kind != VarKind::Node {
                return Err(GraphError::semantic(format!("`{name}` is not a node")));
            }
            if !n.labels.is_empty() || !n.props.is_empty() {
                return Err(GraphError::semantic(format!(
                    "`{name}` is already bound; CREATE cannot add labels or properties to it (use SET)"
                )));
            }
            let slot = vars.lookup(name).expect("kind_of found it");
            nodes.push(CreateNode::Existing);
            node_slots.push(slot);
            continue;
        }
        let (id, props) = split_id(&n.props, &mut c, vars)?;
        let slot = vars.add(n.var.as_deref(), VarKind::Node)?;
        nodes.push(CreateNode::New { slot, labels: n.labels.clone(), id, props });
        node_slots.push(slot);
    }
    let mut rels = Vec::new();
    for (i, (r, _)) in p.steps.iter().enumerate() {
        if r.length.is_some() {
            return Err(GraphError::semantic("variable-length relationships cannot be created"));
        }
        if r.types.len() != 1 {
            return Err(GraphError::semantic("CREATE needs exactly one relationship type, e.g. -[:KNOWS]->"));
        }
        if let Some(name) = &r.var
            && vars.lookup(name).is_some()
        {
            return Err(GraphError::semantic(format!("variable `{name}` is already bound")));
        }
        let (id, props) = split_id(&r.props, &mut c, vars)?;
        let slot = vars.add(r.var.as_deref(), VarKind::Edge)?;
        rels.push(CreateRel {
            slot,
            edge_type: r.types[0].clone(),
            left: node_slots[i],
            right: node_slots[i + 1],
            dir: r.direction,
            id,
            props,
        });
    }
    Ok(Update::Create { nodes, rels })
}

fn props_rv(p: &Properties) -> BTreeMap<String, Rv> {
    p.iter().map(|(k, v)| (k.clone(), Rv::from_value(v))).collect()
}

fn storable(v: Rv, key: &str) -> Result<Value> {
    v.to_value().ok_or_else(|| {
        GraphError::type_err(format!("property `{key}` cannot store a {}; only data values are allowed", v.type_name()))
    })
}

fn eval_props(props: &[(String, CExpr)], ctx: &Ctx<'_>) -> Result<Properties> {
    let mut out = Properties::new();
    for (k, e) in props {
        let v = storable(eval(e, ctx)?, k)?;
        if !v.is_null() {
            out.insert(k.clone(), v);
        }
    }
    Ok(out)
}

fn id_value(v: Rv, kind: &str) -> Result<String> {
    match v {
        Rv::Str(s) => Ok(s),
        Rv::Int(i) => Ok(i.to_string()),
        other => Err(GraphError::type_err(format!("{kind} id must be a string, got {}", other.type_name()))),
    }
}

fn prop_op(store: &Store, target: &Rv, key: &str, value: Value) -> Result<Option<Op>> {
    Ok(match target {
        Rv::Null => None,
        Rv::Node(s) => {
            let n = store.node(*s).ok_or_else(|| GraphError::semantic("cannot set a property on a deleted node"))?;
            if n.props.get(key) == Some(&value) || (value.is_null() && !n.props.contains_key(key)) {
                None
            } else {
                Some(Op::SetNodeProperty { id: n.id.to_string(), key: key.to_owned(), value })
            }
        }
        Rv::Edge(s) => {
            let e = store.edge(*s).ok_or_else(|| GraphError::semantic("cannot set a property on a deleted edge"))?;
            if e.props.get(key) == Some(&value) || (value.is_null() && !e.props.contains_key(key)) {
                None
            } else {
                Some(Op::SetEdgeProperty { id: e.id.to_string(), key: key.to_owned(), value })
            }
        }
        other => return Err(GraphError::type_err(format!("cannot set properties on {}", other.type_name()))),
    })
}

fn collect_deletes(v: Rv, out: &mut Vec<Rv>) -> Result<()> {
    match v {
        Rv::Null => {}
        Rv::Node(_) | Rv::Edge(_) => {
            if !out.contains(&v) {
                out.push(v);
            }
        }
        Rv::Path(p) => {
            for e in p.edges {
                collect_deletes(Rv::Edge(e), out)?;
            }
            for n in p.nodes {
                collect_deletes(Rv::Node(n), out)?;
            }
        }
        Rv::List(items) => {
            for i in items {
                collect_deletes(i, out)?;
            }
        }
        other => {
            return Err(GraphError::type_err(format!(
                "DELETE expects nodes, edges or paths, got {}",
                other.type_name()
            )));
        }
    }
    Ok(())
}

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate an id that does not exist yet: `<prefix>-<16 hex digits>`.
pub(crate) fn generate_id(store: &Store, prefix: &str, node: bool) -> String {
    loop {
        let mut h = std::hash::RandomState::new().build_hasher();
        h.write_u64(ID_COUNTER.fetch_add(1, Ordering::Relaxed));
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
        let id = format!("{prefix}-{:016x}", h.finish());
        let exists = if node { store.node_slot(&id).is_some() } else { store.edge_slot(&id).is_some() };
        if !exists {
            return id;
        }
    }
}
