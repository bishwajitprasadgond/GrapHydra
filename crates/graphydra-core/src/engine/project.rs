//! `RETURN` processing: projection, aggregation, DISTINCT, ORDER BY,
//! SKIP and LIMIT.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};

use indexmap::IndexMap;

use crate::engine::expr::{AggFunc, AggSpec, CExpr, Compiler, Ctx, Scope, eval};
use crate::engine::plan::VarTable;
use crate::engine::result::{Budget, ResultValue};
use crate::engine::rv::{GroupKey, Rv};
use crate::error::{GraphError, Result};
use crate::language::ast::{Expr, ReturnClause};
use crate::store::Store;
use crate::value::Value;

pub(crate) struct Projection {
    pub(crate) names: Vec<String>,
    /// Non-aggregating: items in row scope. Aggregating: post-aggregation
    /// items (`Column(k)` = k-th grouping key, `Agg(j)` = j-th aggregate).
    items: Vec<CExpr>,
    agg: Option<AggPlan>,
    distinct: bool,
    order: Vec<(CExpr, bool)>,
    skip: usize,
    limit: Option<usize>,
}

struct AggPlan {
    keys: Vec<CExpr>,
    specs: Vec<AggSpec>,
}

struct OrderScope<'a> {
    names: &'a [String],
    vars: Option<&'a VarTable>,
}

impl Scope for OrderScope<'_> {
    fn resolve(&self, name: &str) -> Option<CExpr> {
        if let Some(i) = self.names.iter().position(|n| n == name) {
            return Some(CExpr::Column(i));
        }
        self.vars.and_then(|v| v.resolve(name))
    }
}

/// Scope that rejects every variable (for SKIP/LIMIT).
struct EmptyScope;

impl Scope for EmptyScope {
    fn resolve(&self, _: &str) -> Option<CExpr> {
        None
    }
}

fn count_arg(e: Option<&Expr>, params: &BTreeMap<String, Value>, what: &str) -> Result<Option<usize>> {
    let Some(e) = e else { return Ok(None) };
    let c = Compiler { params, aggs: None }.compile(e, &EmptyScope).map_err(|err| match err {
        GraphError::Semantic(_) => GraphError::semantic(format!("{what} must be a literal or parameter")),
        other => other,
    })?;
    match c.const_value() {
        Some(Rv::Int(i)) if *i >= 0 => Ok(Some(usize::try_from(*i).unwrap_or(usize::MAX))),
        _ => Err(GraphError::semantic(format!("{what} must be a non-negative integer"))),
    }
}

impl Projection {
    /// Projection for statements without RETURN (mutations only).
    pub(crate) fn none() -> Self {
        Projection {
            names: Vec::new(),
            items: Vec::new(),
            agg: None,
            distinct: false,
            order: Vec::new(),
            skip: 0,
            limit: Some(0),
        }
    }

    pub(crate) fn compile(ret: &ReturnClause, vars: &VarTable, params: &BTreeMap<String, Value>) -> Result<Self> {
        let mut item_exprs: Vec<Expr> = ret.items.iter().map(|i| i.expr.clone()).collect();
        let mut names: Vec<String> = ret.items.iter().map(|i| i.name.clone()).collect();
        if ret.star {
            let named = vars.named();
            if named.is_empty() {
                return Err(GraphError::semantic("RETURN * requires at least one named variable"));
            }
            for (_, n) in named {
                item_exprs.push(Expr::Var(n.to_owned()));
                names.push(n.to_owned());
            }
        }
        for (i, n) in names.iter().enumerate() {
            if names[..i].contains(n) {
                return Err(GraphError::semantic(format!("duplicate column name `{n}`; use AS to rename")));
            }
        }
        let skip = count_arg(ret.skip.as_ref(), params, "SKIP")?.unwrap_or(0);
        let limit = count_arg(ret.limit.as_ref(), params, "LIMIT")?;
        let aggregating = item_exprs.iter().any(Expr::contains_aggregate);

        if !aggregating {
            let mut c = Compiler { params, aggs: None };
            let items = item_exprs.iter().map(|e| c.compile(e, vars)).collect::<Result<Vec<_>>>()?;
            let order_scope = OrderScope { names: &names, vars: if ret.distinct { None } else { Some(vars) } };
            let mut order = Vec::new();
            for o in &ret.order_by {
                if o.expr.contains_aggregate() {
                    return Err(GraphError::semantic("ORDER BY cannot use aggregates unless RETURN aggregates"));
                }
                let compiled = match item_exprs.iter().position(|e| *e == o.expr) {
                    Some(i) => CExpr::Column(i),
                    None => c.compile(&o.expr, &order_scope).map_err(|e| match e {
                        GraphError::Semantic(m) if ret.distinct => GraphError::semantic(format!(
                            "{m}; with RETURN DISTINCT, ORDER BY may only use returned columns"
                        )),
                        other => other,
                    })?,
                };
                order.push((compiled, o.descending));
            }
            return Ok(Projection { names, items, agg: None, distinct: ret.distinct, order, skip, limit });
        }

        let mut keys = Vec::new();
        let mut specs = Vec::new();
        let mut items = Vec::new();
        for e in &item_exprs {
            if e.contains_aggregate() {
                let mut c = Compiler { params, aggs: Some(&mut specs) };
                let post = c.compile(e, vars)?;
                let mut deps = Vec::new();
                post.deps(&mut deps);
                if !deps.is_empty() {
                    return Err(GraphError::semantic(
                        "an expression mixing aggregates with non-aggregated variables is not supported; \
                         return the non-aggregated part as its own column",
                    ));
                }
                items.push(post);
            } else {
                let mut c = Compiler { params, aggs: None };
                keys.push(c.compile(e, vars)?);
                items.push(CExpr::Column(keys.len() - 1));
            }
        }
        let order_scope = OrderScope { names: &names, vars: None };
        let mut order = Vec::new();
        for o in &ret.order_by {
            let compiled = match item_exprs.iter().position(|e| *e == o.expr) {
                Some(i) => CExpr::Column(i),
                None => {
                    let mut c = Compiler { params, aggs: Some(&mut specs) };
                    c.compile(&o.expr, &order_scope).map_err(|e| match e {
                        GraphError::Semantic(m) => GraphError::semantic(format!(
                            "{m}; in an aggregating RETURN, ORDER BY may only use returned columns and aggregates"
                        )),
                        other => other,
                    })?
                }
            };
            order.push((compiled, o.descending));
        }
        Ok(Projection { names, items, agg: Some(AggPlan { keys, specs }), distinct: ret.distinct, order, skip, limit })
    }

    pub(crate) fn streaming(&self) -> bool {
        self.agg.is_none() && self.order.is_empty()
    }
}

#[derive(Debug, Clone)]
enum AggState {
    Count(i64),
    Sum { int: i64, float: f64, is_float: bool },
    Avg { sum: f64, n: u64 },
    Min(Option<Rv>),
    Max(Option<Rv>),
    Collect(Vec<Rv>),
}

impl AggState {
    fn new(f: AggFunc) -> Self {
        match f {
            AggFunc::Count => AggState::Count(0),
            AggFunc::Sum => AggState::Sum { int: 0, float: 0.0, is_float: false },
            AggFunc::Avg => AggState::Avg { sum: 0.0, n: 0 },
            AggFunc::Min => AggState::Min(None),
            AggFunc::Max => AggState::Max(None),
            AggFunc::Collect => AggState::Collect(Vec::new()),
        }
    }

    /// `v` is `None` for `count(*)`.
    fn update(&mut self, v: Option<Rv>, store: &Store) -> Result<()> {
        match (self, v) {
            (AggState::Count(n), None) => *n += 1,
            // Only count(*) has no argument.
            (_, None) | (_, Some(Rv::Null)) => {}
            (AggState::Count(n), Some(_)) => *n += 1,
            (AggState::Sum { int, float, is_float }, Some(v)) => match v {
                Rv::Int(i) if !*is_float => match int.checked_add(i) {
                    Some(s) => *int = s,
                    None => return Err(GraphError::type_err("integer overflow in sum()")),
                },
                Rv::Int(i) => *float += i as f64,
                Rv::Float(f) => {
                    if !*is_float {
                        *is_float = true;
                        *float = *int as f64;
                    }
                    *float += f;
                }
                other => return Err(GraphError::type_err(format!("sum() expects numbers, got {}", other.type_name()))),
            },
            (AggState::Avg { sum, n }, Some(v)) => match v {
                Rv::Int(i) => {
                    *sum += i as f64;
                    *n += 1;
                }
                Rv::Float(f) => {
                    *sum += f;
                    *n += 1;
                }
                other => return Err(GraphError::type_err(format!("avg() expects numbers, got {}", other.type_name()))),
            },
            (AggState::Min(cur), Some(v)) => {
                if cur.as_ref().is_none_or(|c| v.total_cmp(c, store) == Ordering::Less) {
                    *cur = Some(v);
                }
            }
            (AggState::Max(cur), Some(v)) => {
                if cur.as_ref().is_none_or(|c| v.total_cmp(c, store) == Ordering::Greater) {
                    *cur = Some(v);
                }
            }
            (AggState::Collect(items), Some(v)) => items.push(v),
        }
        Ok(())
    }

    fn finish(self) -> Result<Rv> {
        Ok(match self {
            AggState::Count(n) => Rv::Int(n),
            AggState::Sum { int, float, is_float } => {
                if is_float {
                    if !float.is_finite() {
                        return Err(GraphError::type_err("sum() overflowed"));
                    }
                    Rv::Float(float)
                } else {
                    Rv::Int(int)
                }
            }
            AggState::Avg { sum, n } => {
                if n == 0 {
                    Rv::Null
                } else {
                    Rv::Float(sum / n as f64)
                }
            }
            AggState::Min(v) | AggState::Max(v) => v.unwrap_or(Rv::Null),
            AggState::Collect(items) => Rv::List(items),
        })
    }
}

struct Group {
    keys: Vec<Rv>,
    states: Vec<AggState>,
    seen: Vec<Option<HashSet<GroupKey>>>,
}

/// Incremental consumer of matched rows.
pub(crate) struct Projector<'p> {
    p: &'p Projection,
    out: Vec<(Vec<Rv>, Vec<Rv>)>,
    seen: HashSet<Vec<GroupKey>>,
    skipped: usize,
    groups: IndexMap<Vec<GroupKey>, Group>,
}

impl<'p> Projector<'p> {
    pub(crate) fn new(p: &'p Projection) -> Self {
        Projector { p, out: Vec::new(), seen: HashSet::new(), skipped: 0, groups: IndexMap::new() }
    }

    fn keep_limit(&self) -> Option<usize> {
        self.p.limit.map(|l| l.saturating_add(self.p.skip))
    }

    /// Consume one row. Returns `false` once no more rows are needed.
    pub(crate) fn push(&mut self, store: &Store, row: &[Rv], budget: &Budget) -> Result<bool> {
        if self.p.limit == Some(0) && self.p.agg.is_none() {
            return Ok(false);
        }
        let ctx = Ctx::new(store, row);
        if let Some(agg) = &self.p.agg {
            let keys = agg.keys.iter().map(|k| eval(k, &ctx)).collect::<Result<Vec<_>>>()?;
            let gk: Vec<GroupKey> = keys.iter().cloned().map(GroupKey).collect();
            if !self.groups.contains_key(&gk) {
                budget.check_rows(self.groups.len() + 1, "intermediate")?;
                let group = Group {
                    keys,
                    states: agg.specs.iter().map(|s| AggState::new(s.func)).collect(),
                    seen: agg.specs.iter().map(|s| s.distinct.then(HashSet::new)).collect(),
                };
                self.groups.insert(gk.clone(), group);
            }
            let group = self.groups.get_mut(&gk).expect("inserted above");
            for (j, spec) in agg.specs.iter().enumerate() {
                let v = spec.arg.as_ref().map(|a| eval(a, &ctx)).transpose()?;
                if let (Some(seen), Some(val)) = (&mut group.seen[j], &v)
                    && (val.is_null() || !seen.insert(GroupKey(val.clone())))
                {
                    continue;
                }
                group.states[j].update(v, store)?;
            }
            return Ok(true);
        }
        let cols = self.p.items.iter().map(|e| eval(e, &ctx)).collect::<Result<Vec<_>>>()?;
        if self.p.distinct && !self.seen.insert(cols.iter().cloned().map(GroupKey).collect()) {
            return Ok(true);
        }
        if self.p.order.is_empty() {
            if self.skipped < self.p.skip {
                self.skipped += 1;
                return Ok(true);
            }
            self.out.push((cols, Vec::new()));
            budget.check_rows(self.out.len(), "result")?;
            return Ok(self.p.limit.is_none_or(|l| self.out.len() < l));
        }
        let octx = Ctx { store, row, cols: &cols, aggs: &[] };
        let keys = self.p.order.iter().map(|(e, _)| eval(e, &octx)).collect::<Result<Vec<_>>>()?;
        self.out.push((cols, keys));
        budget.check_rows(self.out.len(), "intermediate")?;
        if let Some(k) = self.keep_limit() {
            // Bounded top-k: periodically sort and discard rows that can
            // never make it into the final window. Stable sort keeps the
            // first-seen row among equals, matching a full sort.
            if self.out.len() > k.saturating_mul(2).saturating_add(1024) {
                self.sort(store);
                self.out.truncate(k);
            }
        }
        Ok(true)
    }

    fn sort(&mut self, store: &Store) {
        let order = &self.p.order;
        self.out.sort_by(|a, b| {
            for (i, (_, desc)) in order.iter().enumerate() {
                let c = a.1[i].total_cmp(&b.1[i], store);
                let c = if *desc { c.reverse() } else { c };
                if c != Ordering::Equal {
                    return c;
                }
            }
            Ordering::Equal
        });
    }

    pub(crate) fn finish(mut self, store: &Store, budget: &Budget) -> Result<Vec<Vec<ResultValue>>> {
        if let Some(agg) = &self.p.agg {
            if self.groups.is_empty() && agg.keys.is_empty() {
                self.groups.insert(
                    Vec::new(),
                    Group {
                        keys: Vec::new(),
                        states: agg.specs.iter().map(|s| AggState::new(s.func)).collect(),
                        seen: Vec::new(),
                    },
                );
            }
            let mut seen: HashSet<Vec<GroupKey>> = HashSet::new();
            for (_, g) in std::mem::take(&mut self.groups) {
                let aggs = g.states.into_iter().map(AggState::finish).collect::<Result<Vec<_>>>()?;
                let ctx = Ctx { store, row: &[], cols: &g.keys, aggs: &aggs };
                let cols = self.p.items.iter().map(|e| eval(e, &ctx)).collect::<Result<Vec<_>>>()?;
                if self.p.distinct && !seen.insert(cols.iter().cloned().map(GroupKey).collect()) {
                    continue;
                }
                let octx = Ctx { store, row: &[], cols: &cols, aggs: &aggs };
                let keys = self.p.order.iter().map(|(e, _)| eval(e, &octx)).collect::<Result<Vec<_>>>()?;
                self.out.push((cols, keys));
            }
        }
        let windowed_already = self.p.streaming();
        if !self.p.order.is_empty() {
            self.sort(store);
        }
        let rows: Vec<Vec<Rv>> = if windowed_already {
            self.out.into_iter().map(|(c, _)| c).collect()
        } else {
            let it = self.out.into_iter().skip(self.p.skip).map(|(c, _)| c);
            match self.p.limit {
                Some(l) => it.take(l).collect(),
                None => it.collect(),
            }
        };
        budget.check_rows(rows.len(), "result")?;
        Ok(rows.into_iter().map(|r| r.iter().map(|v| v.to_result(store)).collect()).collect())
    }
}
