//! Hashable, totally ordered keys for property indexes.

use std::cmp::Ordering;
use std::hash::{Hash, Hasher};

use crate::value::{Value, cmp_int_float};

/// Normalised scalar used as an index key.
///
/// Numbers are normalised so that numerically equal values share a key:
/// an integral float within the `i64` range becomes `Int`. Keys order as
/// booleans < numbers < strings, matching query comparison "bands".
#[derive(Debug, Clone)]
pub(crate) enum IndexKey {
    Bool(bool),
    Int(i64),
    /// Always finite and either non-integral or outside the `i64` range.
    Float(f64),
    Str(String),
}

impl IndexKey {
    /// Key for a value, or `None` if the value is not indexable
    /// (null, lists and maps are not indexed).
    pub(crate) fn from_value(v: &Value) -> Option<IndexKey> {
        match v {
            Value::Bool(b) => Some(IndexKey::Bool(*b)),
            Value::Int(i) => Some(IndexKey::Int(*i)),
            Value::Float(f) => Some(Self::from_float(*f)),
            Value::String(s) => Some(IndexKey::Str(s.clone())),
            _ => None,
        }
    }

    fn from_float(f: f64) -> IndexKey {
        const TWO_63: f64 = 9_223_372_036_854_775_808.0;
        if f.fract() == 0.0 && (-TWO_63..TWO_63).contains(&f) {
            // Exact: integral and in range. Also folds -0.0 into 0.
            IndexKey::Int(f as i64)
        } else {
            IndexKey::Float(f)
        }
    }

    fn band(&self) -> u8 {
        match self {
            IndexKey::Bool(_) => 0,
            IndexKey::Int(_) | IndexKey::Float(_) => 1,
            IndexKey::Str(_) => 2,
        }
    }

    /// Smallest key of the same comparison band (for open-ended ranges).
    pub(crate) fn band_min(&self) -> IndexKey {
        match self.band() {
            0 => IndexKey::Bool(false),
            1 => IndexKey::Float(f64::MIN),
            _ => IndexKey::Str(String::new()),
        }
    }

    /// Returns true if `other` belongs to the same comparison band.
    pub(crate) fn same_band(&self, other: &IndexKey) -> bool {
        self.band() == other.band()
    }
}

impl Ord for IndexKey {
    fn cmp(&self, other: &Self) -> Ordering {
        use IndexKey::*;
        match (self, other) {
            (Bool(a), Bool(b)) => a.cmp(b),
            (Int(a), Int(b)) => a.cmp(b),
            (Float(a), Float(b)) => a.total_cmp(b),
            (Int(a), Float(b)) => cmp_int_float(*a, *b),
            (Float(a), Int(b)) => cmp_int_float(*b, *a).reverse(),
            (Str(a), Str(b)) => a.cmp(b),
            _ => self.band().cmp(&other.band()),
        }
    }
}

impl PartialOrd for IndexKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for IndexKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for IndexKey {}

impl Hash for IndexKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Consistent with Eq: after normalisation, Int and Float never compare
        // equal to each other, so hashing the variant is sound.
        match self {
            IndexKey::Bool(b) => (0u8, *b).hash(state),
            IndexKey::Int(i) => (1u8, *i).hash(state),
            IndexKey::Float(f) => (2u8, f.to_bits()).hash(state),
            IndexKey::Str(s) => {
                3u8.hash(state);
                s.hash(state);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_integral_floats() {
        assert_eq!(IndexKey::from_value(&Value::Float(3.0)), IndexKey::from_value(&Value::Int(3)));
        assert_eq!(IndexKey::from_value(&Value::Float(-0.0)), Some(IndexKey::Int(0)));
        assert!(matches!(IndexKey::from_value(&Value::Float(2.5)), Some(IndexKey::Float(_))));
        assert!(matches!(IndexKey::from_value(&Value::Float(1e19)), Some(IndexKey::Float(_))));
        assert!(IndexKey::from_value(&Value::Null).is_none());
    }

    #[test]
    fn orders_by_band_then_value() {
        let mut keys = vec![
            IndexKey::Str("a".into()),
            IndexKey::Float(2.5),
            IndexKey::Int(2),
            IndexKey::Bool(true),
            IndexKey::Int(3),
        ];
        keys.sort();
        assert_eq!(
            keys,
            vec![
                IndexKey::Bool(true),
                IndexKey::Int(2),
                IndexKey::Float(2.5),
                IndexKey::Int(3),
                IndexKey::Str("a".into())
            ]
        );
        assert!(IndexKey::Float(f64::MIN) < IndexKey::Int(i64::MIN));
    }
}
