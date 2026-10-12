//! Property values.
//!
//! The supported value types are exactly those of the `.graph` 1.x format:
//! null, booleans, 64-bit signed integers, *finite* 64-bit floats, UTF-8
//! strings, lists and string-keyed maps. Non-finite floats can never be
//! constructed through the public parsing paths and are rejected on insert.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A property value.
///
/// `PartialEq` is *structural* (`Int(1) != Float(1.0)`); query semantics use
/// [`Value::query_eq`], which compares numbers numerically.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
}

/// Map of property names to values. Sorted keys give canonical output.
pub type Properties = BTreeMap<String, Value>;

impl Value {
    /// Human-readable type name used in error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Int(_) => "integer",
            Value::Float(_) => "float",
            Value::String(_) => "string",
            Value::List(_) => "list",
            Value::Map(_) => "map",
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// Nesting depth: scalars are 0, `[1]` is 1, `[[1]]` is 2.
    pub fn depth(&self) -> usize {
        match self {
            Value::List(items) => 1 + items.iter().map(Value::depth).max().unwrap_or(0),
            Value::Map(map) => 1 + map.values().map(Value::depth).max().unwrap_or(0),
            _ => 0,
        }
    }

    /// Returns true when every float in the value is finite.
    pub fn is_finite(&self) -> bool {
        match self {
            Value::Float(f) => f.is_finite(),
            Value::List(items) => items.iter().all(Value::is_finite),
            Value::Map(map) => map.values().all(Value::is_finite),
            _ => true,
        }
    }

    /// Approximate in-memory size in bytes, used for limit checks.
    pub fn approx_size(&self) -> usize {
        match self {
            Value::String(s) => s.len(),
            Value::List(items) => items.iter().map(Value::approx_size).sum::<usize>() + 8,
            Value::Map(map) => map.iter().map(|(k, v)| k.len() + v.approx_size()).sum::<usize>() + 8,
            _ => 8,
        }
    }

    /// Query-language equality with three-valued logic.
    ///
    /// Returns `None` ("unknown") when either side is null or when a nested
    /// comparison is unknown. Numbers compare numerically across int/float.
    /// Values of different types are simply unequal.
    pub fn query_eq(&self, other: &Value) -> Option<bool> {
        match (self, other) {
            (Value::Null, _) | (_, Value::Null) => None,
            (Value::Bool(a), Value::Bool(b)) => Some(a == b),
            (Value::String(a), Value::String(b)) => Some(a == b),
            (Value::List(a), Value::List(b)) => {
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
            (Value::Map(a), Value::Map(b)) => {
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
                (Some(a), Some(b)) => Some(a.cmp(&b) == Ordering::Equal),
                _ => Some(false),
            },
        }
    }

    /// Ordering comparison for `<`, `<=`, `>`, `>=`.
    ///
    /// Only numbers, strings and booleans are comparable with values of the
    /// same kind; every other combination yields `None` (null in queries).
    pub fn query_cmp(&self, other: &Value) -> Option<Ordering> {
        match (self, other) {
            (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
            (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
            _ => match (self.as_number(), other.as_number()) {
                (Some(a), Some(b)) => Some(a.cmp(&b)),
                _ => None,
            },
        }
    }

    pub(crate) fn as_number(&self) -> Option<Number> {
        match self {
            Value::Int(i) => Some(Number::Int(*i)),
            Value::Float(f) => Some(Number::Float(*f)),
            _ => None,
        }
    }

    /// Total order used by `ORDER BY` and for deterministic output.
    ///
    /// Ascending order is: booleans, numbers, strings, lists, maps, and null
    /// last. Within a type the natural order applies; lists and maps compare
    /// lexicographically.
    pub fn total_cmp(&self, other: &Value) -> Ordering {
        fn rank(v: &Value) -> u8 {
            match v {
                Value::Bool(_) => 0,
                Value::Int(_) | Value::Float(_) => 1,
                Value::String(_) => 2,
                Value::List(_) => 3,
                Value::Map(_) => 4,
                Value::Null => 9,
            }
        }
        match (self, other) {
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            (Value::String(a), Value::String(b)) => a.cmp(b),
            (Value::List(a), Value::List(b)) => {
                for (x, y) in a.iter().zip(b) {
                    let c = x.total_cmp(y);
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                a.len().cmp(&b.len())
            }
            (Value::Map(a), Value::Map(b)) => {
                for ((ka, va), (kb, vb)) in a.iter().zip(b) {
                    let c = ka.cmp(kb).then_with(|| va.total_cmp(vb));
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                a.len().cmp(&b.len())
            }
            _ => match (self.as_number(), other.as_number()) {
                (Some(a), Some(b)) => a.cmp(&b),
                _ => rank(self).cmp(&rank(other)),
            },
        }
    }
}

/// A numeric value with exact cross-type comparison.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Number {
    Int(i64),
    Float(f64),
}

/// Compare an integer with a finite float without precision loss.
pub(crate) fn cmp_int_float(i: i64, f: f64) -> Ordering {
    // 2^63 is exactly representable; every i64 is strictly below it.
    const TWO_63: f64 = 9_223_372_036_854_775_808.0;
    if f >= TWO_63 {
        return Ordering::Less;
    }
    if f < -TWO_63 {
        return Ordering::Greater;
    }
    let fl = f.floor();
    // In range [-2^63, 2^63), so the cast is exact.
    let fi = fl as i64;
    match i.cmp(&fi) {
        Ordering::Equal if f > fl => Ordering::Less,
        other => other,
    }
}

impl Ord for Number {
    fn cmp(&self, other: &Self) -> Ordering {
        match (*self, *other) {
            (Number::Int(a), Number::Int(b)) => a.cmp(&b),
            (Number::Float(a), Number::Float(b)) => a.total_cmp(&b),
            (Number::Int(a), Number::Float(b)) => cmp_int_float(a, b),
            (Number::Float(a), Number::Int(b)) => cmp_int_float(b, a).reverse(),
        }
    }
}

impl PartialOrd for Number {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Number {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Number {}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match serde_json::to_string(self) {
            Ok(s) => f.write_str(&s),
            Err(_) => f.write_str("<unprintable>"),
        }
    }
}

macro_rules! impl_from {
    ($($t:ty => $variant:ident),* $(,)?) => {
        $(impl From<$t> for Value {
            fn from(v: $t) -> Self { Value::$variant(v.into()) }
        })*
    };
}

impl_from!(bool => Bool, i64 => Int, i32 => Int, u32 => Int, f64 => Float, String => String, &str => String);

impl<T: Into<Value>> From<Vec<T>> for Value {
    fn from(v: Vec<T>) -> Self {
        Value::List(v.into_iter().map(Into::into).collect())
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        v.map_or(Value::Null, Into::into)
    }
}

impl From<BTreeMap<String, Value>> for Value {
    fn from(v: BTreeMap<String, Value>) -> Self {
        Value::Map(v)
    }
}

impl Serialize for Value {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Value::Null => serializer.serialize_unit(),
            Value::Bool(b) => serializer.serialize_bool(*b),
            Value::Int(i) => serializer.serialize_i64(*i),
            Value::Float(f) => {
                if !f.is_finite() {
                    return Err(serde::ser::Error::custom("non-finite float cannot be serialized"));
                }
                serializer.serialize_f64(*f)
            }
            Value::String(s) => serializer.serialize_str(s),
            Value::List(items) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Value::Map(map) => {
                let mut m = serializer.serialize_map(Some(map.len()))?;
                for (k, v) in map {
                    m.serialize_entry(k, v)?;
                }
                m.end()
            }
        }
    }
}

/// Strict deserializer: rejects duplicate map keys, integers outside the
/// `i64` range and non-finite floats.
impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ValueVisitor)
    }
}

struct ValueVisitor;

impl<'de> Visitor<'de> for ValueVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        Deserialize::deserialize(d)
    }

    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
        Ok(Value::Int(v))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        i64::try_from(v)
            .map(Value::Int)
            .map_err(|_| E::custom(format!("integer {v} is outside the supported 64-bit signed range")))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        if v.is_finite() { Ok(Value::Float(v)) } else { Err(E::custom("non-finite numbers are not supported")) }
    }

    fn visit_str<E>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_owned()))
    }

    fn visit_string<E>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(4096));
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Value::List(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
        Ok(Value::Map(StrictMap::visit(&mut access)?))
    }
}

/// Helper that deserializes a JSON object into a map, rejecting duplicates.
pub(crate) struct StrictMap;

impl StrictMap {
    fn visit<'de, A: MapAccess<'de>>(access: &mut A) -> Result<BTreeMap<String, Value>, A::Error> {
        let mut map = BTreeMap::new();
        while let Some(key) = access.next_key::<String>()? {
            let value: Value = access.next_value()?;
            if map.contains_key(&key) {
                return Err(de::Error::custom(format!("duplicate key {key:?}")));
            }
            map.insert(key, value);
        }
        Ok(map)
    }
}

/// Deserialize a properties object strictly (used with `#[serde(deserialize_with)]`).
pub(crate) fn deserialize_properties<'de, D: Deserializer<'de>>(d: D) -> Result<Properties, D::Error> {
    struct PropsVisitor;
    impl<'de> Visitor<'de> for PropsVisitor {
        type Value = Properties;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a JSON object of properties")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Properties, A::Error> {
            StrictMap::visit(&mut access)
        }
    }
    d.deserialize_map(PropsVisitor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_float_comparison_is_exact() {
        assert_eq!(cmp_int_float(1, 1.0), Ordering::Equal);
        assert_eq!(cmp_int_float(1, 1.5), Ordering::Less);
        assert_eq!(cmp_int_float(2, 1.5), Ordering::Greater);
        assert_eq!(cmp_int_float(-2, -1.5), Ordering::Less);
        assert_eq!(cmp_int_float(i64::MAX, 9.3e18), Ordering::Less);
        assert_eq!(cmp_int_float(i64::MIN, -9.3e18), Ordering::Greater);
        // i64::MAX is not representable as f64; nearest double is 2^63.
        assert_eq!(cmp_int_float(i64::MAX, i64::MAX as f64), Ordering::Less);
        assert_eq!(cmp_int_float(i64::MIN, i64::MIN as f64), Ordering::Equal);
    }

    #[test]
    fn query_eq_three_valued() {
        assert_eq!(Value::Int(1).query_eq(&Value::Float(1.0)), Some(true));
        assert_eq!(Value::Int(1).query_eq(&Value::from("1")), Some(false));
        assert_eq!(Value::Null.query_eq(&Value::Null), None);
        let a = Value::from(vec![Value::Int(1), Value::Null]);
        let b = Value::from(vec![Value::Int(1), Value::Int(2)]);
        assert_eq!(a.query_eq(&b), None);
        let c = Value::from(vec![Value::Int(2), Value::Null]);
        assert_eq!(a.query_eq(&c), Some(false));
    }

    #[test]
    fn strict_json_rejects_duplicates_and_overflow() {
        assert!(serde_json::from_str::<Value>(r#"{"a":1,"a":2}"#).is_err());
        assert!(serde_json::from_str::<Value>("18446744073709551615").is_err());
        assert!(serde_json::from_str::<Value>("1e400").is_err());
        let v: Value = serde_json::from_str(r#"{"a":[1,2.5,"x",null,true]}"#).expect("valid");
        assert_eq!(v.depth(), 2);
    }

    #[test]
    fn total_order_puts_null_last() {
        let mut v = vec![Value::Null, Value::from("a"), Value::Int(3), Value::Float(2.5), Value::Bool(true)];
        v.sort_by(Value::total_cmp);
        assert_eq!(v, vec![Value::Bool(true), Value::Float(2.5), Value::Int(3), Value::from("a"), Value::Null]);
    }
}
