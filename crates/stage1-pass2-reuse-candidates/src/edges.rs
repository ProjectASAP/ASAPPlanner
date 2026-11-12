//! Steps 4 to 6, the logical edges: every possible $(u, c, i)$ of the memo with
//! $\alpha^{\log}$, which dead-candidate elimination reads and whose
//! satisfied edges between survivors are Pass 2's output.

use std::collections::BTreeMap;

use asap_accuracy::{AccuracyEvidence, ErrorMetricsOfMemo};
use asap_types::stages::{
    CandidateForMemoGroupId, EvaluationGrid, Memo, MemoGroupId, RootPruneReason,
};

use crate::evaluation_grids::chain_off_the_evaluation_grid;

/// One possible $(u, c, i)$: `producer` is a candidate of the group input
/// `input` of `consumer` reads, and whether it satisfies that input's
/// logical requirement (`ErrorMetricsOfMemo::edge_meets_logical_requirements`).
pub(crate) struct CandidateEdge {
    pub(crate) producer: CandidateForMemoGroupId,
    pub(crate) consumer: CandidateForMemoGroupId,
    pub(crate) input: usize,
    pub(crate) satisfies: Result<(), RootPruneReason>,
}

/// Every $(u, c, i)$ of `memo` (`Memo::producers`), grouped by consumer and
/// input, producers in group order; `error_metrics` is
/// `ErrorMetricsOfMemo::of` `memo`. An edge into a readout of
/// `readouts_without_per_value_failure_probability` fails with
/// `ValuesPerEvaluationUnknown`. An edge from a chain whose windows do not
/// end at the consuming group's evaluation times
/// ([`chain_off_the_evaluation_grid`], from `evaluation_grid_of_group`)
/// fails with `ChainWidthDoesNotDivideEvaluationGrid`: a chain of another
/// reader's width does not end at this reader's evaluations.
pub(crate) fn candidate_edges(
    memo: &Memo,
    error_metrics: &ErrorMetricsOfMemo,
    evaluation_grid_of_group: &[Option<EvaluationGrid>],
    readouts_without_per_value_failure_probability: &BTreeMap<CandidateForMemoGroupId, usize>,
    evidence: &dyn AccuracyEvidence,
) -> Vec<CandidateEdge> {
    let mut edges = Vec::new();
    for (group, memo_group) in memo.groups().iter().enumerate() {
        let group = MemoGroupId(group as u32);
        for (index_in_group, candidate) in memo_group.candidates.iter().enumerate() {
            let consumer = CandidateForMemoGroupId {
                group,
                index_in_group,
            };
            for (input, &input_group) in candidate
                .expression
                .operator
                .children()
                .into_iter()
                .enumerate()
            {
                let chain_off_the_grid = chain_off_the_evaluation_grid(
                    memo,
                    evaluation_grid_of_group,
                    group,
                    input_group,
                );
                for producer in memo.producers(consumer, input) {
                    edges.push(CandidateEdge {
                        producer,
                        consumer,
                        input,
                        satisfies: match (
                            chain_off_the_grid,
                            readouts_without_per_value_failure_probability.get(&consumer),
                        ) {
                            (Some((tumbling_window_width_ms, evaluation_period_ms)), _) => {
                                Err(RootPruneReason::ChainWidthDoesNotDivideEvaluationGrid {
                                    consumer,
                                    input,
                                    tumbling_window_width_ms,
                                    evaluation_period_ms,
                                })
                            }
                            (None, Some(&query)) => {
                                Err(RootPruneReason::ValuesPerEvaluationUnknown { consumer, query })
                            }
                            (None, None) => error_metrics.edge_meets_logical_requirements(
                                memo, producer, consumer, input, evidence,
                            ),
                        },
                    });
                }
            }
        }
    }
    edges
}
