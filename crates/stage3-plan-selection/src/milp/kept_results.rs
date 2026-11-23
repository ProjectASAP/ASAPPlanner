//! The data plane's result cache and answer cache in the selection MILP
//! (`docs/design_docs/proposals/cost-terms-from-calibration.md`, section
//! 2), charged as `plan_cost` charges them.
//!
//! - **Kept query-time results.** Each execution's are in its own cost
//!   (`ExecutionCosts::kept_result_bytes_per_execution`). Under a bound
//!   $B$ the cache holds $\min(S, B)$ of their sum $S = \sum k_e x_e$; the
//!   selection charges $t \ge S - M b$, $t \ge B b$ ($b$ binary, $M$ the
//!   largest $S$ less $B$) in place of $S$: $t = \min(S, B)$ at the optimum, as its
//!   quantity per byte is positive.
//! - **Answers.** Query $q$ keeps one answer per evaluation time within the
//!   plan's longest retention $H$ (in seconds) plus the margin $m$:
//!   $a_e\,((H + m)/p_q + 1)$ bytes for the execution $e$ answering it
//!   ($z_{q,e}$), $p_q$ its period. $H \ge h_v$ for every $h_v$, and $H
//!   z_{q,e}$ is $w_{q,e} \ge H - \bar H (1 - z_{q,e})$, $w \ge 0$, $\bar
//!   H$ the largest retention any read needs: exact for binary $z$ as $w$'s
//!   quantity is positive. A bound is $\min$ as above.

use std::collections::{BTreeMap, HashMap};

use good_lp::{constraint, variable, Constraint, Expression, ProblemVariables, Variable};

use asap_cost::plan::kept_results::{held_by_bounded_cache, kept_per_grid};
use asap_types::stages::PhysicalCandidateId;

use super::model::SelectionInputs;
use crate::cost::ExecutionCosts;
use crate::retention::ms_to_seconds;
use crate::selectable::QueryId;

/// The variables the terms read: $x$ by execution, $z$ by (query,
/// execution), $h_v$ in seconds by candidate.
pub(crate) struct KeptResultVariables<'a> {
    pub(crate) x: &'a [Variable],
    pub(crate) z: &'a BTreeMap<(QueryId, usize), Variable>,
    pub(crate) h: &'a BTreeMap<PhysicalCandidateId, Variable>,
}

/// The result cache's and the answer cache's terms of `costs`' quantity
/// (module doc), with their variables added to `problem` and their rows to
/// `rows`: each (quantity per unit, variable), unscaled. `kept` by
/// execution position: an execution left out keeps nothing.
pub(crate) fn kept_result_terms(
    inputs: &SelectionInputs<'_>,
    costs: &ExecutionCosts,
    variables: &KeptResultVariables<'_>,
    kept: &[bool],
    problem: &mut ProblemVariables,
    rows: &mut Vec<Constraint>,
) -> Vec<(f64, Variable)> {
    let Some(per_byte) = costs.per_kept_byte else {
        return Vec::new();
    };
    let mut terms = Vec::new();
    // Kept query-time results under a bound: min(S, B) in place of S.
    if let Some(max_bytes) = costs
        .kept_query_time_results
        .and_then(|results| results.max_bytes)
    {
        let mut sum = Expression::default();
        let mut largest = 0.0;
        for (index, &bytes) in costs.kept_result_bytes_per_execution.iter().enumerate() {
            if bytes > 0.0 && kept[index] {
                sum += bytes * variables.x[index];
                largest += bytes;
                terms.push((-per_byte * bytes, variables.x[index]));
            }
        }
        if largest > 0.0 {
            terms.push((
                per_byte,
                bounded_pool(sum, largest, max_bytes as f64, problem, rows),
            ));
        }
    }
    // Answers, over the plan's longest retention H.
    if let Some(answers) = costs.answer_cache {
        let longest = variables
            .h
            .keys()
            .filter_map(|&v| largest_retention_ms(inputs, costs, v))
            .max()
            .unwrap_or(0);
        let horizon = problem.add(variable().min(0).max(ms_to_seconds(longest)));
        for &h in variables.h.values() {
            rows.push(constraint!(horizon >= h));
        }
        let mut pool = Expression::default();
        let mut largest = 0.0;
        let mut pool_terms = Vec::new();
        for (query, answering) in inputs.selectable.answering() {
            let Some(entry) = inputs.entries.get(query.0) else {
                continue;
            };
            let period_ms = entry.evaluation_grid().period_ms.max(1) as f64;
            for &index in answering {
                let (Some(&z), Some(&bytes)) = (
                    variables.z.get(&(*query, index)),
                    costs.answer_bytes_per_execution.get(index),
                ) else {
                    continue;
                };
                if bytes <= 0.0 {
                    continue;
                }
                // a ((H + m)/p + 1) = a (m/p + 1) z + (1,000 a/p) H z.
                let at_margin =
                    bytes * kept_per_grid(answers.retention_margin_ms, period_ms as u64);
                let per_second = bytes * 1_000.0 / period_ms;
                let w = problem.add(variable().min(0));
                rows.push(constraint!(w >= horizon - ms_to_seconds(longest) * (1 - z)));
                pool += at_margin * z + per_second * w;
                largest += at_margin + per_second * ms_to_seconds(longest);
                pool_terms.push((at_margin, z));
                pool_terms.push((per_second, w));
            }
        }
        match answers.max_bytes {
            Some(max_bytes) if largest > max_bytes as f64 => terms.push((
                per_byte,
                bounded_pool(pool, largest, max_bytes as f64, problem, rows),
            )),
            _ => terms.extend(
                pool_terms
                    .into_iter()
                    .map(|(bytes, variable)| (per_byte * bytes, variable)),
            ),
        }
    }
    terms
}

/// $t = \min(\text{pool}, \text{bound})$ at the optimum for a positive
/// quantity on $t$: $t \ge \text{pool} - M b$, $t \ge \text{bound}\, b$,
/// $b$ binary, $M$ = `largest` − `bound` (`largest` the largest the pool can
/// be): with $b = 1$, $t \ge$ the bound and the first row is no tighter.
fn bounded_pool(
    pool: Expression,
    largest: f64,
    bound: f64,
    problem: &mut ProblemVariables,
    rows: &mut Vec<Constraint>,
) -> Variable {
    let held = problem.add(variable().min(0));
    let over = problem.add(variable().binary());
    rows.push(constraint!(held >= pool - (largest - bound) * over));
    rows.push(constraint!(held >= bound * over));
    held
}

/// The largest retention, in ms, any read of candidate `v` needs
/// (`ExecutionCosts::retention_ms_for_need`): the most its $h_v$ can be.
fn largest_retention_ms(
    inputs: &SelectionInputs<'_>,
    costs: &ExecutionCosts,
    v: PhysicalCandidateId,
) -> Option<u64> {
    let executions = inputs.selectable.executions();
    inputs
        .selectable
        .reads_back(&costs.retention_need_ms_per_edge)
        .filter(|&(index, _, _)| executions[index].candidate == v)
        .filter_map(|(_, _, need_ms)| Some(costs.retention_ms_for_need(v, need_ms?)))
        .max()
}

/// The result cache's and the answer cache's quantity for a selection
/// (`selected` executions, `answering` query outputs, `retention_ms` the
/// retention of each candidate with $h_v$), as `plan_cost` charges it:
/// the kept results' correction to their bound, $\min(S, B) - S$, and the
/// answers over the longest retention.
pub(crate) fn kept_results_quantity(
    inputs: &SelectionInputs<'_>,
    costs: &ExecutionCosts,
    selected: &[usize],
    answering: &[(QueryId, usize)],
    retention_ms: &HashMap<PhysicalCandidateId, u64>,
) -> f64 {
    let Some(per_byte) = costs.per_kept_byte else {
        return 0.0;
    };
    let mut quantity = 0.0;
    if let Some(results) = costs.kept_query_time_results {
        let sum: f64 = selected
            .iter()
            .filter_map(|&index| costs.kept_result_bytes_per_execution.get(index))
            .sum();
        quantity += per_byte * (held_by_bounded_cache(sum, results.max_bytes) - sum);
    }
    if let Some(answers) = costs.answer_cache {
        let horizon_ms = retention_ms
            .values()
            .copied()
            .max()
            .unwrap_or(0)
            .saturating_add(answers.retention_margin_ms);
        let sum: f64 = answering
            .iter()
            .filter_map(|&(query, index)| {
                let entry = inputs.entries.get(query.0)?;
                let bytes = costs.answer_bytes_per_execution.get(index)?;
                Some(bytes * kept_per_grid(horizon_ms, entry.evaluation_grid().period_ms))
            })
            .sum();
        quantity += per_byte * held_by_bounded_cache(sum, answers.max_bytes);
    }
    quantity
}
