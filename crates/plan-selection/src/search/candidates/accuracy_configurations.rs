//! Additional finite capacities for whole-result failure budgets.
//! Original capacities remain available; final composed accuracy is still checked.
use asap_logical_asap_optimization::memo::candidates::CandidateMemo;
use asap_types::ir::{properties::AccuracyScope, QueryRoot};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};
use std::{rc::Rc, time::Instant};

pub(crate) fn add_scoped_candidates(
    memo: &mut CandidateMemo,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    deadline: Option<Instant>,
) -> Result<(), String> {
    if !entries.iter().any(|entry| {
        entry.requirements.accuracy.accuracy_scope == AccuracyScope::WholeResult
            && !entry.requirements.accuracy.is_exact()
    }) {
        return Ok(());
    }
    let mut sources = std::collections::HashSet::new();
    for group in &memo.groups {
        if let Some(asap_types::ir::NonASAPOp::Scan { source, .. }) = group.node.non_asap() {
            sources.insert(crate::source_name(source));
        }
    }
    let population = sources
        .into_iter()
        .map(|source| {
            data.sources
                .get(source)
                .and_then(|facts| facts.series_count)
                .filter(|count| *count > 0)
        })
        .collect::<Option<Vec<_>>>()
        .and_then(|counts| counts.into_iter().max());
    let mut budget: Option<f64> = None;
    for entry in entries {
        let target = &entry.requirements.accuracy;
        if target.is_exact() {
            continue;
        }
        let Some(delta) = target
            .failure_probability
            .filter(|d| d.is_finite() && *d > 0.0)
        else {
            return Ok(());
        };
        let count = match target.accuracy_scope {
            AccuracyScope::WholeResult => population,
            scope => scope.max_values_covered(None),
        };
        let Some(count) = count.filter(|count| *count > 0) else {
            return Ok(());
        };
        let delta = delta / count as f64;
        if delta <= 0.0 {
            return Ok(());
        }
        budget = Some(budget.map_or(delta, |prior| prior.min(delta)));
    }
    let Some(budget) = budget else {
        return Ok(());
    };
    for candidates in &mut memo.candidates {
        let mut additional = Vec::new();
        for candidate in candidates.iter() {
            super::check_deadline(deadline).map_err(|e| e.to_string())?;
            let roots = crate::with_resized_summaries(
                &[QueryRoot::Operator(candidate.node.clone())],
                &|_, params| Some(asap_logical_asap_optimization::accuracy::estimators::with_failure_probability(
                    params, budget / candidate.budget_share.max(1) as f64)),
            ).map_err(|e| e.to_string())?;
            let QueryRoot::Operator(node) = &roots[0] else {
                unreachable!()
            };
            if !Rc::ptr_eq(node, &candidate.node) {
                let mut sized = candidate.clone();
                sized.node = node.clone();
                additional.push(sized);
            }
        }
        candidates.extend(additional);
    }
    Ok(())
}
