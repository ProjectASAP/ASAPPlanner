//! Stage 1, Pass 1's output: the per-computation candidates attached to the
//! memo groups of the workload's hash-consed DAG
//! ([`PerComputationLogicalMemoCandidates`]), which Pass 2 extends.

use std::rc::Rc;
use std::time::Instant;

use asap_types::ir::operator::operator_properties::Source;
use asap_types::ir::properties::AccuracyScope;

use asap_types::ir::{NonASAPOp, QueryRoot, SchemaDerivationError};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::pass1::per_computation_candidates::{
    with_resized_summaries, ComputationTarget, HowToComputeAggregateMeasure,
    PerComputationCandidates, PruneReason,
};
use crate::pass2::memo::candidates::{try_populate_pass1, CandidateMemo, CandidatePopulationError};
use crate::pass2::memo::{
    check_generation_deadline, CandidateGenerationTimeout, DagOfLogicalMemoGroups, MemoGroupId,
};
use asap_accuracy::estimators::with_failure_probability;

/// One candidate a stage removed from group `group`, and why. `C` is the
/// stage's candidate type.
#[derive(Debug, Clone, PartialEq)]
pub struct PrunedCandidate<C> {
    pub group: MemoGroupId,
    pub candidate: C,
    pub reason: PruneReason,
}

/// Stage 1, Pass 1's output: the workload's memo groups, each with its
/// Pass 1 candidates, and the computation targets they were built from.
#[derive(Debug, Clone)]
pub struct PerComputationLogicalMemoCandidates {
    /// The groups moved from the [`DagOfLogicalMemoGroups`], its query
    /// roots, and every group's candidates (origin
    /// [`Pass1`](crate::pass2::memo::candidates::OriginOfCandidateForMemoGroup::Pass1)).
    pub memo: CandidateMemo,
    /// One per aggregate measure: what Pass 1 chose among.
    pub computation_targets: Vec<ComputationTarget>,
    /// The ways to compute a target's measure that Pass 1 pruned, each on
    /// its target's aggregate group, in pruning order.
    pub pruned_candidates: Vec<PrunedCandidate<HowToComputeAggregateMeasure>>,
}

/// Why [`generate_per_computation_logical_memo_candidates`] failed.
#[derive(Debug, thiserror::Error)]
pub enum PerComputationLogicalMemoCandidatesError {
    /// The inventory does not belong to the DAG of logical memo groups.
    #[error(transparent)]
    Population(#[from] CandidatePopulationError),
    /// The deadline passed while adding the accuracy-scoped sizes.
    #[error(transparent)]
    Timeout(#[from] CandidateGenerationTimeout),
    /// A candidate could not be resized for the whole-result failure budget.
    #[error(transparent)]
    Sizing(#[from] SchemaDerivationError),
}

/// Pass 1's candidates on the memo groups of `dag_of_logical_memo_groups`:
/// each group's candidates from `per_computation`, the Pass 1 inventory of
/// the same roots less what the deployment and early query pruning removed
/// ([`try_populate_pass1`]), then each candidate again with its summaries
/// sized for the workload's whole-result failure budget when that changes
/// it. `entries[i]` is query `i`'s workload entry. Stops with
/// [`PerComputationLogicalMemoCandidatesError::Timeout`] once `deadline`
/// passes.
pub fn generate_per_computation_logical_memo_candidates<Id: Clone>(
    dag_of_logical_memo_groups: DagOfLogicalMemoGroups,
    per_computation: PerComputationCandidates<Id>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    deadline: Option<Instant>,
) -> Result<PerComputationLogicalMemoCandidates, PerComputationLogicalMemoCandidatesError> {
    let aggregate_groups: Vec<Option<MemoGroupId>> = per_computation
        .targets
        .iter()
        .map(|target| dag_of_logical_memo_groups.group_for_node(&target.aggregate))
        .collect();
    let mut memo = try_populate_pass1(dag_of_logical_memo_groups, &per_computation)?;
    add_scoped_candidates(&mut memo, entries, data, deadline)?;
    let pruned_candidates = per_computation
        .pruned_ways_to_compute
        .into_iter()
        .map(|pruned| PrunedCandidate {
            group: aggregate_groups[pruned.target]
                .expect("population checked every target's aggregate group"),
            candidate: pruned.way_to_compute,
            reason: pruned.reason,
        })
        .collect();
    Ok(PerComputationLogicalMemoCandidates {
        memo,
        computation_targets: per_computation.targets,
        pruned_candidates,
    })
}

/// Adds, beside every candidate of `memo`, the candidate with its summaries
/// sized for the strictest whole-result failure budget of `entries`: each
/// approximate query's failure probability over the values it covers (the
/// largest source population for a whole-result scope), shared equally by
/// the candidate's `accuracy_budget_parts`. Adds nothing unless some
/// approximate query has a whole-result scope and every approximate query
/// has a finite positive failure probability and value count. The original
/// candidates stay; final composed accuracy is still checked.
fn add_scoped_candidates(
    memo: &mut CandidateMemo,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    deadline: Option<Instant>,
) -> Result<(), PerComputationLogicalMemoCandidatesError> {
    if !entries.iter().any(|entry| {
        entry.requirements.accuracy.accuracy_scope == AccuracyScope::WholeResult
            && !entry.requirements.accuracy.is_exact()
    }) {
        return Ok(());
    }
    let mut sources = std::collections::HashSet::new();
    for group in &memo.groups {
        if let Some(NonASAPOp::Scan { source, .. }) = group.logical_node.non_asap() {
            sources.insert(match source {
                Source::TimeSeries { metric } => metric.as_str(),
                Source::Table { table_ref } => table_ref.as_str(),
            });
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
            check_generation_deadline(deadline)?;
            let roots = with_resized_summaries(
                &[QueryRoot::Operator(candidate.root_node.clone())],
                &|_, params| {
                    Some(with_failure_probability(
                        params,
                        budget / candidate.accuracy_budget_parts.max(1) as f64,
                    ))
                },
            )?;
            let QueryRoot::Operator(node) = &roots[0] else {
                unreachable!()
            };
            if !Rc::ptr_eq(node, &candidate.root_node) {
                let mut sized = candidate.clone();
                sized.root_node = node.clone();
                additional.push(sized);
            }
        }
        candidates.extend(additional);
    }
    Ok(())
}
