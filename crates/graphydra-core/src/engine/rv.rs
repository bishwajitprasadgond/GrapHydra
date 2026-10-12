//! Runtime values used during query execution.
//!
//! Unlike [`Value`], runtime values can reference graph entities by slot.
//! They are converted to owned [`ResultValue`]s only when results are
//! produced.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

use crate::engine::result::ResultValue;
use crate::model::Path;
use crate::store::{Slot, Store};
use crate::value::{Number, Value};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Rv {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<Rv>),
    Map(BTreeMap<String, Rv>),
    Node(Slot),
    Edge(Slot),
    Path(Box<PathRv>),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PathRv {
    pub(crate) nodes: Vec<Slot>,
    pub(crate) edges: Vec<Slot>,
}

impl Rv {
    pub(crate) fn from_value(v: &Value) -> Rv {
        match v {
            Value::Null => Rv::Null,
            Value::Bool(b) => Rv::Bool(*b),
            Value::Int(i) => Rv::Int(*i),
            Value::Float(f) => Rv::Float(*f),
            Value::String(s) => Rv::Str(s.clone()),
            Value::List(items) => Rv::List(items.iter().map(Rv::from_value).collect()),
            Value::Map(m) => Rv::Map(m.iter().map(|(k, v)| (k.clone(), Rv::from_value(v))).collect()),
        }
    }

    /// Convert to a storable property value. Entities cannot be stored.
    pub(crate) fn to_value(&self) -> Option<Value> {
        Some(match self {
            Rv::Null => Value::Null,
            Rv::Bool(b) => Value::Bool(*b),
            Rv::Int(i) => Value::Int(*i),
            Rv::Float(f) => Value::Float(*f),
            Rv::Str(s) => Value::String(s.clone()),
            Rv::List(items) => Value::List(items.iter().map(Rv::to_value).collect::<Option<_>>()?),
            Rv::Map(m) => Value::Map(m.iter().map(|(k, v)| Some((k.clone(), v.to_value()?))).collect::<Option<_>>()?),
            Rv::Node(_) | Rv::Edge(_) | Rv::Path(_) => return None,
        })
    }

    pub(crate) fn to_result(&self, store: &Store) -> ResultValue {
        match self {
            Rv::Null => ResultValue::Null,
            Rv::Bool(b) => ResultValue::Bool(*b),
            Rv::Int(i) => ResultValue::Int(*i),
            Rv::Float(f) => ResultValue::Float(*f),
            Rv::Str(s) => ResultValue::String(s.clone()),
            Rv::List(items) => ResultValue::List(items.iter().map(|i| i.to_result(store)).collect()),
            Rv::Map(m) => ResultValue::Map(m.iter().map(|(k, v)| (k.clone(), v.to_result(store))).collect()),
            Rv::Node(s) => store.node_to_public(*s).map_or(ResultValue::Null, ResultValue::Node),
            Rv::Edge(s) => store.edge_to_public(*s).map_or(ResultValue::Null, ResultValue::Edge),
            Rv::Path(p) => {
                let nodes: Option<Vec<_>> = p.nodes.iter().map(|n| store.node_to_public(*n)).collect();
                let edges: Option<Vec<_>> = p.edges.iter().map(|e| store.edge_to_public(*e)).collect();
                match (nodes, edges) {
                    (Some(nodes), Some(edges)) => ResultValue::Path(Path { nodes, edges }),
                    _ => ResultValue::Null,
                }
            }
        }
    }

    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            Rv::Null => "null",
            Rv::Bool(_) => "boolean",
            Rv::Int(_) => "integer",
            Rv::Float(_) => "float",
            Rv::Str(_) => "string",
            Rv::List(_) => "list",
            Rv::Map(_) => "map",
            Rv::Node(_) => "node",
            Rv::Edge(_) => "edge",
            Rv::Path(_) => "path",
        }
    }

    pub(crate) fn is_null(&self) -> bool {
        matches!(self, Rv::Null)
    }

    fn as_number(&self) -> Option<Number> {
        match self {
            Rv::Int(i) => Some(Number::Int(*i)),
            Rv::Float(f) => Some(Number::Float(*f)),
            _ => None,
        }
    }

    /// `=` with three-valued logic (see [`Value::query_eq`]).
    pub(crate) fn query_eq(&self, other: &Rv) -> Option<bool> {
        match (self, other) {
            (Rv::Null, _) | (_, Rv::Null) => None,
            (Rv::Bool(a), Rv::Bool(b)) => Some(a == b),
            (Rv::Str(a), Rv::Str(b)) => Some(a == b),
            (Rv::Node(a), Rv::Node(b)) | (Rv::Edge(a), Rv::Edge(b)) => Some(a == b),
            (Rv::Path(a), Rv::Path(b)) => Some(a == b),
            (Rv::List(a), Rv::List(b)) => {
                if a.len() != b.len() {
                    return Some(false);
                }
                let mut unknown = false;
                for (x, y) in a.iter().zip(b) {
                    match x.query_eq(y) {
                        Some(false) => return Some(false),
                        None => unknown = true,
                        Some(true) => {}
                    }
                }
                if unknown { None } else { Some(true) }
            }
            (Rv::Map(a), Rv::Map(b)) => {
                if a.len() != b.len() || a.keys().ne(b.keys()) {
                    return Some(false);
                }
                let mut unknown = false;
                for (x, y) in a.values().zip(b.values()) {
                    match x.query_eq(y) {
                        Some(false) => return Some(false),
                        None => unknown = true,
                        Some(true) => {}
                    }
                }
                if unknown { None } else { Some(true) }
            }
            _ => match (self.as_number(), other.as_number()) {
                (Some(a), Some(b)) => Some(a == b),
                _ => Some(false),
            },
        }
    }

    /// `<`-family comparison: only numbers, strings and booleans.
    pub(crate) fn query_cmp(&self, other: &Rv) -> Option<Ordering> {
        match (self, other) {
            (Rv::Str(a), Rv::Str(b)) => Some(a.cmp(b)),
            (Rv::Bool(a), Rv::Bool(b)) => Some(a.cmp(b)),
            _ => match (self.as_number(), other.as_number()) {
                (Some(a), Some(b)) => Some(a.cmp(&b)),
                _ => None,
            },
        }
    }

    /// Total order for ORDER BY / min / max. Entities order by id.
    pub(crate) fn total_cmp(&self, other: &Rv, store: &Store) -> Ordering {
        fn rank(v: &Rv) -> u8 {
            match v {
                Rv::Bool(_) => 0,
                Rv::Int(_) | Rv::Float(_) => 1,
                Rv::Str(_) => 2,
                Rv::List(_) => 3,
                Rv::Map(_) => 4,
                Rv::Node(_) => 5,
                Rv::Edge(_) => 6,
                Rv::Path(_) => 7,
                Rv::Null => 9,
            }
        }
        match (self, other) {
            (Rv::Bool(a), Rv::Bool(b)) => a.cmp(b),
            (Rv::Str(a), Rv::Str(b)) => a.cmp(b),
            (Rv::List(a), Rv::List(b)) => {
                for (x, y) in a.iter().zip(b) {
                    let c = x.total_cmp(y, store);
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                a.len().cmp(&b.len())
            }
            (Rv::Map(a), Rv::Map(b)) => {
                for ((ka, va), (kb, vb)) in a.iter().zip(b) {
                    let c = ka.cmp(kb).then_with(|| va.total_cmp(vb, store));
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                a.len().cmp(&b.len())
            }
            (Rv::Node(a), Rv::Node(b)) => {
                let ida = store.node(*a).map(|n| &*n.id);
                let idb = store.node(*b).map(|n| &*n.id);
                ida.cmp(&idb)
            }
            (Rv::Edge(a), Rv::Edge(b)) => {
                let ida = store.edge(*a).map(|e| &*e.id);
                let idb = store.edge(*b).map(|e| &*e.id);
                ida.cmp(&idb)
            }
            (Rv::Path(a), Rv::Path(b)) => {
                let la: Vec<Rv> = a.edges.iter().map(|e| Rv::Edge(*e)).collect();
                let lb: Vec<Rv> = b.edges.iter().map(|e| Rv::Edge(*e)).collect();
                Rv::List(la).total_cmp(&Rv::List(lb), store)
            }
            _ => match (self.as_number(), other.as_number()) {
                (Some(a), Some(b)) => a.cmp(&b),
                _ => rank(self).cmp(&rank(other)),
            },
        }
    }
}

/// Hashable grouping key with the same equality as `=` on non-null values
/// (numbers compare numerically; nulls group together, as in SQL/Cypher).
#[derive(Debug, Clone)]
pub(crate) struct GroupKey(pub(crate) Rv);

impl PartialEq for GroupKey {
    fn eq(&self, other: &Self) -> bool {
        group_eq(&self.0, &other.0)
    }
}

impl Eq for GroupKey {}

fn group_eq(a: &Rv, b: &Rv) -> bool {
    match (a, b) {
        (Rv::Null, Rv::Null) => true,
        (Rv::List(x), Rv::List(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| group_eq(p, q)),
        (Rv::Map(x), Rv::Map(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|((ka, va), (kb, vb))| ka == kb && group_eq(va, vb))
        }
        _ => a.query_eq(b) == Some(true),
    }
}

impl Hash for GroupKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        hash_rv(&self.0, state);
    }
}

fn hash_rv<H: Hasher>(v: &Rv, state: &mut H) {
    match v {
        Rv::Null => 0u8.hash(state),
        Rv::Bool(b) => (1u8, *b).hash(state),
        // Numbers that are equal must hash equally: hash integral values as
        // integers and everything else by bit pattern.
        Rv::Int(i) => (2u8, *i).hash(state),
        Rv::Float(f) => {
            const TWO_63: f64 = 9_223_372_036_854_775_808.0;
            if f.fract() == 0.0 && (-TWO_63..TWO_63).contains(f) {
                (2u8, *f as i64).hash(state);
            } else {
                (3u8, f.to_bits()).hash(state);
            }
        }
        Rv::Str(s) => {
            4u8.hash(state);
            s.hash(state);
        }
        Rv::List(items) => {
            5u8.hash(state);
            items.len().hash(state);
            for i in items {
                hash_rv(i, state);
            }
        }
        Rv::Map(m) => {
            6u8.hash(state);
            m.len().hash(state);
            for (k, v) in m {
                k.hash(state);
                hash_rv(v, state);
            }
        }
        Rv::Node(s) => (7u8, *s).hash(state),
        Rv::Edge(s) => (8u8, *s).hash(state),
        Rv::Path(p) => {
            9u8.hash(state);
            p.hash(state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn group_keys_unify_numbers_and_nulls() {
        let mut set = HashSet::new();
        set.insert(GroupKey(Rv::Int(1)));
        set.insert(GroupKey(Rv::Float(1.0)));
        set.insert(GroupKey(Rv::Null));
        set.insert(GroupKey(Rv::Null));
        set.insert(GroupKey(Rv::List(vec![Rv::Int(2), Rv::Null])));
        set.insert(GroupKey(Rv::List(vec![Rv::Float(2.0), Rv::Null])));
        assert_eq!(set.len(), 3);
    }
}
