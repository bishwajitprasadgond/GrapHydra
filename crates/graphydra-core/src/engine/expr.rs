//! Compiled expressions, the expression evaluator and built-in functions.
//!
//! The evaluator is a pure interpreter over [`CExpr`]: it cannot execute
//! code, touch the filesystem or the network. Nulls follow SQL/Cypher
//! three-valued logic.

use std::collections::BTreeMap;

use crate::engine::rv::{PathRv, Rv};
use crate::error::{GraphError, Result};
use crate::indexes::text::text_matches;
use crate::language::ast::{BinOp, Expr, UnOp, is_aggregate_name};
use crate::store::{Direction, Store};
use crate::value::Value;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CExpr {
    Const(Rv),
    Var(usize),
    /// Projected column (ORDER BY scope).
    Column(usize),
    /// Result of the k-th aggregate (post-aggregation scope).
    Agg(usize),
    Prop(Box<CExpr>, String),
    Index(Box<CExpr>, Box<CExpr>),
    List(Vec<CExpr>),
    Map(Vec<(String, CExpr)>),
    Not(Box<CExpr>),
    Neg(Box<CExpr>),
    Bin(BinOp, Box<CExpr>, Box<CExpr>),
    IsNull(Box<CExpr>, bool),
    Call(Func, Vec<CExpr>),
    Case {
        operand: Option<Box<CExpr>>,
        branches: Vec<(CExpr, CExpr)>,
        default: Option<Box<CExpr>>,
    },
}

impl CExpr {
    /// Row slots referenced by the expression.
    pub(crate) fn deps(&self, out: &mut Vec<usize>) {
        match self {
            CExpr::Var(v) => {
                if !out.contains(v) {
                    out.push(*v);
                }
            }
            CExpr::Const(_) | CExpr::Column(_) | CExpr::Agg(_) => {}
            CExpr::Prop(e, _) | CExpr::Not(e) | CExpr::Neg(e) | CExpr::IsNull(e, _) => e.deps(out),
            CExpr::Index(a, b) | CExpr::Bin(_, a, b) => {
                a.deps(out);
                b.deps(out);
            }
            CExpr::List(items) | CExpr::Call(_, items) => items.iter().for_each(|i| i.deps(out)),
            CExpr::Map(entries) => entries.iter().for_each(|(_, e)| e.deps(out)),
            CExpr::Case { operand, branches, default } => {
                if let Some(o) = operand {
                    o.deps(out);
                }
                for (w, t) in branches {
                    w.deps(out);
                    t.deps(out);
                }
                if let Some(d) = default {
                    d.deps(out);
                }
            }
        }
    }

    pub(crate) fn const_value(&self) -> Option<&Rv> {
        match self {
            CExpr::Const(v) => Some(v),
            _ => None,
        }
    }
}

macro_rules! funcs {
    ($($variant:ident => [$($name:literal),+], $min:expr, $max:expr;)*) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) enum Func { $($variant),* }

        impl Func {
            fn lookup(name: &str) -> Option<(Func, usize, usize)> {
                match name {
                    $($($name)|+ => Some((Func::$variant, $min, $max)),)*
                    _ => None,
                }
            }
        }
    };
}

funcs! {
    Id => ["id"], 1, 1;
    Labels => ["labels"], 1, 1;
    Type => ["type"], 1, 1;
    Keys => ["keys"], 1, 1;
    Properties => ["properties"], 1, 1;
    Size => ["size"], 1, 1;
    Length => ["length"], 1, 1;
    Nodes => ["nodes"], 1, 1;
    Relationships => ["relationships", "edges"], 1, 1;
    StartNode => ["startnode"], 1, 1;
    EndNode => ["endnode"], 1, 1;
    Degree => ["degree"], 1, 1;
    InDegree => ["in_degree", "indegree"], 1, 1;
    OutDegree => ["out_degree", "outdegree"], 1, 1;
    Coalesce => ["coalesce"], 1, 64;
    Lower => ["tolower", "lower"], 1, 1;
    Upper => ["toupper", "upper"], 1, 1;
    Trim => ["trim"], 1, 1;
    LTrim => ["ltrim"], 1, 1;
    RTrim => ["rtrim"], 1, 1;
    Replace => ["replace"], 3, 3;
    Substring => ["substring"], 2, 3;
    Left => ["left"], 2, 2;
    Right => ["right"], 2, 2;
    Split => ["split"], 2, 2;
    Reverse => ["reverse"], 1, 1;
    ToString => ["tostring"], 1, 1;
    ToInteger => ["tointeger", "toint"], 1, 1;
    ToFloat => ["tofloat"], 1, 1;
    ToBoolean => ["toboolean"], 1, 1;
    Abs => ["abs"], 1, 1;
    Ceil => ["ceil"], 1, 1;
    Floor => ["floor"], 1, 1;
    Round => ["round"], 1, 1;
    Sqrt => ["sqrt"], 1, 1;
    Sign => ["sign"], 1, 1;
    Exp => ["exp"], 1, 1;
    Log => ["log"], 1, 1;
    Log10 => ["log10"], 1, 1;
    Head => ["head"], 1, 1;
    Last => ["last"], 1, 1;
    Tail => ["tail"], 1, 1;
    Range => ["range"], 2, 3;
    Exists => ["exists"], 1, 1;
    TextMatch => ["text_match", "textmatch"], 2, 2;
}

/// Aggregate function call collected during compilation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AggSpec {
    pub(crate) func: AggFunc,
    pub(crate) distinct: bool,
    /// `None` for `count(*)`.
    pub(crate) arg: Option<CExpr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    Collect,
}

/// Name resolution for compilation.
pub(crate) trait Scope {
    fn resolve(&self, name: &str) -> Option<CExpr>;
}

pub(crate) struct Compiler<'a> {
    pub(crate) params: &'a BTreeMap<String, Value>,
    /// When set, aggregate calls are allowed and collected here.
    pub(crate) aggs: Option<&'a mut Vec<AggSpec>>,
}

/// Evaluate operator and function nodes whose inputs are all constants at
/// compile time, so that e.g. `x.p < $a + 1` can use a range index. Nodes
/// whose evaluation fails are kept, so errors still surface at run time.
fn fold(e: CExpr) -> CExpr {
    let all_const = match &e {
        CExpr::Not(a) | CExpr::Neg(a) | CExpr::IsNull(a, _) => a.const_value().is_some(),
        CExpr::Bin(_, a, b) => a.const_value().is_some() && b.const_value().is_some(),
        CExpr::Call(_, args) => args.iter().all(|a| a.const_value().is_some()),
        _ => false,
    };
    if !all_const {
        return e;
    }
    let store = Store::new(crate::config::Limits::default());
    match eval(&e, &Ctx::new(&store, &[])) {
        Ok(v) => CExpr::Const(v),
        Err(_) => e,
    }
}

impl Compiler<'_> {
    pub(crate) fn compile(&mut self, e: &Expr, scope: &dyn Scope) -> Result<CExpr> {
        self.compile_unfolded(e, scope).map(fold)
    }

    fn compile_unfolded(&mut self, e: &Expr, scope: &dyn Scope) -> Result<CExpr> {
        Ok(match e {
            Expr::Literal(v) => CExpr::Const(Rv::from_value(v)),
            Expr::Param(p) => match self.params.get(p) {
                Some(v) => CExpr::Const(Rv::from_value(v)),
                None => return Err(GraphError::Parameter(format!("missing value for parameter ${p}"))),
            },
            Expr::Var(name) => {
                scope.resolve(name).ok_or_else(|| GraphError::semantic(format!("variable `{name}` is not defined")))?
            }
            Expr::Prop(target, key) => CExpr::Prop(Box::new(self.compile(target, scope)?), key.clone()),
            Expr::Index(a, b) => CExpr::Index(Box::new(self.compile(a, scope)?), Box::new(self.compile(b, scope)?)),
            Expr::List(items) => {
                let items = items.iter().map(|i| self.compile(i, scope)).collect::<Result<Vec<_>>>()?;
                if items.iter().all(|i| i.const_value().is_some()) {
                    CExpr::Const(Rv::List(
                        items
                            .into_iter()
                            .map(|i| match i {
                                CExpr::Const(v) => v,
                                _ => unreachable!("checked const"),
                            })
                            .collect(),
                    ))
                } else {
                    CExpr::List(items)
                }
            }
            Expr::Map(entries) => {
                let entries = entries
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), self.compile(v, scope)?)))
                    .collect::<Result<Vec<_>>>()?;
                if entries.iter().all(|(_, v)| v.const_value().is_some()) {
                    CExpr::Const(Rv::Map(
                        entries
                            .into_iter()
                            .map(|(k, v)| match v {
                                CExpr::Const(v) => (k, v),
                                _ => unreachable!("checked const"),
                            })
                            .collect(),
                    ))
                } else {
                    CExpr::Map(entries)
                }
            }
            Expr::Unary(UnOp::Not, inner) => CExpr::Not(Box::new(self.compile(inner, scope)?)),
            Expr::Unary(UnOp::Neg, inner) => CExpr::Neg(Box::new(self.compile(inner, scope)?)),
            Expr::Binary(op, a, b) => {
                CExpr::Bin(*op, Box::new(self.compile(a, scope)?), Box::new(self.compile(b, scope)?))
            }
            Expr::IsNull { expr, negated } => CExpr::IsNull(Box::new(self.compile(expr, scope)?), *negated),
            Expr::CountStar => self.aggregate(AggFunc::Count, false, None)?,
            Expr::Call { name, distinct, args } => {
                if is_aggregate_name(name) {
                    let func = match name.as_str() {
                        "count" => AggFunc::Count,
                        "sum" => AggFunc::Sum,
                        "avg" => AggFunc::Avg,
                        "min" => AggFunc::Min,
                        "max" => AggFunc::Max,
                        _ => AggFunc::Collect,
                    };
                    if args.len() != 1 {
                        return Err(GraphError::semantic(format!("{name}() takes exactly one argument")));
                    }
                    if args[0].contains_aggregate() {
                        return Err(GraphError::semantic("aggregate functions cannot be nested"));
                    }
                    // Compile the argument without aggregate collection.
                    let mut inner = Compiler { params: self.params, aggs: None };
                    let arg = inner.compile(&args[0], scope)?;
                    return self.aggregate(func, *distinct, Some(arg));
                }
                let Some((func, min, max)) = Func::lookup(name) else {
                    return Err(GraphError::semantic(format!("unknown function {name}()")));
                };
                if *distinct {
                    return Err(GraphError::semantic(format!(
                        "DISTINCT is only valid in aggregate functions, not {name}()"
                    )));
                }
                if args.len() < min || args.len() > max {
                    let expected = if min == max { format!("{min}") } else { format!("{min} to {max}") };
                    return Err(GraphError::semantic(format!(
                        "{name}() takes {expected} argument(s), got {}",
                        args.len()
                    )));
                }
                let args = args.iter().map(|a| self.compile(a, scope)).collect::<Result<Vec<_>>>()?;
                CExpr::Call(func, args)
            }
            Expr::Case { operand, branches, default } => CExpr::Case {
                operand: operand.as_ref().map(|o| self.compile(o, scope).map(Box::new)).transpose()?,
                branches: branches
                    .iter()
                    .map(|(w, t)| Ok((self.compile(w, scope)?, self.compile(t, scope)?)))
                    .collect::<Result<Vec<_>>>()?,
                default: default.as_ref().map(|d| self.compile(d, scope).map(Box::new)).transpose()?,
            },
        })
    }

    fn aggregate(&mut self, func: AggFunc, distinct: bool, arg: Option<CExpr>) -> Result<CExpr> {
        match self.aggs.as_deref_mut() {
            Some(aggs) => {
                let spec = AggSpec { func, distinct, arg };
                let idx = match aggs.iter().position(|a| *a == spec) {
                    Some(i) => i,
                    None => {
                        aggs.push(spec);
                        aggs.len() - 1
                    }
                };
                Ok(CExpr::Agg(idx))
            }
            None => Err(GraphError::semantic("aggregate functions are only allowed in RETURN")),
        }
    }
}

/// Evaluation context.
pub(crate) struct Ctx<'a> {
    pub(crate) store: &'a Store,
    pub(crate) row: &'a [Rv],
    pub(crate) cols: &'a [Rv],
    pub(crate) aggs: &'a [Rv],
}

impl<'a> Ctx<'a> {
    pub(crate) fn new(store: &'a Store, row: &'a [Rv]) -> Self {
        Ctx { store, row, cols: &[], aggs: &[] }
    }
}

/// WHERE semantics: only `true` passes.
pub(crate) fn eval_predicate(e: &CExpr, ctx: &Ctx<'_>) -> Result<bool> {
    match eval(e, ctx)? {
        Rv::Bool(b) => Ok(b),
        Rv::Null => Ok(false),
        other => Err(GraphError::type_err(format!("WHERE expects a boolean, got {}", other.type_name()))),
    }
}

fn truth(v: &Rv, what: &str) -> Result<Option<bool>> {
    match v {
        Rv::Bool(b) => Ok(Some(*b)),
        Rv::Null => Ok(None),
        other => Err(GraphError::type_err(format!("{what} expects booleans, got {}", other.type_name()))),
    }
}

fn finite(f: f64, op: &str) -> Result<Rv> {
    if f.is_finite() {
        Ok(Rv::Float(f))
    } else {
        Err(GraphError::type_err(format!("{op} produced a non-finite number")))
    }
}

fn overflow(op: &str) -> GraphError {
    GraphError::type_err(format!("integer overflow in {op}"))
}

pub(crate) fn eval(e: &CExpr, ctx: &Ctx<'_>) -> Result<Rv> {
    Ok(match e {
        CExpr::Const(v) => v.clone(),
        CExpr::Var(i) => ctx.row.get(*i).cloned().unwrap_or(Rv::Null),
        CExpr::Column(i) => ctx.cols.get(*i).cloned().unwrap_or(Rv::Null),
        CExpr::Agg(i) => ctx.aggs.get(*i).cloned().unwrap_or(Rv::Null),
        CExpr::Prop(target, key) => {
            let t = eval(target, ctx)?;
            property(&t, key, ctx.store)?
        }
        CExpr::Index(a, b) => {
            let (a, b) = (eval(a, ctx)?, eval(b, ctx)?);
            match (&a, &b) {
                (Rv::Null, _) | (_, Rv::Null) => Rv::Null,
                (Rv::List(items), Rv::Int(i)) => {
                    let len = items.len() as i64;
                    let idx = if *i < 0 { len + i } else { *i };
                    if (0..len).contains(&idx) { items[idx as usize].clone() } else { Rv::Null }
                }
                (Rv::Map(_) | Rv::Node(_) | Rv::Edge(_), Rv::Str(k)) => property(&a, k, ctx.store)?,
                _ => {
                    return Err(GraphError::type_err(format!("cannot index {} with {}", a.type_name(), b.type_name())));
                }
            }
        }
        CExpr::List(items) => Rv::List(items.iter().map(|i| eval(i, ctx)).collect::<Result<_>>()?),
        CExpr::Map(entries) => {
            Rv::Map(entries.iter().map(|(k, v)| Ok((k.clone(), eval(v, ctx)?))).collect::<Result<_>>()?)
        }
        CExpr::Not(inner) => match truth(&eval(inner, ctx)?, "NOT")? {
            Some(b) => Rv::Bool(!b),
            None => Rv::Null,
        },
        CExpr::Neg(inner) => match eval(inner, ctx)? {
            Rv::Null => Rv::Null,
            Rv::Int(i) => Rv::Int(i.checked_neg().ok_or_else(|| overflow("negation"))?),
            Rv::Float(f) => Rv::Float(-f),
            other => return Err(GraphError::type_err(format!("cannot negate {}", other.type_name()))),
        },
        CExpr::IsNull(inner, negated) => Rv::Bool(eval(inner, ctx)?.is_null() != *negated),
        CExpr::Bin(op, a, b) => binary(*op, a, b, ctx)?,
        CExpr::Call(func, args) => call(*func, args, ctx)?,
        CExpr::Case { operand, branches, default } => {
            let subject = operand.as_ref().map(|o| eval(o, ctx)).transpose()?;
            for (when, then) in branches {
                let w = eval(when, ctx)?;
                let hit = match &subject {
                    Some(s) => s.query_eq(&w) == Some(true),
                    None => matches!(w, Rv::Bool(true)),
                };
                if hit {
                    return eval(then, ctx);
                }
            }
            match default {
                Some(d) => eval(d, ctx)?,
                None => Rv::Null,
            }
        }
    })
}

fn property(target: &Rv, key: &str, store: &Store) -> Result<Rv> {
    Ok(match target {
        Rv::Null => Rv::Null,
        Rv::Node(s) => match store.node(*s) {
            Some(n) if key == "id" => Rv::Str(n.id.to_string()),
            Some(n) => n.props.get(key).map_or(Rv::Null, Rv::from_value),
            None => Rv::Null,
        },
        Rv::Edge(s) => match store.edge(*s) {
            Some(e) if key == "id" => Rv::Str(e.id.to_string()),
            Some(e) => e.props.get(key).map_or(Rv::Null, Rv::from_value),
            None => Rv::Null,
        },
        Rv::Map(m) => m.get(key).cloned().unwrap_or(Rv::Null),
        other => {
            return Err(GraphError::type_err(format!("cannot read property `{key}` of {}", other.type_name())));
        }
    })
}

fn binary(op: BinOp, a: &CExpr, b: &CExpr, ctx: &Ctx<'_>) -> Result<Rv> {
    // Short-circuiting Boolean operators (Kleene logic).
    match op {
        BinOp::And => {
            let l = truth(&eval(a, ctx)?, "AND")?;
            if l == Some(false) {
                return Ok(Rv::Bool(false));
            }
            let r = truth(&eval(b, ctx)?, "AND")?;
            return Ok(match (l, r) {
                (_, Some(false)) => Rv::Bool(false),
                (Some(true), Some(true)) => Rv::Bool(true),
                _ => Rv::Null,
            });
        }
        BinOp::Or => {
            let l = truth(&eval(a, ctx)?, "OR")?;
            if l == Some(true) {
                return Ok(Rv::Bool(true));
            }
            let r = truth(&eval(b, ctx)?, "OR")?;
            return Ok(match (l, r) {
                (_, Some(true)) => Rv::Bool(true),
                (Some(false), Some(false)) => Rv::Bool(false),
                _ => Rv::Null,
            });
        }
        BinOp::Xor => {
            let l = truth(&eval(a, ctx)?, "XOR")?;
            let r = truth(&eval(b, ctx)?, "XOR")?;
            return Ok(match (l, r) {
                (Some(x), Some(y)) => Rv::Bool(x != y),
                _ => Rv::Null,
            });
        }
        _ => {}
    }
    let l = eval(a, ctx)?;
    let r = eval(b, ctx)?;
    let bool_or_null = |o: Option<bool>| o.map_or(Rv::Null, Rv::Bool);
    Ok(match op {
        BinOp::Eq => bool_or_null(l.query_eq(&r)),
        BinOp::Neq => bool_or_null(l.query_eq(&r).map(|b| !b)),
        BinOp::Lt => bool_or_null(l.query_cmp(&r).map(|o| o.is_lt())),
        BinOp::Le => bool_or_null(l.query_cmp(&r).map(|o| o.is_le())),
        BinOp::Gt => bool_or_null(l.query_cmp(&r).map(|o| o.is_gt())),
        BinOp::Ge => bool_or_null(l.query_cmp(&r).map(|o| o.is_ge())),
        BinOp::In => match &r {
            Rv::Null => Rv::Null,
            Rv::List(items) => {
                let mut unknown = false;
                for item in items {
                    match l.query_eq(item) {
                        Some(true) => return Ok(Rv::Bool(true)),
                        None => unknown = true,
                        Some(false) => {}
                    }
                }
                if unknown { Rv::Null } else { Rv::Bool(false) }
            }
            other => return Err(GraphError::type_err(format!("IN expects a list, got {}", other.type_name()))),
        },
        BinOp::Contains | BinOp::StartsWith | BinOp::EndsWith => match (&l, &r) {
            (Rv::Str(s), Rv::Str(p)) => Rv::Bool(match op {
                BinOp::Contains => s.contains(p.as_str()),
                BinOp::StartsWith => s.starts_with(p.as_str()),
                _ => s.ends_with(p.as_str()),
            }),
            _ => Rv::Null,
        },
        BinOp::Add => add(l, r, ctx.store)?,
        BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Pow => arithmetic(op, &l, &r)?,
        BinOp::And | BinOp::Or | BinOp::Xor => unreachable!("handled above"),
    })
}

fn scalar_to_string(v: &Rv) -> Option<String> {
    match v {
        Rv::Str(s) => Some(s.clone()),
        Rv::Int(i) => Some(i.to_string()),
        Rv::Float(f) => Some(format_float(*f)),
        Rv::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn format_float(f: f64) -> String {
    let s = f.to_string();
    if s.contains(['.', 'e', 'E']) || !f.is_finite() { s } else { format!("{s}.0") }
}

fn add(l: Rv, r: Rv, store: &Store) -> Result<Rv> {
    Ok(match (l, r) {
        (Rv::Null, _) | (_, Rv::Null) => Rv::Null,
        (Rv::Int(a), Rv::Int(b)) => Rv::Int(a.checked_add(b).ok_or_else(|| overflow("addition"))?),
        (Rv::Int(a), Rv::Float(b)) => finite(a as f64 + b, "addition")?,
        (Rv::Float(a), Rv::Int(b)) => finite(a + b as f64, "addition")?,
        (Rv::Float(a), Rv::Float(b)) => finite(a + b, "addition")?,
        (Rv::List(mut a), Rv::List(b)) => {
            a.extend(b);
            Rv::List(a)
        }
        (Rv::List(mut a), b) => {
            a.push(b);
            Rv::List(a)
        }
        (a, Rv::List(b)) => {
            let mut v = Vec::with_capacity(b.len() + 1);
            v.push(a);
            v.extend(b);
            Rv::List(v)
        }
        (a, b) => match (scalar_to_string(&a), scalar_to_string(&b)) {
            (Some(x), Some(y)) if matches!(a, Rv::Str(_)) || matches!(b, Rv::Str(_)) => {
                if x.len() + y.len() > store.limits.max_string_bytes {
                    return Err(GraphError::LimitExceeded("string concatenation result is too large".into()));
                }
                Rv::Str(x + &y)
            }
            _ => return Err(GraphError::type_err(format!("cannot add {} and {}", a.type_name(), b.type_name()))),
        },
    })
}

fn arithmetic(op: BinOp, l: &Rv, r: &Rv) -> Result<Rv> {
    let name = match op {
        BinOp::Sub => "subtraction",
        BinOp::Mul => "multiplication",
        BinOp::Div => "division",
        BinOp::Mod => "modulo",
        _ => "exponentiation",
    };
    Ok(match (l, r) {
        (Rv::Null, _) | (_, Rv::Null) => Rv::Null,
        (Rv::Int(a), Rv::Int(b)) if op != BinOp::Pow => {
            let (a, b) = (*a, *b);
            Rv::Int(match op {
                BinOp::Sub => a.checked_sub(b).ok_or_else(|| overflow(name))?,
                BinOp::Mul => a.checked_mul(b).ok_or_else(|| overflow(name))?,
                BinOp::Div => {
                    if b == 0 {
                        return Err(GraphError::type_err("division by zero"));
                    }
                    a.checked_div(b).ok_or_else(|| overflow(name))?
                }
                _ => {
                    if b == 0 {
                        return Err(GraphError::type_err("modulo by zero"));
                    }
                    a.checked_rem(b).ok_or_else(|| overflow(name))?
                }
            })
        }
        _ => {
            let (Some(a), Some(b)) = (as_f64(l), as_f64(r)) else {
                return Err(GraphError::type_err(format!(
                    "cannot apply {name} to {} and {}",
                    l.type_name(),
                    r.type_name()
                )));
            };
            let v = match op {
                BinOp::Sub => a - b,
                BinOp::Mul => a * b,
                BinOp::Div => a / b,
                BinOp::Mod => a % b,
                _ => a.powf(b),
            };
            finite(v, name)?
        }
    })
}

fn as_f64(v: &Rv) -> Option<f64> {
    match v {
        Rv::Int(i) => Some(*i as f64),
        Rv::Float(f) => Some(*f),
        _ => None,
    }
}

fn type_mismatch(func: &str, v: &Rv) -> GraphError {
    GraphError::type_err(format!("{func}() does not accept {}", v.type_name()))
}

fn int_arg(v: &Rv, func: &str) -> Result<Option<i64>> {
    match v {
        Rv::Null => Ok(None),
        Rv::Int(i) => Ok(Some(*i)),
        other => Err(type_mismatch(func, other)),
    }
}

fn str_arg<'v>(v: &'v Rv, func: &str) -> Result<Option<&'v str>> {
    match v {
        Rv::Null => Ok(None),
        Rv::Str(s) => Ok(Some(s)),
        other => Err(type_mismatch(func, other)),
    }
}

fn call(func: Func, args: &[CExpr], ctx: &Ctx<'_>) -> Result<Rv> {
    if func == Func::Coalesce {
        for a in args {
            let v = eval(a, ctx)?;
            if !v.is_null() {
                return Ok(v);
            }
        }
        return Ok(Rv::Null);
    }
    let vals = args.iter().map(|a| eval(a, ctx)).collect::<Result<Vec<_>>>()?;
    let store = ctx.store;
    let a = &vals[0];
    let name = format!("{func:?}").to_lowercase();
    let math = |f: fn(f64) -> f64| -> Result<Rv> {
        match a {
            Rv::Null => Ok(Rv::Null),
            v => match as_f64(v) {
                Some(x) => {
                    let r = f(x);
                    Ok(if r.is_finite() { Rv::Float(r) } else { Rv::Null })
                }
                None => Err(type_mismatch(&name, v)),
            },
        }
    };
    Ok(match func {
        Func::Coalesce => unreachable!("handled above"),
        Func::Id => match a {
            Rv::Null => Rv::Null,
            Rv::Node(s) => store.node(*s).map_or(Rv::Null, |n| Rv::Str(n.id.to_string())),
            Rv::Edge(s) => store.edge(*s).map_or(Rv::Null, |e| Rv::Str(e.id.to_string())),
            other => return Err(type_mismatch("id", other)),
        },
        Func::Labels => match a {
            Rv::Null => Rv::Null,
            Rv::Node(s) => Rv::List(store.node_label_names(*s).into_iter().map(Rv::Str).collect()),
            other => return Err(type_mismatch("labels", other)),
        },
        Func::Type => match a {
            Rv::Null => Rv::Null,
            Rv::Edge(s) => store.edge(*s).map_or(Rv::Null, |e| Rv::Str(store.types.name(e.ty).to_owned())),
            other => return Err(type_mismatch("type", other)),
        },
        Func::Keys | Func::Properties => {
            let props: Option<BTreeMap<String, Rv>> = match a {
                Rv::Null => None,
                Rv::Node(s) => {
                    store.node(*s).map(|n| n.props.iter().map(|(k, v)| (k.clone(), Rv::from_value(v))).collect())
                }
                Rv::Edge(s) => {
                    store.edge(*s).map(|e| e.props.iter().map(|(k, v)| (k.clone(), Rv::from_value(v))).collect())
                }
                Rv::Map(m) => Some(m.clone()),
                other => return Err(type_mismatch(&name, other)),
            };
            match (props, func) {
                (None, _) => Rv::Null,
                (Some(p), Func::Keys) => Rv::List(p.into_keys().map(Rv::Str).collect()),
                (Some(p), _) => Rv::Map(p),
            }
        }
        Func::Size => match a {
            Rv::Null => Rv::Null,
            Rv::List(l) => Rv::Int(l.len() as i64),
            Rv::Str(s) => Rv::Int(s.chars().count() as i64),
            Rv::Map(m) => Rv::Int(m.len() as i64),
            other => return Err(type_mismatch("size", other)),
        },
        Func::Length => match a {
            Rv::Null => Rv::Null,
            Rv::Path(p) => Rv::Int(p.edges.len() as i64),
            Rv::List(l) => Rv::Int(l.len() as i64),
            Rv::Str(s) => Rv::Int(s.chars().count() as i64),
            other => return Err(type_mismatch("length", other)),
        },
        Func::Nodes => match a {
            Rv::Null => Rv::Null,
            Rv::Path(p) => Rv::List(p.nodes.iter().map(|n| Rv::Node(*n)).collect()),
            other => return Err(type_mismatch("nodes", other)),
        },
        Func::Relationships => match a {
            Rv::Null => Rv::Null,
            Rv::Path(p) => Rv::List(p.edges.iter().map(|e| Rv::Edge(*e)).collect()),
            Rv::List(l) => Rv::List(l.clone()),
            other => return Err(type_mismatch("relationships", other)),
        },
        Func::StartNode | Func::EndNode => match a {
            Rv::Null => Rv::Null,
            Rv::Edge(s) => match store.edge(*s) {
                Some(e) => Rv::Node(if func == Func::StartNode { e.src } else { e.tgt }),
                None => Rv::Null,
            },
            other => return Err(type_mismatch(&name, other)),
        },
        Func::Degree | Func::InDegree | Func::OutDegree => match a {
            Rv::Null => Rv::Null,
            Rv::Node(s) if store.node(*s).is_some() => {
                let dir = match func {
                    Func::InDegree => Direction::Incoming,
                    Func::OutDegree => Direction::Outgoing,
                    _ => Direction::Both,
                };
                Rv::Int(store.degree(*s, dir) as i64)
            }
            Rv::Node(_) => Rv::Null,
            other => return Err(type_mismatch(&name, other)),
        },
        Func::Lower | Func::Upper | Func::Trim | Func::LTrim | Func::RTrim => match str_arg(a, &name)? {
            None => Rv::Null,
            Some(s) => Rv::Str(match func {
                Func::Lower => s.to_lowercase(),
                Func::Upper => s.to_uppercase(),
                Func::Trim => s.trim().to_owned(),
                Func::LTrim => s.trim_start().to_owned(),
                _ => s.trim_end().to_owned(),
            }),
        },
        Func::Replace => match (str_arg(a, "replace")?, str_arg(&vals[1], "replace")?, str_arg(&vals[2], "replace")?) {
            (Some(s), Some(from), Some(to)) => {
                if from.is_empty() {
                    return Err(GraphError::type_err("replace() search string must not be empty"));
                }
                let count = s.matches(from).count();
                if s.len() + count.saturating_mul(to.len()) > store.limits.max_string_bytes {
                    return Err(GraphError::LimitExceeded("replace() result is too large".into()));
                }
                Rv::Str(s.replace(from, to))
            }
            _ => Rv::Null,
        },
        Func::Substring => {
            let s = str_arg(a, "substring")?;
            let start = int_arg(&vals[1], "substring")?;
            let len = match vals.get(2) {
                Some(v) => int_arg(v, "substring")?.map(Some),
                None => Some(None),
            };
            match (s, start, len) {
                (Some(s), Some(start), Some(len)) => {
                    if start < 0 || len.is_some_and(|l| l < 0) {
                        return Err(GraphError::type_err("substring() start and length must be non-negative"));
                    }
                    let it = s.chars().skip(start as usize);
                    Rv::Str(match len {
                        Some(l) => it.take(l as usize).collect(),
                        None => it.collect(),
                    })
                }
                _ => Rv::Null,
            }
        }
        Func::Left | Func::Right => match (str_arg(a, &name)?, int_arg(&vals[1], &name)?) {
            (Some(s), Some(n)) => {
                if n < 0 {
                    return Err(GraphError::type_err(format!("{name}() length must be non-negative")));
                }
                let n = n as usize;
                Rv::Str(if func == Func::Left {
                    s.chars().take(n).collect()
                } else {
                    let total = s.chars().count();
                    s.chars().skip(total.saturating_sub(n)).collect()
                })
            }
            _ => Rv::Null,
        },
        Func::Split => match (str_arg(a, "split")?, str_arg(&vals[1], "split")?) {
            (Some(s), Some(d)) => {
                if d.is_empty() {
                    Rv::List(s.chars().map(|c| Rv::Str(c.to_string())).collect())
                } else {
                    Rv::List(s.split(d).map(|p| Rv::Str(p.to_owned())).collect())
                }
            }
            _ => Rv::Null,
        },
        Func::Reverse => match a {
            Rv::Null => Rv::Null,
            Rv::Str(s) => Rv::Str(s.chars().rev().collect()),
            Rv::List(l) => Rv::List(l.iter().rev().cloned().collect()),
            other => return Err(type_mismatch("reverse", other)),
        },
        Func::ToString => match a {
            Rv::Null => Rv::Null,
            v => match scalar_to_string(v) {
                Some(s) => Rv::Str(s),
                None => return Err(type_mismatch("toString", v)),
            },
        },
        Func::ToInteger => match a {
            Rv::Int(i) => Rv::Int(*i),
            Rv::Float(f) => {
                let t = f.trunc();
                if (-9.223_372_036_854_776e18..9.223_372_036_854_776e18).contains(&t) {
                    Rv::Int(t as i64)
                } else {
                    Rv::Null
                }
            }
            Rv::Str(s) => {
                let s = s.trim();
                match s.parse::<i64>() {
                    Ok(i) => Rv::Int(i),
                    Err(_) => match s.parse::<f64>() {
                        Ok(f) if f.is_finite() && f.trunc() >= -9.2e18 && f.trunc() <= 9.2e18 => {
                            Rv::Int(f.trunc() as i64)
                        }
                        _ => Rv::Null,
                    },
                }
            }
            Rv::Bool(b) => Rv::Int(i64::from(*b)),
            _ => Rv::Null,
        },
        Func::ToFloat => match a {
            Rv::Int(i) => Rv::Float(*i as f64),
            Rv::Float(f) => Rv::Float(*f),
            Rv::Str(s) => match s.trim().parse::<f64>() {
                Ok(f) if f.is_finite() => Rv::Float(f),
                _ => Rv::Null,
            },
            _ => Rv::Null,
        },
        Func::ToBoolean => match a {
            Rv::Bool(b) => Rv::Bool(*b),
            Rv::Str(s) if s.trim().eq_ignore_ascii_case("true") => Rv::Bool(true),
            Rv::Str(s) if s.trim().eq_ignore_ascii_case("false") => Rv::Bool(false),
            Rv::Int(i) => Rv::Bool(*i != 0),
            _ => Rv::Null,
        },
        Func::Abs => match a {
            Rv::Null => Rv::Null,
            Rv::Int(i) => Rv::Int(i.checked_abs().ok_or_else(|| overflow("abs"))?),
            Rv::Float(f) => Rv::Float(f.abs()),
            other => return Err(type_mismatch("abs", other)),
        },
        Func::Sign => match a {
            Rv::Null => Rv::Null,
            Rv::Int(i) => Rv::Int(i.signum()),
            Rv::Float(f) => Rv::Int(if *f > 0.0 {
                1
            } else if *f < 0.0 {
                -1
            } else {
                0
            }),
            other => return Err(type_mismatch("sign", other)),
        },
        Func::Ceil => math(f64::ceil)?,
        Func::Floor => math(f64::floor)?,
        Func::Round => math(f64::round)?,
        Func::Sqrt => math(f64::sqrt)?,
        Func::Exp => math(f64::exp)?,
        Func::Log => math(f64::ln)?,
        Func::Log10 => math(f64::log10)?,
        Func::Head | Func::Last => match a {
            Rv::Null => Rv::Null,
            Rv::List(l) => (if func == Func::Head { l.first() } else { l.last() }).cloned().unwrap_or(Rv::Null),
            other => return Err(type_mismatch(&name, other)),
        },
        Func::Tail => match a {
            Rv::Null => Rv::Null,
            Rv::List(l) => Rv::List(l.iter().skip(1).cloned().collect()),
            other => return Err(type_mismatch("tail", other)),
        },
        Func::Range => {
            let start = int_arg(a, "range")?;
            let end = int_arg(&vals[1], "range")?;
            let step = match vals.get(2) {
                Some(v) => int_arg(v, "range")?,
                None => Some(1),
            };
            match (start, end, step) {
                (Some(s), Some(e), Some(st)) => {
                    if st == 0 {
                        return Err(GraphError::type_err("range() step must not be zero"));
                    }
                    let count = if (st > 0 && e < s) || (st < 0 && e > s) {
                        0
                    } else {
                        ((i128::from(e) - i128::from(s)) / i128::from(st) + 1) as u128
                    };
                    if count > 10_000_000 {
                        return Err(GraphError::LimitExceeded(
                            "range() would produce more than 10,000,000 values".into(),
                        ));
                    }
                    let mut out = Vec::with_capacity(count as usize);
                    let mut v = i128::from(s);
                    for _ in 0..count {
                        out.push(Rv::Int(v as i64));
                        v += i128::from(st);
                    }
                    Rv::List(out)
                }
                _ => Rv::Null,
            }
        }
        Func::Exists => Rv::Bool(!a.is_null()),
        Func::TextMatch => match (&vals[0], str_arg(&vals[1], "text_match")?) {
            (Rv::Null, _) | (_, None) => Rv::Null,
            (v, Some(q)) => match v.to_value() {
                Some(value) => Rv::Bool(text_matches(&value, q)),
                None => return Err(type_mismatch("text_match", v)),
            },
        },
    })
}

/// Build a path value from a start node and an ordered edge list.
pub(crate) fn walk_path(store: &Store, start: u32, edges: &[u32]) -> PathRv {
    let mut nodes = Vec::with_capacity(edges.len() + 1);
    nodes.push(start);
    let mut cur = start;
    for e in edges {
        if let Some(rec) = store.edge(*e) {
            cur = rec.other(cur);
        }
        nodes.push(cur);
    }
    PathRv { nodes, edges: edges.to_vec() }
}
