//! Stage 1, Pass 1's entry and output: the per-computation candidates attached to the
//! memo groups of the workload's hash-consed DAG
//! ([`PerComputationLogicalMemoCandidates`]), which Pass 2 extends.

use std::rc::Rc;
use std::time::Instant;

use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::{QueryRoot, SchemaDerivationError};
use asap_types::workload::DataWorkload;

use asap_accuracy::sizing::{
    per_input_failure_probability, with_summaries_sized_for_failure_probability,
};
use asap_accuracy::{AccuracyEvidenceProvider, AccuracyModel};

use crate::deployment_capabilities::SupportedSummary;
use crate::pass1::per_computation_candidates::{
    generate_per_computation_candidates, PerComputationCandidateError,
};
use crate::pass2::memo::candidates::CandidatePopulationError;
use crate::pass2::memo::check_generation_deadline;
use crate::pass2::memo::groups::try_populate_pass1;
use asap_types::stages::{
    CandidateGenerationTimeout, DagOfLogicalMemoGroups, MemoCandidates,
    PerComputationLogicalMemoCandidates, PrunedCandidate,
};

/// Why [`generate_per_computation_logical_memo_candidates`] failed.
#[derive(Debug, thiserror::Error)]
pub enum PerComputationLogicalMemoCandidatesError {
    /// A query's DAG cannot be planned: it already holds ASAP or physical
    /// annotations, or a measure's accuracy target is invalid.
    #[error(transparent)]
    PerComputation(#[from] PerComputationCandidateError),
    /// The targets do not belong to the DAG of logical memo groups.
    #[error(transparent)]
    Population(#[from] CandidatePopulationError),
    /// The deadline passed while sizing the candidates.
    #[error(transparent)]
    Timeout(#[from] CandidateGenerationTimeout),
    /// A candidate's summaries sized for its readers' failure probability
    /// did not rebuild.
    #[error(transparent)]
    Sizing(#[from] SchemaDerivationError),
}

/// Stage 1, Pass 1: the per-computation candidates on the memo groups of
/// `dag_of_logical_memo_groups`. It finds one computation target per
/// measure of each aggregate group with a realization besides exact
/// execution, drops the summaries the deployment does not support
/// (`supported_summaries`, the deployment's
/// `DeploymentCapabilities::supported_summaries`; `None` supports every
/// one) and, unless `accuracy` or `evidence` may accept an output without
/// its guarantee, the summaries no query reading them can use (a query
/// `q`'s target is `accuracy_targets[q]`). It then attaches each group's
/// candidates, each with its summaries sized for its readers' failure
/// probabilities (`size_for_failure_probability`): one sized candidate
/// per accuracy-budget split. Every pruned way to compute a measure is in
/// the output's `pruned_candidates`. Stops with
/// [`PerComputationLogicalMemoCandidatesError::Timeout`] once `deadline`
/// passes while sizing.
pub fn generate_per_computation_logical_memo_candidates(
    dag_of_logical_memo_groups: DagOfLogicalMemoGroups,
    accuracy_targets: &[AccuracyTarget],
    accuracy: &dyn AccuracyModel,
    evidence: &dyn AccuracyEvidenceProvider,
    data: &DataWorkload,
    supported_summaries: Option<&[SupportedSummary]>,
    deadline: Option<Instant>,
) -> Result<PerComputationLogicalMemoCandidates, PerComputationLogicalMemoCandidatesError> {
    let mut per_computation = generate_per_computation_candidates(&dag_of_logical_memo_groups)?
        .without_unsupported_summaries(supported_summaries);
    if !accuracy.may_accept_regardless_of_guarantee() && !evidence.may_provide_evidence() {
        per_computation =
            per_computation.without_summaries_no_query_can_use(accuracy_targets, data, accuracy);
    }
    let mut memo = try_populate_pass1(dag_of_logical_memo_groups, &per_computation)?;
    size_for_failure_probability(
        &mut memo,
        &per_computation.roots,
        accuracy_targets,
        data,
        deadline,
    )?;
    let pruned_candidates = per_computation
        .pruned_ways_to_compute
        .into_iter()
        .map(|pruned| PrunedCandidate {
            group: per_computation.targets[pruned.target].aggregate_group,
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

/// Sizes every candidate of `memo` for its readers' failure probabilities,
/// in place: each approximate input of a candidate with `n`
/// accuracy budget parts (`accuracy_budget_parts`) gets the smallest
/// [`per_input_failure_probability`] `δ_q / (n · V_q)` among the queries `q`
/// reaching its group, `V_q` the values `q`'s target covers (`G` of its
/// root, `roots[q]`, for the whole result)
/// ([`with_summaries_sized_for_failure_probability`]). These are the candidates' final
/// sizes: Pass 2 inherits them or takes the strictest shared reader's
/// ([`size_for_all_readers`](asap_accuracy::estimators::size_for_all_readers)),
/// and Stage 3 only checks the composed accuracy. A candidate no reader
/// gives a failure probability (exact, per-value with no `δ`, or a
/// whole-result target over an unbound `G`) keeps its Pass 1 size.
fn size_for_failure_probability(
    memo: &mut MemoCandidates,
    roots: &[QueryRoot],
    accuracy_targets: &[AccuracyTarget],
    data: &DataWorkload,
    deadline: Option<Instant>,
) -> Result<(), PerComputationLogicalMemoCandidatesError> {
    for (group, candidates) in memo.groups.iter().zip(&mut memo.candidates) {
        for candidate in candidates.iter_mut() {
            check_generation_deadline(deadline)?;
            let Some(failure_probability) = group
                .reached_by_queries()
                .iter()
                .filter_map(|&query| {
                    per_input_failure_probability(
                        accuracy_targets.get(query)?,
                        candidate.accuracy_budget_parts,
                        roots.get(query)?,
                        data,
                    )
                })
                .reduce(f64::min)
            else {
                continue;
            };
            let sized = with_summaries_sized_for_failure_probability(
                &[QueryRoot::Operator(Rc::clone(&candidate.root_node))],
                failure_probability,
                data,
            )?;
            let [QueryRoot::Operator(node)] = &sized[..] else {
                unreachable!("one operator root in, one out")
            };
            candidate.root_node = Rc::clone(node);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pass2::memo::generate_dag_of_logical_memo_groups;
    use crate::pass2::memo::shared_configurations::PaneWidthDomain;
    use crate::test_support::{
        lower_promql, shared_summary_state_groups, sharing_rules_and_shared_state_groups,
    };
    use asap_accuracy::{IgnoreAccuracyGuarantees, NoAccuracyEvidence};
    use asap_types::ir::properties::{AccuracyMetric, AccuracyScope, AccuracyTarget};
    use asap_types::ir::schema::{FieldDataType, SketchAlgorithm, SketchParams};
    use asap_types::ir::{ASAPOp, OperatorNode};
    use asap_types::stages::CandidateForMemoGroup;
    use asap_types::workload::DurationMs;

    /// Rank error 0.01 at δ = 0.01 over `scope`.
    fn rank_target(scope: AccuracyScope) -> AccuracyTarget {
        AccuracyTarget::max_error(0.01)
            .failure_probability(0.01)
            .accuracy_metric(AccuracyMetric::Rank)
            .accuracy_scope(scope)
    }

    /// Metric `x` with `series` series (`None`: undeclared).
    fn data(series: Option<u64>) -> DataWorkload {
        let data = DataWorkload::sampled_every(["x".to_string()], DurationMs(1_000));
        match series {
            Some(series) => data.with_series_count(series),
            None => data,
        }
    }

    /// Pass 1's memo for `queries`, each lowered and entered with its target.
    fn pass1(queries: &[(&str, AccuracyTarget)], data: &DataWorkload) -> MemoCandidates {
        let roots: Vec<QueryRoot> = queries
            .iter()
            .map(|(query, target)| {
                QueryRoot::Operator(lower_promql(
                    query,
                    target.clone().accuracy_scope(AccuracyScope::PerValue),
                ))
            })
            .collect();
        let targets: Vec<AccuracyTarget> =
            queries.iter().map(|(_, target)| target.clone()).collect();
        generate_per_computation_logical_memo_candidates(
            generate_dag_of_logical_memo_groups(&roots),
            &targets,
            &IgnoreAccuracyGuarantees,
            &NoAccuracyEvidence,
            data,
            None,
            None,
        )
        .unwrap()
        .memo
    }

    /// The failure probability of every KLL `candidates` build.
    fn kll_failure_probabilities<'a>(
        candidates: impl IntoIterator<Item = &'a CandidateForMemoGroup>,
    ) -> Vec<Option<f64>> {
        candidates
            .into_iter()
            .flat_map(|candidate| OperatorNode::reachable(&candidate.root_node))
            .filter_map(|node| match node.asap() {
                Some(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, _),
                    ..
                }) if *kind.algorithm() == SketchAlgorithm::Kll => match kind.params() {
                    SketchParams::Kll {
                        failure_probability,
                        ..
                    } => Some(*failure_probability),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    /// The failure probability of every KLL build of `memo`'s candidates
    /// for one accuracy budget part (the whole budget).
    fn whole_share_kll(memo: &MemoCandidates) -> Vec<Option<f64>> {
        kll_failure_probabilities(
            memo.candidates
                .iter()
                .flatten()
                .filter(|candidate| candidate.accuracy_budget_parts == 1),
        )
    }

    /// Pass 1 sizes each sketch for `δ / V`, `V` the values its query's
    /// target covers: one for a per-value target, `⌊1 / share⌋` = 20 for
    /// the groups holding at least 5% of the records (whether or not `G` is
    /// declared), `G` = 10 000 series for the whole result. A whole-result
    /// target over an undeclared `G` leaves the per-value size (KLL's
    /// default failure probability, `None`), which Stage 3 then rejects.
    #[test]
    fn pass1_sizes_each_sketch_for_the_values_its_target_covers() {
        let query = "quantile_over_time(0.99, x[5m])";
        let five_percent = AccuracyScope::GroupsWithRecordShareAtLeast(0.05);
        for (scope, series, failure_probability) in [
            (AccuracyScope::PerValue, Some(10_000), None),
            (five_percent, None, Some(0.01 / 20.0)),
            (five_percent, Some(10_000), Some(0.01 / 20.0)),
            (
                AccuracyScope::WholeResult,
                Some(10_000),
                Some(0.01 / 10_000.0),
            ),
            (AccuracyScope::WholeResult, None, None),
        ] {
            let memo = pass1(&[(query, rank_target(scope))], &data(series));
            let found = whole_share_kll(&memo);
            assert!(!found.is_empty(), "{scope:?}");
            assert!(
                found.iter().all(|f| *f == failure_probability),
                "{scope:?} {series:?}: {found:?}"
            );
        }
    }

    /// A group several queries reach is sized for the strictest of them:
    /// the per-series quantiles are both a whole-result query's value (over
    /// `G` = 10 000 series) and the input of a global `max` (one value), so
    /// they take `δ / 10 000`.
    #[test]
    fn a_group_several_queries_reach_takes_the_strictest_failure_probability() {
        let whole = rank_target(AccuracyScope::WholeResult);
        let memo = pass1(
            &[
                ("quantile_over_time(0.99, x[5m])", whole.clone()),
                ("max(quantile_over_time(0.99, x[5m]))", whole),
            ],
            &data(Some(10_000)),
        );
        let found = whole_share_kll(&memo);
        assert!(!found.is_empty());
        assert!(
            found.iter().all(|f| *f == Some(0.01 / 10_000.0)),
            "{found:?}"
        );
    }

    /// Pass 2 shares one build among readers sized differently in Pass 1
    /// at the strictest reader's size: a per-value 0.95 quantile (δ) and a
    /// whole-result 0.99 quantile over `G` = 10 000 series (δ / 10 000)
    /// share a KLL sized for δ / 10 000. Stage 3 does not resize it.
    #[test]
    fn a_shared_build_takes_its_strictest_readers_size() {
        let memo = pass1(
            &[
                (
                    "quantile_over_time(0.95, x[5m])",
                    rank_target(AccuracyScope::PerValue),
                ),
                (
                    "quantile_over_time(0.99, x[5m])",
                    rank_target(AccuracyScope::WholeResult),
                ),
            ],
            &data(Some(10_000)),
        );
        let memo = sharing_rules_and_shared_state_groups(memo, PaneWidthDomain::SeedGrids).unwrap();
        let found = kll_failure_probabilities(
            shared_summary_state_groups(&memo)
                .iter()
                .filter(|group| memo.groups[group.0 as usize].reached_by_queries() == [0, 1])
                .flat_map(|group| &memo.candidates[group.0 as usize]),
        );
        assert!(!found.is_empty());
        assert!(
            found.iter().all(|f| *f == Some(0.01 / 10_000.0)),
            "{found:?}"
        );
    }
}
