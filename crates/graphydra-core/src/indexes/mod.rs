//! Secondary property indexes (hash, sorted, full-text).
//!
//! Every index is *derived data*: it is rebuilt from records on open and
//! maintained incrementally on every mutation. Indexes return a superset of
//! matching records; the executor always re-applies the original predicate,
//! so indexed and unindexed execution are equivalent by construction.

pub(crate) mod key;
pub mod text;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Bound;

use crate::model::{IndexDef, IndexKind};
use crate::value::Value;
pub(crate) use key::IndexKey;
pub(crate) use text::TextIndex;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum IndexData {
    Hash(HashMap<IndexKey, BTreeSet<u32>>),
    Sorted(BTreeMap<IndexKey, BTreeSet<u32>>),
    Text(TextIndex),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PropertyIndex {
    pub(crate) def: IndexDef,
    pub(crate) data: IndexData,
    /// Number of indexed entries (records with an indexable value).
    pub(crate) entries: usize,
}

impl PropertyIndex {
    pub(crate) fn new(def: IndexDef) -> Self {
        let data = match def.kind {
            IndexKind::Hash => IndexData::Hash(HashMap::new()),
            IndexKind::Sorted => IndexData::Sorted(BTreeMap::new()),
            IndexKind::Text => IndexData::Text(TextIndex::default()),
        };
        PropertyIndex { def, data, entries: 0 }
    }

    pub(crate) fn insert(&mut self, slot: u32, value: &Value) {
        match &mut self.data {
            IndexData::Hash(m) => {
                if let Some(k) = IndexKey::from_value(value)
                    && m.entry(k).or_default().insert(slot)
                {
                    self.entries += 1;
                }
            }
            IndexData::Sorted(m) => {
                if let Some(k) = IndexKey::from_value(value)
                    && m.entry(k).or_default().insert(slot)
                {
                    self.entries += 1;
                }
            }
            IndexData::Text(t) => {
                if !text::value_terms(value).is_empty() {
                    self.entries += 1;
                }
                t.insert(slot, value);
            }
        }
    }

    pub(crate) fn remove(&mut self, slot: u32, value: &Value) {
        fn remove_from<M>(m: &mut M, k: &IndexKey, slot: u32) -> bool
        where
            M: PostingMap,
        {
            m.remove_slot(k, slot)
        }
        match &mut self.data {
            IndexData::Hash(m) => {
                if let Some(k) = IndexKey::from_value(value)
                    && remove_from(m, &k, slot)
                {
                    self.entries -= 1;
                }
            }
            IndexData::Sorted(m) => {
                if let Some(k) = IndexKey::from_value(value)
                    && remove_from(m, &k, slot)
                {
                    self.entries -= 1;
                }
            }
            IndexData::Text(t) => {
                if !text::value_terms(value).is_empty() {
                    self.entries -= 1;
                }
                t.remove(slot, value);
            }
        }
    }

    /// Slots whose value equals `key` (ascending). `None` if this index
    /// kind cannot answer equality.
    pub(crate) fn lookup_eq(&self, key: &IndexKey) -> Option<Vec<u32>> {
        match &self.data {
            IndexData::Hash(m) => Some(m.get(key).map(|s| s.iter().copied().collect()).unwrap_or_default()),
            IndexData::Sorted(m) => Some(m.get(key).map(|s| s.iter().copied().collect()).unwrap_or_default()),
            IndexData::Text(_) => None,
        }
    }

    /// Range scan restricted to the comparison band of the bounds.
    /// Returns ascending, de-duplicated slots, or `None` for non-sorted indexes.
    pub(crate) fn lookup_range(&self, lower: Bound<&IndexKey>, upper: Bound<&IndexKey>) -> Option<Vec<u32>> {
        let IndexData::Sorted(m) = &self.data else { return None };
        let reference = match (lower, upper) {
            (Bound::Included(k) | Bound::Excluded(k), _) | (_, Bound::Included(k) | Bound::Excluded(k)) => k.clone(),
            (Bound::Unbounded, Bound::Unbounded) => return Some(Vec::new()),
        };
        let band_min = reference.band_min();
        let lo = match lower {
            Bound::Unbounded => Bound::Included(&band_min),
            b => b,
        };
        if let (Bound::Included(a) | Bound::Excluded(a), Bound::Included(b) | Bound::Excluded(b)) = (lo, upper)
            && (a > b || (a == b && !(matches!(lo, Bound::Included(_)) && matches!(upper, Bound::Included(_)))))
        {
            return Some(Vec::new());
        }
        let mut out: Vec<u32> = Vec::new();
        for (k, slots) in m.range::<IndexKey, _>((lo, upper)) {
            if !k.same_band(&reference) {
                break;
            }
            out.extend(slots.iter().copied());
        }
        out.sort_unstable();
        out.dedup();
        Some(out)
    }

    /// Prefix scan over string keys (`STARTS WITH`).
    pub(crate) fn lookup_prefix(&self, prefix: &str) -> Option<Vec<u32>> {
        let IndexData::Sorted(m) = &self.data else { return None };
        let start = IndexKey::Str(prefix.to_owned());
        let mut out: Vec<u32> = Vec::new();
        for (k, slots) in m.range(start..) {
            match k {
                IndexKey::Str(s) if s.starts_with(prefix) => out.extend(slots.iter().copied()),
                _ => break,
            }
        }
        out.sort_unstable();
        out.dedup();
        Some(out)
    }

    pub(crate) fn text(&self) -> Option<&TextIndex> {
        match &self.data {
            IndexData::Text(t) => Some(t),
            _ => None,
        }
    }

    /// Number of distinct keys (or terms for text indexes).
    pub(crate) fn distinct_keys(&self) -> usize {
        match &self.data {
            IndexData::Hash(m) => m.len(),
            IndexData::Sorted(m) => m.len(),
            IndexData::Text(t) => t.term_count(),
        }
    }
}

trait PostingMap {
    fn remove_slot(&mut self, k: &IndexKey, slot: u32) -> bool;
}

impl PostingMap for HashMap<IndexKey, BTreeSet<u32>> {
    fn remove_slot(&mut self, k: &IndexKey, slot: u32) -> bool {
        let Some(set) = self.get_mut(k) else { return false };
        let removed = set.remove(&slot);
        if set.is_empty() {
            self.remove(k);
        }
        removed
    }
}

impl PostingMap for BTreeMap<IndexKey, BTreeSet<u32>> {
    fn remove_slot(&mut self, k: &IndexKey, slot: u32) -> bool {
        let Some(set) = self.get_mut(k) else { return false };
        let removed = set.remove(&slot);
        if set.is_empty() {
            self.remove(k);
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::IndexTarget;

    fn sorted_index(values: &[Value]) -> PropertyIndex {
        let mut idx = PropertyIndex::new(IndexDef::new(IndexKind::Sorted, IndexTarget::Node, "x"));
        for (i, v) in values.iter().enumerate() {
            idx.insert(i as u32, v);
        }
        idx
    }

    #[test]
    fn range_stays_in_band() {
        let idx =
            sorted_index(&[Value::Int(1), Value::Float(2.5), Value::from("3"), Value::Bool(true), Value::Int(10)]);
        let gt2 = IndexKey::Int(2);
        assert_eq!(idx.lookup_range(Bound::Excluded(&gt2), Bound::Unbounded), Some(vec![1, 4]));
        assert_eq!(idx.lookup_range(Bound::Unbounded, Bound::Included(&gt2)), Some(vec![0]));
        let s = IndexKey::Str(String::new());
        assert_eq!(idx.lookup_range(Bound::Included(&s), Bound::Unbounded), Some(vec![2]));
        let (a, b) = (IndexKey::Int(5), IndexKey::Int(1));
        assert_eq!(idx.lookup_range(Bound::Included(&a), Bound::Included(&b)), Some(vec![]));
    }

    #[test]
    fn insert_remove_tracks_entries() {
        let mut idx = sorted_index(&[Value::Int(1), Value::Int(1), Value::Null]);
        assert_eq!(idx.entries, 2);
        assert_eq!(idx.lookup_eq(&IndexKey::Int(1)), Some(vec![0, 1]));
        idx.remove(0, &Value::Float(1.0));
        assert_eq!(idx.lookup_eq(&IndexKey::Int(1)), Some(vec![1]));
        assert_eq!(idx.entries, 1);
    }

    #[test]
    fn prefix_scan() {
        let idx = sorted_index(&[Value::from("work-1"), Value::from("work-2"), Value::from("wx"), Value::from("a")]);
        assert_eq!(idx.lookup_prefix("work"), Some(vec![0, 1]));
        assert_eq!(idx.lookup_prefix(""), Some(vec![0, 1, 2, 3]));
    }
}
