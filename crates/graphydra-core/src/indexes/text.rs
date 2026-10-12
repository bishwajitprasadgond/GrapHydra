//! Inverted full-text index and the tokenizer shared by indexed and
//! unindexed text matching (so both paths return identical results).

use std::collections::{BTreeMap, HashMap};

use crate::value::Value;

/// Split text into normalised terms: Unicode-lowercased runs of
/// alphanumeric characters. Everything else is a separator.
pub fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).map(str::to_lowercase).collect()
}

/// Terms of an indexable value: strings and lists of strings.
pub(crate) fn value_terms(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => tokenize(s),
        Value::List(items) => items.iter().filter_map(Value::as_str).flat_map(tokenize).collect(),
        _ => Vec::new(),
    }
}

/// `text_match` semantics: true iff the query has at least one term and
/// every query term occurs in the value.
pub(crate) fn text_matches(v: &Value, query: &str) -> bool {
    let q = tokenize(query);
    if q.is_empty() {
        return false;
    }
    let terms = value_terms(v);
    q.iter().all(|t| terms.contains(t))
}

#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct TextIndex {
    /// term -> (record slot -> term frequency)
    postings: HashMap<String, BTreeMap<u32, u32>>,
    docs: usize,
}

impl TextIndex {
    pub(crate) fn insert(&mut self, slot: u32, v: &Value) {
        let terms = value_terms(v);
        if terms.is_empty() {
            return;
        }
        self.docs += 1;
        for t in terms {
            *self.postings.entry(t).or_default().entry(slot).or_insert(0) += 1;
        }
    }

    pub(crate) fn remove(&mut self, slot: u32, v: &Value) {
        let terms = value_terms(v);
        if terms.is_empty() {
            return;
        }
        self.docs = self.docs.saturating_sub(1);
        for t in terms {
            if let Some(p) = self.postings.get_mut(&t) {
                p.remove(&slot);
                if p.is_empty() {
                    self.postings.remove(&t);
                }
            }
        }
    }

    /// Slots containing all query terms, ascending. Empty query -> empty.
    pub(crate) fn lookup_all(&self, query: &str) -> Vec<u32> {
        let mut terms = tokenize(query);
        if terms.is_empty() {
            return Vec::new();
        }
        terms.sort();
        terms.dedup();
        let mut lists: Vec<&BTreeMap<u32, u32>> = Vec::with_capacity(terms.len());
        for t in &terms {
            match self.postings.get(t) {
                Some(p) => lists.push(p),
                None => return Vec::new(),
            }
        }
        lists.sort_by_key(|p| p.len());
        let (first, rest) = lists.split_first().expect("non-empty");
        first.keys().copied().filter(|s| rest.iter().all(|p| p.contains_key(s))).collect()
    }

    /// Ranked search: slots matching *any* term, scored by TF-IDF.
    /// Ties break on slot so ranking is deterministic.
    pub(crate) fn search_ranked(&self, query: &str) -> Vec<(u32, f64)> {
        let mut terms = tokenize(query);
        terms.sort();
        terms.dedup();
        let n = self.docs.max(1) as f64;
        let mut scores: BTreeMap<u32, f64> = BTreeMap::new();
        for t in &terms {
            if let Some(p) = self.postings.get(t) {
                let idf = (1.0 + n / p.len() as f64).ln();
                for (slot, tf) in p {
                    *scores.entry(*slot).or_insert(0.0) += (1.0 + f64::from(*tf).ln()) * idf;
                }
            }
        }
        let mut out: Vec<(u32, f64)> = scores.into_iter().collect();
        out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        out
    }

    pub(crate) fn term_count(&self) -> usize {
        self.postings.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizer_normalises() {
        assert_eq!(tokenize("Hello, WORLD! foo_bar 42x"), vec!["hello", "world", "foo", "bar", "42x"]);
        assert!(tokenize("  ,, ").is_empty());
        assert_eq!(tokenize("Ünïcode Straße"), vec!["ünïcode", "straße"]);
    }

    #[test]
    fn index_matches_unindexed_semantics() {
        let mut idx = TextIndex::default();
        let docs =
            [Value::from("Remote access trojan"), Value::from("trojan dropper"), Value::from(vec!["Access", "broker"])];
        for (i, d) in docs.iter().enumerate() {
            idx.insert(i as u32, d);
        }
        for q in ["trojan", "access", "ACCESS trojan", "nothing", "", "broker access"] {
            let expected: Vec<u32> =
                docs.iter().enumerate().filter(|(_, d)| text_matches(d, q)).map(|(i, _)| i as u32).collect();
            assert_eq!(idx.lookup_all(q), expected, "query {q:?}");
        }
        idx.remove(0, &docs[0]);
        assert_eq!(idx.lookup_all("trojan"), vec![1]);
        // Equal scores tie-break on slot.
        let ranked: Vec<u32> = idx.search_ranked("trojan access").iter().map(|r| r.0).collect();
        assert_eq!(ranked, vec![1, 2]);
    }
}
