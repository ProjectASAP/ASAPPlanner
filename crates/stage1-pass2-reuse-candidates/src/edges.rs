//! Step 6, the logical edges: every possible $(u, c, i)$ of the memo with
//! $\alpha^{\log}$, which dead-candidate elimination reads and whose
//! satisfied edges between survivors are Pass 2's output.

use std::collections::BTreeMap;

use crate::evaluation_grids;
use asap_accuracy::{accepted_metrics, producer_satisfies_logical_requirements, AccuracyEvidence};
use asap_types::ir::{ASAPOp, Operator};
use asap_types::stages::{CandidateForMemoGroupId, Memo, MemoGroupId, RootPruneReason};
use asap_types::workload::QueryWorkloadEntry;

/// One possible $(u, c, i)$: `producer` is a candidate of the group input
/// `input` of `consumer` reads, and whether it satisfies that input's
/// logical requirement (`producer_satisfies_logical_requirements`).
pub(crate) struct CandidateEdge {
    pub(crate) producer: CandidateForMemoGroupId,
    pub(crate) consumer: CandidateForMemoGroupId,
    pub(crate) input: usize,
    pub(crate) satisfies: Result<(), RootPruneReason>,
}

/// Every $(u, c, i)$ of `memo` (`Memo::producers`), grouped by consumer and
/// input, producers in group order. An edge into a readout of
/// `unknown_values` fails with `ValuesPerEvaluationUnknown`. A chain
/// `SummaryTumblingWindowChain[w]` has an edge only into a consumer group
/// whose result does not depend on the evaluation time or whose
/// evaluation grid ([`evaluation_grids`]) $w$ divides: a chain of another
/// reader's width does not end at this reader's evaluations.
pub(crate) fn candidate_edges(
    memo: &Memo,
    entries: &[QueryWorkloadEntry],
    unknown_values: &BTreeMap<CandidateForMemoGroupId, usize>,
    evidence: &dyn AccuracyEvidence,
) -> Vec<CandidateEdge> {
    let accepted = accepted_metrics(memo);
    let evaluation_grid_of_group = evaluation_grids(memo, entries);
    let mut edges = Vec::new();
    for (group, memo_group) in memo.groups().iter().enumerate() {
        let ends_on_the_evaluations =
            |input_group: MemoGroupId| match memo.groups()[input_group.0 as usize].expression {
                Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                    tumbling_window_width_ms,
                    ..
                }) if memo_group.group_properties.depends_on_evaluation_time => {
                    evaluation_grid_of_group[group]
                        .is_some_and(|grid| grid.period_ms.is_multiple_of(tumbling_window_width_ms))
                }
                _ => true,
            };
        for (index_in_group, candidate) in memo_group.candidates.iter().enumerate() {
            let consumer = CandidateForMemoGroupId {
                group: MemoGroupId(group as u32),
                index_in_group,
            };
            for (input, &input_group) in candidate
                .expression
                .operator
                .children()
                .into_iter()
                .enumerate()
            {
                if !ends_on_the_evaluations(input_group) {
                    continue;
                }
                for producer in memo.producers(consumer, input) {
                    edges.push(CandidateEdge {
                        producer,
                        consumer,
                        input,
                        satisfies: match unknown_values.get(&consumer) {
                            Some(&query) => {
                                Err(RootPruneReason::ValuesPerEvaluationUnknown { consumer, query })
                            }
                            None => producer_satisfies_logical_requirements(
                                memo, &accepted, producer, consumer, input, evidence,
                            ),
                        },
                    });
                }
            }
        }
    }
    edges
}
