//! Pins the candidates the shared-producer cases generate, recorded before
//! they moved onto one generation path: each workload's expanded memo
//! (groups, candidates, their origins and children) hashes to a fixed value.
use super::candidates::{try_populate_pass1, CandidateMemo, CandidateOrigin};
use super::state_configurations::PaneWidthDomain;
use super::state_groups::expand_concrete_summary_state_groups_in_domain;
use super::window_candidates::add_local_window_composition;
use super::LogicalMemo;
use crate::per_computation_candidates::generate_per_computation_candidates;
use crate::test_support::{entry_with, lower_promql};
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
    let initial = LogicalMemo::from_roots(&roots);
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
    expand_concrete_summary_state_groups_in_domain(memo, PaneWidthDomain::SeedGrids)
        .unwrap()
        .memo
}

fn signature(node: &Rc<asap_types::ir::OperatorNode>) -> String {
    serde_json::to_string(&flatten(&[QueryRoot::Operator(Rc::clone(node))]).0).unwrap()
}

/// FNV-1a over every group's node and its candidates (origin, children and
/// nodes), independent of the order groups and candidates were added in.
fn fingerprint(memo: &CandidateMemo) -> (usize, u64) {
    let mut entries: Vec<String> = Vec::new();
    for (group, candidates) in memo.groups.iter().zip(&memo.candidates) {
        let mut own: Vec<String> = candidates
            .iter()
            .map(|candidate| {
                let children: Vec<String> = candidate
                    .children
                    .iter()
                    .map(|child| signature(&memo.groups[child.0 as usize].node))
                    .collect();
                format!(
                    "{:?}|{}|{:?}|{}",
                    candidate.origin,
                    candidate.budget_share,
                    children,
                    signature(&candidate.node)
                )
            })
            .collect();
        own.sort();
        entries.push(format!("{}=>{:?}", signature(&group.node), own));
    }
    entries.sort();
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
        .any(|candidate| candidate.origin == CandidateOrigin::TopKReuse));
    assert_eq!(fingerprint(&memo), (313, 13956034881628269997));
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
    assert_eq!(fingerprint(&memo), (508, 2366282220866866287));
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
        asap_types::ir::OperatorNode::reachable(&candidate.node)
            .iter()
            .any(|node| matches!(node.asap(), Some(asap_types::ir::ASAPOp::PaneChain { .. })))
    }));
    assert_eq!(fingerprint(&memo), (729, 17784816355208127629));
}
