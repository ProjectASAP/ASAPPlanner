//! Pins the candidates the shared-producer cases generate, recorded before
//! they moved onto one generation path: each workload's expanded memo
//! (groups, candidates, their origins and children) hashes to a fixed value.
//! The top-k pins were re-recorded when equal keyless results became one
//! group: the readers' shared `sum_over_time` is now one group, not one per
//! reader.
use super::candidates::{try_populate_pass1, CandidateMemo, OriginOfCandidateForMemoGroup};
use super::shared_configurations::PaneWidthDomain;
use super::window_candidates::add_local_window_composition;
use super::DagOfLogicalMemoGroups;
use crate::pass1::per_computation_candidates::generate_per_computation_candidates;
use crate::test_support::{entry_with, lower_promql, sharing_rules_and_shared_state_groups};
use asap_types::ir::flat::flatten;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::QueryRoot;
use asap_types::workload::{QueryRecurrence, RepeatedDemand, RepetitionInterval};
use std::rc::Rc;

/// The expanded memo of `queries` (each with its error bound), every one
/// repeated every `step_ms` milliseconds, with local pane candidates.
fn expanded(queries: &[(&str, f64)], step_ms: u32) -> CandidateMemo {
    let targets: Vec<_> = queries
        .iter()
        .map(|&(_, error)| {
            if error == 0.0 {
                AccuracyTarget::exact()
            } else {
                AccuracyTarget::max_error(error).failure_probability(0.001)
            }
        })
        .collect();
    let roots: Vec<_> = queries
        .iter()
        .zip(&targets)
        .enumerate()
        .map(|(id, ((query, _), target))| {
            (id, QueryRoot::Operator(lower_promql(query, target.clone())))
        })
        .collect();
    let initial = DagOfLogicalMemoGroups::from_roots(&roots);
    let inventory = generate_per_computation_candidates(roots).unwrap();
    let mut memo = try_populate_pass1(initial, &inventory).unwrap();
    let entries: Vec<_> = targets
        .into_iter()
        .map(|target| {
            entry_with(
                target,
                QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
                    step_ms,
                ))),
            )
        })
        .collect();
    add_local_window_composition(&mut memo, &entries);
    sharing_rules_and_shared_state_groups(memo, PaneWidthDomain::SeedGrids).unwrap()
}

/// Whether one `sum_over_time` group (an aggregate directly over a window
/// of `metric`) is reached by every query in `queries`.
fn one_sum_over_time_group_for(memo: &CandidateMemo, metric: &str, queries: &[usize]) -> bool {
    use asap_types::ir::operator::Source;
    use asap_types::ir::NonASAPOp;
    memo.groups.iter().any(|group| {
        let Some(NonASAPOp::Aggregate { child, .. }) = group.logical_node.non_asap() else {
            return false;
        };
        let Some(NonASAPOp::TimeRange { child: scan, .. }) = child.non_asap() else {
            return false;
        };
        matches!(scan.non_asap(), Some(NonASAPOp::Scan { source: Source::TimeSeries { metric: m }, .. }) if m == metric)
            && queries.iter().all(|query| group.queries.contains(query))
    })
}

fn signature(node: &Rc<asap_types::ir::OperatorNode>) -> String {
    serde_json::to_string(&flatten(&[QueryRoot::Operator(Rc::clone(node))]).0).unwrap()
}

/// FNV-1a over every group (node, edges, queries, roots) and its candidates
/// (origin, budget share, implemented and child groups, node), in memo
/// order: the search's tie-breaks depend on that order too.
fn fingerprint(memo: &CandidateMemo) -> (usize, u64) {
    let mut entries: Vec<String> = Vec::new();
    for (index, (group, candidates)) in memo.groups.iter().zip(&memo.candidates).enumerate() {
        let own: Vec<String> = candidates
            .iter()
            .map(|candidate| {
                format!(
                    "{:?}|{}|{:?}|{:?}|{}",
                    candidate.origin,
                    candidate.accuracy_budget_parts,
                    std::iter::once(super::MemoGroupId(index as u32))
                        .chain(candidate.fused_groups.iter().copied())
                        .collect::<Vec<_>>(),
                    candidate.input_groups,
                    signature(&candidate.root_node)
                )
            })
            .collect();
        entries.push(format!(
            "{}|{:?}|{:?}|{:?}|{:?}=>{:?}",
            signature(&group.logical_node),
            group.child_groups,
            group.parents,
            group.queries,
            super::queries_rooted_at(&memo.query_roots, super::MemoGroupId(index as u32)),
            own
        ));
    }
    let count = memo.candidates.iter().map(Vec::len).sum();
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in entries.join("\n").bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    (count, hash)
}

/// Top-k readers of one ranking with different `k`.
#[test]
fn top_k_reuse_candidates_are_unchanged() {
    let memo = expanded(
        &[
            ("topk(3, sum_over_time(m_gauge[2m]))", 0.0),
            ("topk(10, sum_over_time(m_gauge[2m]))", 0.0),
        ],
        60_000,
    );
    assert!(memo
        .candidates
        .iter()
        .flatten()
        .any(|candidate| candidate.origin == OriginOfCandidateForMemoGroup::TopKReuse));
    assert!(one_sum_over_time_group_for(&memo, "m_gauge", &[0, 1]));
    assert_eq!(fingerprint(&memo), (69, 14825718632741491576));
}

/// Quantile readers of one input at different sizes.
#[test]
fn shared_size_candidates_are_unchanged() {
    let memo = expanded(
        &[
            ("quantile_over_time(0.95, latency[5m])", 0.01),
            ("quantile_over_time(0.5, latency[5m])", 0.05),
        ],
        6_000,
    );
    assert_eq!(fingerprint(&memo), (508, 7457272222287953080));
}

/// Nested windows of one sum, sharing a pane chain.
#[test]
fn shared_pane_candidates_are_unchanged() {
    let memo = expanded(
        &[
            ("sum(sum_over_time(m_gauge[2m]))", 0.0),
            ("sum(sum_over_time(m_gauge[1m]))", 0.0),
        ],
        60_000,
    );
    assert!(memo.candidates.iter().flatten().any(|candidate| {
        asap_types::ir::OperatorNode::reachable(&candidate.root_node)
            .iter()
            .any(|node| matches!(node.asap(), Some(asap_types::ir::ASAPOp::PaneChain { .. })))
    }));
    assert_eq!(fingerprint(&memo), (729, 15532247561651645380));
}

/// Two top-k sets over different inputs, next to a pane set: pins the order
/// sets are expanded in.
#[test]
fn several_top_k_sets_are_unchanged() {
    let memo = expanded(
        &[
            ("topk(3, sum_over_time(m_gauge[2m]))", 0.0),
            ("topk(10, sum_over_time(m_gauge[2m]))", 0.0),
            ("topk(5, sum_over_time(m_gauge[2m]))", 0.0),
            ("topk(2, sum_over_time(other[2m]))", 0.0),
            ("topk(4, sum_over_time(other[2m]))", 0.0),
            ("sum(sum_over_time(other[1m]))", 0.0),
        ],
        60_000,
    );
    assert!(one_sum_over_time_group_for(&memo, "m_gauge", &[0, 1, 2]));
    assert!(one_sum_over_time_group_for(&memo, "other", &[3, 4]));
    assert_eq!(fingerprint(&memo), (504, 3265274154981812875));
}
