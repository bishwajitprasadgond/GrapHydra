//! In-memory record store with primary, label, type, adjacency and
//! property indexes.
//!
//! Records live in slot arenas (`Vec<Option<_>>`). Slots are never reused
//! while a store is alive, which makes undo exact: a deleted record is
//! restored into its original slot. Slots are compacted only when a fresh
//! store is built (on open or checkpoint).

use std::collections::{BTreeSet, HashMap};
use std::ops::ControlFlow;
use std::sync::Arc;

use crate::config::Limits;
use crate::error::{EntityKind, GraphError, Result};
use crate::indexes::PropertyIndex;
use crate::model::{
    self, Edge, IndexDef, IndexKind, IndexTarget, Node, validate_edge, validate_index_def, validate_node,
    validate_property_key, validate_value,
};
use crate::ops::Op;
use crate::value::{Properties, Value};

pub(crate) type Slot = u32;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NodeRec {
    pub(crate) id: Arc<str>,
    /// Sorted, unique label ids.
    pub(crate) labels: Vec<u32>,
    pub(crate) props: Properties,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EdgeRec {
    pub(crate) id: Arc<str>,
    pub(crate) src: Slot,
    pub(crate) tgt: Slot,
    pub(crate) ty: u32,
    pub(crate) directed: bool,
    pub(crate) props: Properties,
}

impl EdgeRec {
    /// The endpoint opposite `node` (itself for self-loops).
    pub(crate) fn other(&self, node: Slot) -> Slot {
        if self.src == node { self.tgt } else { self.src }
    }
}

/// Direction of an adjacency expansion relative to the current node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Outgoing,
    Incoming,
    Both,
}

impl Direction {
    pub fn reverse(self) -> Direction {
        match self {
            Direction::Outgoing => Direction::Incoming,
            Direction::Incoming => Direction::Outgoing,
            Direction::Both => Direction::Both,
        }
    }
}

/// String interner for labels and edge types. Ids are never removed.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Interner {
    names: Vec<Arc<str>>,
    ids: HashMap<Arc<str>, u32>,
}

impl Interner {
    pub(crate) fn get(&self, name: &str) -> Option<u32> {
        self.ids.get(name).copied()
    }

    pub(crate) fn intern(&mut self, name: &str) -> u32 {
        if let Some(id) = self.ids.get(name) {
            return *id;
        }
        let id = u32::try_from(self.names.len()).expect("fewer than 2^32 distinct names");
        let name: Arc<str> = Arc::from(name);
        self.names.push(name.clone());
        self.ids.insert(name, id);
        id
    }

    pub(crate) fn name(&self, id: u32) -> &str {
        &self.names[id as usize]
    }

    pub(crate) fn len(&self) -> usize {
        self.names.len()
    }
}

/// Inverse of an applied op, used for rollback.
#[derive(Debug, Clone)]
pub(crate) enum Undo {
    Nothing,
    RemoveLastNode,
    RemoveLastEdge,
    RestoreNode(Slot, NodeRec),
    RestoreEdge(Slot, EdgeRec),
    NodeProp(Slot, String, Option<Value>),
    EdgeProp(Slot, String, Option<Value>),
    RemoveLabel(Slot, u32),
    AddLabel(Slot, u32),
    DropIndex(IndexDef),
    CreateIndex(IndexDef),
    Metadata(String, Option<Value>),
}

#[derive(Debug, Clone)]
pub(crate) struct Store {
    nodes: Vec<Option<NodeRec>>,
    edges: Vec<Option<EdgeRec>>,
    node_ids: HashMap<Arc<str>, Slot>,
    edge_ids: HashMap<Arc<str>, Slot>,
    pub(crate) labels: Interner,
    pub(crate) types: Interner,
    label_members: Vec<BTreeSet<Slot>>,
    type_members: Vec<BTreeSet<Slot>>,
    /// Per node: (edge type, edge slot), sorted. Undirected edges appear in
    /// the outgoing and incoming lists of both endpoints.
    out_adj: Vec<Vec<(u32, Slot)>>,
    in_adj: Vec<Vec<(u32, Slot)>>,
    node_indexes: Vec<PropertyIndex>,
    edge_indexes: Vec<PropertyIndex>,
    live_nodes: usize,
    live_edges: usize,
    pub(crate) metadata: Properties,
    pub(crate) limits: Limits,
}

impl Store {
    pub(crate) fn new(limits: Limits) -> Self {
        Store {
            nodes: Vec::new(),
            edges: Vec::new(),
            node_ids: HashMap::new(),
            edge_ids: HashMap::new(),
            labels: Interner::default(),
            types: Interner::default(),
            label_members: Vec::new(),
            type_members: Vec::new(),
            out_adj: Vec::new(),
            in_adj: Vec::new(),
            node_indexes: Vec::new(),
            edge_indexes: Vec::new(),
            live_nodes: 0,
            live_edges: 0,
            metadata: Properties::new(),
            limits,
        }
    }

    // ------------------------------------------------------------------
    // Read access
    // ------------------------------------------------------------------

    pub(crate) fn node_count(&self) -> usize {
        self.live_nodes
    }

    pub(crate) fn edge_count(&self) -> usize {
        self.live_edges
    }

    pub(crate) fn node(&self, slot: Slot) -> Option<&NodeRec> {
        self.nodes.get(slot as usize).and_then(Option::as_ref)
    }

    pub(crate) fn edge(&self, slot: Slot) -> Option<&EdgeRec> {
        self.edges.get(slot as usize).and_then(Option::as_ref)
    }

    pub(crate) fn node_slot(&self, id: &str) -> Option<Slot> {
        self.node_ids.get(id).copied()
    }

    pub(crate) fn edge_slot(&self, id: &str) -> Option<Slot> {
        self.edge_ids.get(id).copied()
    }

    pub(crate) fn node_slots_iter(&self) -> impl Iterator<Item = Slot> + '_ {
        self.nodes.iter().enumerate().filter(|(_, n)| n.is_some()).map(|(i, _)| i as Slot)
    }

    pub(crate) fn edge_slots_iter(&self) -> impl Iterator<Item = Slot> + '_ {
        self.edges.iter().enumerate().filter(|(_, e)| e.is_some()).map(|(i, _)| i as Slot)
    }

    pub(crate) fn label_members(&self, label: u32) -> Option<&BTreeSet<Slot>> {
        self.label_members.get(label as usize)
    }

    pub(crate) fn label_count(&self, label: u32) -> usize {
        self.label_members(label).map_or(0, BTreeSet::len)
    }

    pub(crate) fn type_members(&self, ty: u32) -> Option<&BTreeSet<Slot>> {
        self.type_members.get(ty as usize)
    }

    pub(crate) fn type_count(&self, ty: u32) -> usize {
        self.type_members(ty).map_or(0, BTreeSet::len)
    }

    /// Labels with at least one node, with counts, sorted by name.
    pub(crate) fn label_stats(&self) -> Vec<(String, usize)> {
        let mut v: Vec<(String, usize)> = (0..self.labels.len() as u32)
            .map(|l| (self.labels.name(l).to_owned(), self.label_count(l)))
            .filter(|(_, c)| *c > 0)
            .collect();
        v.sort();
        v
    }

    pub(crate) fn type_stats(&self) -> Vec<(String, usize)> {
        let mut v: Vec<(String, usize)> = (0..self.types.len() as u32)
            .map(|t| (self.types.name(t).to_owned(), self.type_count(t)))
            .filter(|(_, c)| *c > 0)
            .collect();
        v.sort();
        v
    }

    pub(crate) fn node_has_labels(&self, slot: Slot, labels: &[u32]) -> bool {
        match self.node(slot) {
            Some(n) => labels.iter().all(|l| n.labels.binary_search(l).is_ok()),
            None => false,
        }
    }

    pub(crate) fn node_to_public(&self, slot: Slot) -> Option<Node> {
        let n = self.node(slot)?;
        Some(Node {
            id: n.id.to_string(),
            labels: n.labels.iter().map(|l| self.labels.name(*l).to_owned()).collect(),
            properties: n.props.clone(),
        })
    }

    pub(crate) fn edge_to_public(&self, slot: Slot) -> Option<Edge> {
        let e = self.edge(slot)?;
        Some(Edge {
            id: e.id.to_string(),
            source: self.node(e.src)?.id.to_string(),
            target: self.node(e.tgt)?.id.to_string(),
            edge_type: self.types.name(e.ty).to_owned(),
            directed: e.directed,
            properties: e.props.clone(),
        })
    }

    pub(crate) fn node_label_names(&self, slot: Slot) -> Vec<String> {
        self.node(slot).map(|n| n.labels.iter().map(|l| self.labels.name(*l).to_owned()).collect()).unwrap_or_default()
    }

    /// Visit edges adjacent to `node`. Each incident edge is visited once per
    /// call. `types`, when given, must be sorted and de-duplicated.
    pub(crate) fn for_each_adjacent<B>(
        &self,
        node: Slot,
        dir: Direction,
        types: Option<&[u32]>,
        mut f: impl FnMut(Slot, Slot) -> ControlFlow<B>,
    ) -> ControlFlow<B> {
        let Some(out) = self.out_adj.get(node as usize) else { return ControlFlow::Continue(()) };
        let inc = &self.in_adj[node as usize];
        let visit = |list: &[(u32, Slot)], skip_dupes: bool, f: &mut dyn FnMut(Slot, Slot) -> ControlFlow<B>| {
            let mut run = |part: &[(u32, Slot)]| {
                for &(_, e) in part {
                    let rec = self.edges[e as usize].as_ref().expect("adjacency references live edge");
                    if skip_dupes && (!rec.directed || rec.src == rec.tgt) {
                        // Already visited through the outgoing list.
                        continue;
                    }
                    f(e, rec.other(node))?;
                }
                ControlFlow::Continue(())
            };
            match types {
                None => run(list),
                Some(ts) => {
                    for t in ts {
                        let lo = list.partition_point(|(ty, _)| ty < t);
                        let hi = list.partition_point(|(ty, _)| ty <= t);
                        run(&list[lo..hi])?;
                    }
                    ControlFlow::Continue(())
                }
            }
        };
        match dir {
            Direction::Outgoing => visit(out, false, &mut f),
            Direction::Incoming => visit(inc, false, &mut f),
            Direction::Both => {
                visit(out, false, &mut f)?;
                visit(inc, true, &mut f)
            }
        }
    }

    /// Distinct incident edges of a node, ascending.
    pub(crate) fn incident_edges(&self, node: Slot) -> Vec<Slot> {
        let mut v: Vec<Slot> = Vec::new();
        if let (Some(o), Some(i)) = (self.out_adj.get(node as usize), self.in_adj.get(node as usize)) {
            v.extend(o.iter().map(|(_, e)| *e));
            v.extend(i.iter().map(|(_, e)| *e));
        }
        v.sort_unstable();
        v.dedup();
        v
    }

    pub(crate) fn degree(&self, node: Slot, dir: Direction) -> usize {
        let mut n = 0usize;
        let _ = self.for_each_adjacent::<()>(node, dir, None, |_, _| {
            n += 1;
            ControlFlow::Continue(())
        });
        n
    }

    // ------------------------------------------------------------------
    // Property indexes
    // ------------------------------------------------------------------

    pub(crate) fn indexes(&self, target: IndexTarget) -> &[PropertyIndex] {
        match target {
            IndexTarget::Node => &self.node_indexes,
            IndexTarget::Edge => &self.edge_indexes,
        }
    }

    pub(crate) fn index_defs(&self) -> Vec<IndexDef> {
        let mut v: Vec<IndexDef> = self.node_indexes.iter().chain(&self.edge_indexes).map(|i| i.def.clone()).collect();
        v.sort();
        v
    }

    /// Best index for an operation on `property`, preferring the given kinds in order.
    pub(crate) fn find_index(
        &self,
        target: IndexTarget,
        property: &str,
        kinds: &[IndexKind],
    ) -> Option<&PropertyIndex> {
        let list = self.indexes(target);
        kinds.iter().find_map(|k| list.iter().find(|i| i.def.kind == *k && i.def.property == property))
    }

    fn index_insert_all(indexes: &mut [PropertyIndex], slot: Slot, props: &Properties) {
        for idx in indexes {
            if let Some(v) = props.get(&idx.def.property) {
                idx.insert(slot, v);
            }
        }
    }

    fn index_remove_all(indexes: &mut [PropertyIndex], slot: Slot, props: &Properties) {
        for idx in indexes {
            if let Some(v) = props.get(&idx.def.property) {
                idx.remove(slot, v);
            }
        }
    }

    fn index_update(indexes: &mut [PropertyIndex], slot: Slot, key: &str, old: Option<&Value>, new: Option<&Value>) {
        for idx in indexes.iter_mut().filter(|i| i.def.property == key) {
            if let Some(o) = old {
                idx.remove(slot, o);
            }
            if let Some(n) = new {
                idx.insert(slot, n);
            }
        }
    }

    fn build_index(&self, def: &IndexDef) -> PropertyIndex {
        let mut idx = PropertyIndex::new(def.clone());
        match def.target {
            IndexTarget::Node => {
                for (slot, n) in self.nodes.iter().enumerate() {
                    if let Some(v) = n.as_ref().and_then(|n| n.props.get(&def.property)) {
                        idx.insert(slot as Slot, v);
                    }
                }
            }
            IndexTarget::Edge => {
                for (slot, e) in self.edges.iter().enumerate() {
                    if let Some(v) = e.as_ref().and_then(|e| e.props.get(&def.property)) {
                        idx.insert(slot as Slot, v);
                    }
                }
            }
        }
        idx
    }

    // ------------------------------------------------------------------
    // Mutation
    // ------------------------------------------------------------------

    fn ensure_label_capacity(&mut self, id: u32) {
        if self.label_members.len() <= id as usize {
            self.label_members.resize_with(id as usize + 1, BTreeSet::new);
        }
    }

    fn ensure_type_capacity(&mut self, id: u32) {
        if self.type_members.len() <= id as usize {
            self.type_members.resize_with(id as usize + 1, BTreeSet::new);
        }
    }

    fn adj_insert(list: &mut Vec<(u32, Slot)>, entry: (u32, Slot)) {
        if let Err(pos) = list.binary_search(&entry) {
            list.insert(pos, entry);
        }
    }

    fn adj_remove(list: &mut Vec<(u32, Slot)>, entry: (u32, Slot)) {
        if let Ok(pos) = list.binary_search(&entry) {
            list.remove(pos);
        }
    }

    fn link_edge(&mut self, slot: Slot, rec: &EdgeRec) {
        let entry = (rec.ty, slot);
        Self::adj_insert(&mut self.out_adj[rec.src as usize], entry);
        Self::adj_insert(&mut self.in_adj[rec.tgt as usize], entry);
        if !rec.directed && rec.src != rec.tgt {
            Self::adj_insert(&mut self.out_adj[rec.tgt as usize], entry);
            Self::adj_insert(&mut self.in_adj[rec.src as usize], entry);
        }
    }

    fn unlink_edge(&mut self, slot: Slot, rec: &EdgeRec) {
        let entry = (rec.ty, slot);
        Self::adj_remove(&mut self.out_adj[rec.src as usize], entry);
        Self::adj_remove(&mut self.in_adj[rec.tgt as usize], entry);
        if !rec.directed && rec.src != rec.tgt {
            Self::adj_remove(&mut self.out_adj[rec.tgt as usize], entry);
            Self::adj_remove(&mut self.in_adj[rec.src as usize], entry);
        }
    }

    fn place_node(&mut self, slot: Slot, rec: NodeRec) {
        for &l in &rec.labels {
            self.ensure_label_capacity(l);
            self.label_members[l as usize].insert(slot);
        }
        Self::index_insert_all(&mut self.node_indexes, slot, &rec.props);
        self.node_ids.insert(rec.id.clone(), slot);
        self.nodes[slot as usize] = Some(rec);
        self.live_nodes += 1;
    }

    fn take_node(&mut self, slot: Slot) -> NodeRec {
        let rec = self.nodes[slot as usize].take().expect("live node slot");
        for &l in &rec.labels {
            self.label_members[l as usize].remove(&slot);
        }
        Self::index_remove_all(&mut self.node_indexes, slot, &rec.props);
        self.node_ids.remove(&*rec.id);
        self.live_nodes -= 1;
        rec
    }

    fn place_edge(&mut self, slot: Slot, rec: EdgeRec) {
        self.ensure_type_capacity(rec.ty);
        self.type_members[rec.ty as usize].insert(slot);
        self.link_edge(slot, &rec);
        Self::index_insert_all(&mut self.edge_indexes, slot, &rec.props);
        self.edge_ids.insert(rec.id.clone(), slot);
        self.edges[slot as usize] = Some(rec);
        self.live_edges += 1;
    }

    fn take_edge(&mut self, slot: Slot) -> EdgeRec {
        let rec = self.edges[slot as usize].take().expect("live edge slot");
        self.type_members[rec.ty as usize].remove(&slot);
        self.unlink_edge(slot, &rec);
        Self::index_remove_all(&mut self.edge_indexes, slot, &rec.props);
        self.edge_ids.remove(&*rec.id);
        self.live_edges -= 1;
        rec
    }

    fn next_slot(len: usize, kind: &str) -> Result<Slot> {
        Slot::try_from(len)
            .ok()
            .filter(|s| *s < Slot::MAX)
            .ok_or_else(|| GraphError::LimitExceeded(format!("too many {kind} slots; checkpoint to compact")))
    }

    fn require_node(&self, id: &str) -> Result<Slot> {
        self.node_slot(id).ok_or_else(|| GraphError::NotFound { kind: EntityKind::Node, id: id.to_owned() })
    }

    fn require_edge(&self, id: &str) -> Result<Slot> {
        self.edge_slot(id).ok_or_else(|| GraphError::NotFound { kind: EntityKind::Edge, id: id.to_owned() })
    }

    /// Validate and apply one op. On error the store is unchanged.
    pub(crate) fn apply(&mut self, op: &Op) -> Result<Undo> {
        match op {
            Op::AddNode { node } => {
                validate_node(node, &self.limits)?;
                if self.node_ids.contains_key(node.id.as_str()) {
                    return Err(GraphError::DuplicateId { kind: EntityKind::Node, id: node.id.clone() });
                }
                let slot = Self::next_slot(self.nodes.len(), "node")?;
                let mut labels: Vec<u32> = node.labels.iter().map(|l| self.labels.intern(l)).collect();
                labels.sort_unstable();
                labels.dedup();
                let rec = NodeRec { id: Arc::from(node.id.as_str()), labels, props: node.properties.clone() };
                self.nodes.push(None);
                self.out_adj.push(Vec::new());
                self.in_adj.push(Vec::new());
                self.place_node(slot, rec);
                Ok(Undo::RemoveLastNode)
            }
            Op::AddEdge { edge } => {
                validate_edge(edge, &self.limits)?;
                if self.edge_ids.contains_key(edge.id.as_str()) {
                    return Err(GraphError::DuplicateId { kind: EntityKind::Edge, id: edge.id.clone() });
                }
                let src = self
                    .node_slot(&edge.source)
                    .ok_or_else(|| GraphError::DanglingEdge { edge: edge.id.clone(), node: edge.source.clone() })?;
                let tgt = self
                    .node_slot(&edge.target)
                    .ok_or_else(|| GraphError::DanglingEdge { edge: edge.id.clone(), node: edge.target.clone() })?;
                let slot = Self::next_slot(self.edges.len(), "edge")?;
                let ty = self.types.intern(&edge.edge_type);
                let rec = EdgeRec {
                    id: Arc::from(edge.id.as_str()),
                    src,
                    tgt,
                    ty,
                    directed: edge.directed,
                    props: edge.properties.clone(),
                };
                self.edges.push(None);
                self.place_edge(slot, rec);
                Ok(Undo::RemoveLastEdge)
            }
            Op::DeleteNode { id } => {
                let slot = self.require_node(id)?;
                let incident = self.incident_edges(slot).len();
                if incident > 0 {
                    return Err(GraphError::NodeHasEdges { id: id.clone(), count: incident });
                }
                let rec = self.take_node(slot);
                Ok(Undo::RestoreNode(slot, rec))
            }
            Op::DeleteEdge { id } => {
                let slot = self.require_edge(id)?;
                let rec = self.take_edge(slot);
                Ok(Undo::RestoreEdge(slot, rec))
            }
            Op::SetNodeProperty { id, key, value } => {
                validate_property_key(key, &self.limits)?;
                validate_value(key, value, &self.limits)?;
                let slot = self.require_node(id)?;
                Ok(Undo::NodeProp(slot, key.clone(), self.set_node_prop(slot, key, value.clone())))
            }
            Op::SetEdgeProperty { id, key, value } => {
                validate_property_key(key, &self.limits)?;
                validate_value(key, value, &self.limits)?;
                let slot = self.require_edge(id)?;
                Ok(Undo::EdgeProp(slot, key.clone(), self.set_edge_prop(slot, key, value.clone())))
            }
            Op::AddLabel { id, label } => {
                model::validate_name("label", label, &self.limits)?;
                let slot = self.require_node(id)?;
                let l = self.labels.intern(label);
                Ok(if self.add_label(slot, l) { Undo::RemoveLabel(slot, l) } else { Undo::Nothing })
            }
            Op::RemoveLabel { id, label } => {
                let slot = self.require_node(id)?;
                match self.labels.get(label) {
                    Some(l) if self.remove_label(slot, l) => Ok(Undo::AddLabel(slot, l)),
                    _ => Ok(Undo::Nothing),
                }
            }
            Op::CreateIndex { index } => {
                validate_index_def(index, &self.limits)?;
                if self.indexes(index.target).iter().any(|i| i.def == *index) {
                    return Err(GraphError::Index(format!("{index} already exists")));
                }
                let built = self.build_index(index);
                match index.target {
                    IndexTarget::Node => self.node_indexes.push(built),
                    IndexTarget::Edge => self.edge_indexes.push(built),
                }
                Ok(Undo::DropIndex(index.clone()))
            }
            Op::DropIndex { index } => {
                let list = match index.target {
                    IndexTarget::Node => &mut self.node_indexes,
                    IndexTarget::Edge => &mut self.edge_indexes,
                };
                let pos = list
                    .iter()
                    .position(|i| i.def == *index)
                    .ok_or_else(|| GraphError::Index(format!("{index} does not exist")))?;
                list.remove(pos);
                Ok(Undo::CreateIndex(index.clone()))
            }
            Op::SetMetadata { key, value } => {
                model::validate_name("metadata key", key, &self.limits)?;
                validate_value(key, value, &self.limits)?;
                let old = if value.is_null() {
                    self.metadata.remove(key)
                } else {
                    self.metadata.insert(key.clone(), value.clone())
                };
                Ok(Undo::Metadata(key.clone(), old))
            }
        }
    }

    fn set_node_prop(&mut self, slot: Slot, key: &str, value: Value) -> Option<Value> {
        let rec = self.nodes[slot as usize].as_mut().expect("live node");
        let old = if value.is_null() { rec.props.remove(key) } else { rec.props.insert(key.to_owned(), value) };
        Self::index_update(&mut self.node_indexes, slot, key, old.as_ref(), rec.props.get(key));
        old
    }

    fn set_edge_prop(&mut self, slot: Slot, key: &str, value: Value) -> Option<Value> {
        let rec = self.edges[slot as usize].as_mut().expect("live edge");
        let old = if value.is_null() { rec.props.remove(key) } else { rec.props.insert(key.to_owned(), value) };
        Self::index_update(&mut self.edge_indexes, slot, key, old.as_ref(), rec.props.get(key));
        old
    }

    fn add_label(&mut self, slot: Slot, l: u32) -> bool {
        let rec = self.nodes[slot as usize].as_mut().expect("live node");
        match rec.labels.binary_search(&l) {
            Ok(_) => false,
            Err(pos) => {
                rec.labels.insert(pos, l);
                self.ensure_label_capacity(l);
                self.label_members[l as usize].insert(slot);
                true
            }
        }
    }

    fn remove_label(&mut self, slot: Slot, l: u32) -> bool {
        let rec = self.nodes[slot as usize].as_mut().expect("live node");
        match rec.labels.binary_search(&l) {
            Ok(pos) => {
                rec.labels.remove(pos);
                self.label_members[l as usize].remove(&slot);
                true
            }
            Err(_) => false,
        }
    }

    /// Revert an op. Undo entries must be applied in reverse order.
    pub(crate) fn undo(&mut self, undo: Undo) {
        match undo {
            Undo::Nothing => {}
            Undo::RemoveLastNode => {
                let slot = (self.nodes.len() - 1) as Slot;
                self.take_node(slot);
                self.nodes.pop();
                self.out_adj.pop();
                self.in_adj.pop();
            }
            Undo::RemoveLastEdge => {
                let slot = (self.edges.len() - 1) as Slot;
                self.take_edge(slot);
                self.edges.pop();
            }
            Undo::RestoreNode(slot, rec) => self.place_node(slot, rec),
            Undo::RestoreEdge(slot, rec) => self.place_edge(slot, rec),
            Undo::NodeProp(slot, key, old) => {
                self.set_node_prop(slot, &key, old.unwrap_or(Value::Null));
            }
            Undo::EdgeProp(slot, key, old) => {
                self.set_edge_prop(slot, &key, old.unwrap_or(Value::Null));
            }
            Undo::RemoveLabel(slot, l) => {
                self.remove_label(slot, l);
            }
            Undo::AddLabel(slot, l) => {
                self.add_label(slot, l);
            }
            Undo::DropIndex(def) => {
                let list = match def.target {
                    IndexTarget::Node => &mut self.node_indexes,
                    IndexTarget::Edge => &mut self.edge_indexes,
                };
                list.retain(|i| i.def != def);
            }
            Undo::CreateIndex(def) => {
                let built = self.build_index(&def);
                match def.target {
                    IndexTarget::Node => self.node_indexes.push(built),
                    IndexTarget::Edge => self.edge_indexes.push(built),
                }
            }
            Undo::Metadata(key, old) => match old {
                Some(v) => {
                    self.metadata.insert(key, v);
                }
                None => {
                    self.metadata.remove(&key);
                }
            },
        }
    }

    // ------------------------------------------------------------------
    // Maintenance
    // ------------------------------------------------------------------

    /// Fraction of slots that are tombstones.
    pub(crate) fn fragmentation(&self) -> f64 {
        let total = self.nodes.len() + self.edges.len();
        if total == 0 {
            return 0.0;
        }
        let dead = total - self.live_nodes - self.live_edges;
        dead as f64 / total as f64
    }

    /// Recompute every derived structure from the records and compare with
    /// the incrementally maintained ones. Returns a list of discrepancies.
    pub(crate) fn verify(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let mut fresh = Store::new(self.limits.clone());
        fresh.labels = self.labels.clone();
        fresh.types = self.types.clone();
        fresh.nodes = vec![None; self.nodes.len()];
        fresh.edges = vec![None; self.edges.len()];
        fresh.out_adj = vec![Vec::new(); self.nodes.len()];
        fresh.in_adj = vec![Vec::new(); self.nodes.len()];
        for (slot, n) in self.nodes.iter().enumerate() {
            if let Some(n) = n {
                fresh.place_node(slot as Slot, n.clone());
            }
        }
        for (slot, e) in self.edges.iter().enumerate() {
            if let Some(e) = e {
                if self.node(e.src).is_none() || self.node(e.tgt).is_none() {
                    problems.push(format!("edge {:?} has a dangling endpoint", e.id));
                    continue;
                }
                fresh.place_edge(slot as Slot, e.clone());
            }
        }
        for def in self.index_defs() {
            let built = fresh.build_index(&def);
            match def.target {
                IndexTarget::Node => fresh.node_indexes.push(built),
                IndexTarget::Edge => fresh.edge_indexes.push(built),
            }
        }
        if fresh.node_ids != self.node_ids {
            problems.push("node id index is inconsistent".into());
        }
        if fresh.edge_ids != self.edge_ids {
            problems.push("edge id index is inconsistent".into());
        }
        let trim = |v: &[BTreeSet<Slot>]| -> Vec<BTreeSet<Slot>> {
            let mut v = v.to_vec();
            while v.last().is_some_and(BTreeSet::is_empty) {
                v.pop();
            }
            v
        };
        if trim(&fresh.label_members) != trim(&self.label_members) {
            problems.push("label index is inconsistent".into());
        }
        if trim(&fresh.type_members) != trim(&self.type_members) {
            problems.push("edge type index is inconsistent".into());
        }
        if fresh.out_adj != self.out_adj || fresh.in_adj != self.in_adj {
            problems.push("adjacency index is inconsistent".into());
        }
        if fresh.live_nodes != self.live_nodes || fresh.live_edges != self.live_edges {
            problems.push("record counts are inconsistent".into());
        }
        for target in [IndexTarget::Node, IndexTarget::Edge] {
            for idx in self.indexes(target) {
                let other = fresh.indexes(target).iter().find(|i| i.def == idx.def);
                if other.is_none_or(|o| o.data != idx.data || o.entries != idx.entries) {
                    problems.push(format!("{} is inconsistent", idx.def));
                }
            }
        }
        problems
    }

    /// Rebuild all property indexes from records.
    pub(crate) fn rebuild_indexes(&mut self) {
        let defs = self.index_defs();
        self.node_indexes.clear();
        self.edge_indexes.clear();
        for def in defs {
            let built = self.build_index(&def);
            match def.target {
                IndexTarget::Node => self.node_indexes.push(built),
                IndexTarget::Edge => self.edge_indexes.push(built),
            }
        }
    }

    /// A new store with the same logical content and no tombstones.
    pub(crate) fn compacted(&self) -> Store {
        let mut fresh = Store::new(self.limits.clone());
        fresh.metadata = self.metadata.clone();
        for slot in self.node_slots_iter() {
            let node = self.node_to_public(slot).expect("live");
            fresh.apply(&Op::AddNode { node }).expect("live data is valid");
        }
        for slot in self.edge_slots_iter() {
            let edge = self.edge_to_public(slot).expect("live");
            fresh.apply(&Op::AddEdge { edge }).expect("live data is valid");
        }
        for def in self.index_defs() {
            fresh.apply(&Op::CreateIndex { index: def }).expect("index defs are valid");
        }
        fresh
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add_node(s: &mut Store, id: &str, labels: &[&str]) -> Undo {
        let mut n = Node::new(id);
        for l in labels {
            n = n.with_label(*l);
        }
        s.apply(&Op::AddNode { node: n }).expect("add node")
    }

    fn adjacent(s: &Store, id: &str, dir: Direction) -> Vec<String> {
        let slot = s.node_slot(id).expect("node");
        let mut out = Vec::new();
        let _ = s.for_each_adjacent::<()>(slot, dir, None, |e, _| {
            out.push(s.edge(e).expect("edge").id.to_string());
            ControlFlow::Continue(())
        });
        out
    }

    #[test]
    fn adjacency_handles_direction_and_self_loops() {
        let mut s = Store::new(Limits::default());
        add_node(&mut s, "a", &[]);
        add_node(&mut s, "b", &[]);
        for e in [
            Edge::new("ab", "a", "b", "T"),
            Edge::new("aa", "a", "a", "T"),
            Edge::new("u", "b", "a", "U").undirected(),
            Edge::new("uu", "a", "a", "U").undirected(),
        ] {
            s.apply(&Op::AddEdge { edge: e }).expect("add edge");
        }
        assert_eq!(adjacent(&s, "a", Direction::Outgoing), vec!["ab", "aa", "u", "uu"]);
        assert_eq!(adjacent(&s, "a", Direction::Incoming), vec!["aa", "u", "uu"]);
        assert_eq!(adjacent(&s, "a", Direction::Both), vec!["ab", "aa", "u", "uu"]);
        assert_eq!(adjacent(&s, "b", Direction::Both), vec!["u", "ab"]);
        assert!(s.verify().is_empty());
    }

    #[test]
    fn undo_restores_exact_state() {
        let mut s = Store::new(Limits::default());
        add_node(&mut s, "a", &["X"]);
        add_node(&mut s, "b", &["Y"]);
        s.apply(&Op::CreateIndex { index: IndexDef::new(IndexKind::Hash, IndexTarget::Node, "p") }).expect("idx");
        s.apply(&Op::AddEdge { edge: Edge::new("e", "a", "b", "T") }).expect("edge");
        let before = s.clone();
        let ops = vec![
            Op::SetNodeProperty { id: "a".into(), key: "p".into(), value: Value::Int(1) },
            Op::AddLabel { id: "a".into(), label: "Z".into() },
            Op::DeleteEdge { id: "e".into() },
            Op::DeleteNode { id: "b".into() },
            Op::AddNode { node: Node::new("c") },
            Op::RemoveLabel { id: "a".into(), label: "X".into() },
            Op::DropIndex { index: IndexDef::new(IndexKind::Hash, IndexTarget::Node, "p") },
        ];
        let mut undos = Vec::new();
        for op in &ops {
            undos.push(s.apply(op).expect("apply"));
        }
        assert!(s.verify().is_empty());
        for u in undos.into_iter().rev() {
            s.undo(u);
        }
        assert!(s.verify().is_empty());
        assert_eq!(s.nodes, before.nodes);
        assert_eq!(s.edges, before.edges);
        assert_eq!(s.out_adj, before.out_adj);
        assert_eq!(s.index_defs(), before.index_defs());
    }

    #[test]
    fn rejects_invalid_mutations_without_side_effects() {
        let mut s = Store::new(Limits::default());
        add_node(&mut s, "a", &[]);
        assert!(matches!(s.apply(&Op::AddNode { node: Node::new("a") }), Err(GraphError::DuplicateId { .. })));
        assert!(matches!(
            s.apply(&Op::AddEdge { edge: Edge::new("e", "a", "zzz", "T") }),
            Err(GraphError::DanglingEdge { .. })
        ));
        s.apply(&Op::AddEdge { edge: Edge::new("e", "a", "a", "T") }).expect("loop");
        assert!(matches!(s.apply(&Op::DeleteNode { id: "a".into() }), Err(GraphError::NodeHasEdges { .. })));
        assert!(s.apply(&Op::AddNode { node: Node::new("b").with_property("id", 1) }).is_err());
        assert!(s.apply(&Op::AddNode { node: Node::new("") }).is_err());
        assert_eq!(s.node_count(), 1);
        assert_eq!(s.edge_count(), 1);
        assert!(s.verify().is_empty());
    }
}
