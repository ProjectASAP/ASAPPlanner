//! The selected plan over the memo, for the selection report
//! ([`crate::MemoPlanReport`]): each group with the implementation candidate
//! the solution chose for it, and the coupling sites its choices read at
//! their chosen thresholds.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::search::memo::rules::{capability, scans, windows};
use crate::search::memo::{CouplingKind, CouplingSite, GroupId, Memo};
use crate::search::milp::model::ModelInput;
use crate::{ChosenCandidateReport, CouplingSiteReport, MemoGroupReport, MemoPlanReport};

/// The report of the solution that chose `chosen` (indices into
/// `input.choices`) with `built_once` groups, over `memo`, whose queries are
/// the workload's `queries`.
pub(super) fn memo_plan_report(
    memo: &Memo,
    input: &ModelInput,
    chosen: &[usize],
    built_once: &BTreeSet<GroupId>,
    queries: &[usize],
) -> MemoPlanReport {
    let mut groups: Vec<MemoGroupReport> = memo
        .groups
        .iter()
        .enumerate()
        .map(|(g, group)| {
            let mut children = group.children.iter();
            let operator = group
                .node
                .operator
                .map_children(|_| children.next().map(|c| c.0 as usize));
            MemoGroupReport {
                group: g,
                operator,
                parents: group.parents.iter().map(|p| p.0 as usize).collect(),
                queries: group.queries.iter().map(|&q| queries[q]).collect(),
                roots_of: group.roots_of.iter().map(|&q| queries[q]).collect(),
                candidates: memo.candidates[g].len(),
                chosen: None,
                implemented_by: None,
            }
        })
        .collect();
    // Each site at its chosen threshold, with the groups reading it.
    let mut sites: BTreeMap<(CouplingSite, usize), BTreeSet<usize>> = BTreeMap::new();
    let mut chosen_sites: Vec<(usize, Vec<(CouplingSite, usize)>)> = Vec::new();
    for &c in chosen {
        let choice = &input.choices[c];
        let g = choice.group.0 as usize;
        let candidate = &memo.candidates[g][choice.candidate];
        let read: Vec<(CouplingSite, usize)> = input.site_thresholds[c]
            .iter()
            .filter_map(|&(key, t)| {
                let site = candidate.coupling_sites.iter().find(|s| s.key == key)?;
                Some((*site, t))
            })
            .collect();
        for at in &read {
            sites.entry(*at).or_default().insert(g);
        }
        chosen_sites.push((g, read));
        for implemented in &choice.implements[1..] {
            groups[implemented.0 as usize].implemented_by = Some(g);
        }
        groups[g].chosen = Some(ChosenCandidateReport {
            origin: format!("{:?}", candidate.origin),
            operator: candidate.node.operator.kind_name().to_string(),
            implements: choice.implements.iter().map(|i| i.0 as usize).collect(),
            reads: choice.children.iter().map(|i| i.0 as usize).collect(),
            built_once: built_once.contains(&choice.group),
            coupling_sites: Vec::new(),
        });
    }
    let index: BTreeMap<(CouplingSite, usize), usize> =
        sites.keys().enumerate().map(|(i, at)| (*at, i)).collect();
    for (g, read) in chosen_sites {
        if let Some(chosen) = &mut groups[g].chosen {
            chosen.coupling_sites = read.iter().map(|at| index[at]).collect();
        }
    }
    let coupling_sites = sites
        .into_iter()
        .map(|((site, t), readers)| {
            let (threshold, thresholds) = threshold_of(memo, &site, t);
            CouplingSiteReport {
                kind: format!("{:?}", site.kind),
                threshold,
                thresholds,
                readers: readers.into_iter().collect(),
            }
        })
        .collect();
    MemoPlanReport {
        groups,
        coupling_sites,
        scans_widened: false,
    }
}

/// `site`'s threshold `t` as JSON, and how many thresholds it has.
fn threshold_of(memo: &Memo, site: &CouplingSite, t: usize) -> (Value, usize) {
    fn at<T>(all: Vec<T>, t: usize, json: impl Fn(&T) -> Value) -> (Value, usize) {
        (all.get(t).map_or(Value::Null, json), all.len())
    }
    match site.kind {
        CouplingKind::PaneChain => at(
            windows::chain_thresholds(memo, site),
            t,
            |c| json!({ "pane_width_ms": c.pane_width_ms, "longest_window_ms": c.longest_window_ms, "family": c.family }),
        ),
        CouplingKind::ScanLabels => at(
            scans::label_thresholds(memo, site),
            t,
            |l| json!({ "labels": l.labels }),
        ),
        CouplingKind::SharedBuildSize if windows::is_rollup_state(memo, site) => at(
            windows::state_size_thresholds(memo, site),
            t,
            |family| json!({ "state": family }),
        ),
        CouplingKind::SharedBuildSize => at(
            capability::size_thresholds(memo, site),
            t,
            |s| json!({ "params": s.params }),
        ),
        CouplingKind::RawHorizon => at(
            scans::horizon_thresholds(memo, site),
            t,
            |h| json!({ "horizon_ms": h.horizon_ms }),
        ),
    }
}
