//! Query planning and execution.

pub(crate) mod expr;
pub(crate) mod matcher;
pub(crate) mod mutate;
pub(crate) mod plan;
pub(crate) mod project;
pub mod result;
pub(crate) mod rv;

use std::time::Instant;

use crate::engine::matcher::Matcher;
use crate::engine::mutate::{Mutator, UpdatePlan};
use crate::engine::plan::{Planner, VarTable, plan_match};
use crate::engine::project::{Projection, Projector};
use crate::engine::result::{Budget, PlanStep, QueryOptions, QueryResult, QueryStats};
use crate::engine::rv::Rv;
use crate::error::Result;
use crate::language::ast::{ExplainMode, Query};

/// Plan and execute one query against `m`. Mutations go through `m`, which
/// records them in the current transaction.
pub(crate) fn run_query(
    m: &mut dyn Mutator,
    q: &Query,
    mode: ExplainMode,
    opts: &QueryOptions,
    budget: &mut Budget,
    mut stats: QueryStats,
) -> Result<QueryResult> {
    let plan_start = Instant::now();
    let params = &opts.parameters;
    let mut vars = VarTable::default();
    let planner = Planner { store: m.store(), disable_indexes: opts.disable_indexes };
    let mut match_plan = plan_match(&planner, &q.patterns, q.where_.as_ref(), &mut vars, params)?;
    let updates = UpdatePlan::compile(&q.updates, &mut vars, params)?;
    let projection = match &q.ret {
        Some(r) => Projection::compile(r, &vars, params)?,
        None => Projection::none(),
    };
    if !q.updates.is_empty() {
        match_plan.explain.steps.push(PlanStep {
            operator: "Update".into(),
            detail: format!("{} clause(s), applied after matching (snapshot semantics)", q.updates.len()),
            estimated_rows: None,
        });
    }
    if q.ret.is_some() {
        match_plan.explain.steps.push(PlanStep {
            operator: "Project".into(),
            detail: projection.names.join(", "),
            estimated_rows: None,
        });
    }
    stats.plan_time = plan_start.elapsed();

    let columns = projection.names.clone();
    if mode == ExplainMode::Explain {
        return Ok(QueryResult { columns, rows: Vec::new(), stats, plan: Some(match_plan.explain) });
    }

    let exec_start = Instant::now();
    let width = vars.len();
    let rows = if q.updates.is_empty() {
        let store = m.store();
        let mut projector = Projector::new(&projection);
        if !match_plan.impossible {
            let mut row = vec![Rv::Null; width];
            let budget_ref: &Budget = &*budget;
            // The budget is shared between the matcher (mutable) and the
            // projector (row limits only); copy the limits the sink needs.
            let (max_rows, max_intermediate) = (budget_ref.max_rows, budget_ref.max_intermediate);
            let limits = Budget::new(None, None, u64::MAX, max_rows, max_intermediate);
            let mut sink = |r: &[Rv]| projector.push(store, r, &limits);
            Matcher::new(store, &match_plan.steps, budget, &mut stats).run(&mut row, &mut sink)?;
        }
        projector.finish(store, budget)?
    } else {
        // Snapshot semantics: match everything first, then mutate, so that
        // updates never affect the rows being matched.
        let mut matched: Vec<Vec<Rv>> = Vec::new();
        if !match_plan.impossible {
            let store = m.store();
            let mut row = vec![Rv::Null; width];
            let max_intermediate = budget.max_intermediate;
            let mut sink = |r: &[Rv]| {
                matched.push(r.to_vec());
                if matched.len() > max_intermediate {
                    return Err(crate::error::GraphError::LimitExceeded(format!(
                        "more than {max_intermediate} rows to update"
                    )));
                }
                Ok(true)
            };
            Matcher::new(store, &match_plan.steps, budget, &mut stats).run(&mut row, &mut sink)?;
        }
        for row in &mut matched {
            budget.check()?;
            updates.apply_row(m, row, &mut stats)?;
        }
        let store = m.store();
        let mut projector = Projector::new(&projection);
        for row in &matched {
            if !projector.push(store, row, budget)? {
                break;
            }
        }
        projector.finish(store, budget)?
    };
    stats.execution_time = exec_start.elapsed();
    stats.rows_returned = rows.len() as u64;
    let plan = (mode == ExplainMode::Profile).then_some(match_plan.explain);
    Ok(QueryResult { columns, rows, stats, plan })
}
