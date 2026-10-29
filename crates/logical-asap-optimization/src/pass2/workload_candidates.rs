//! Logical sharing and window rewrites for an explicitly chosen computation assignment.
//! Production search organizes local candidates in the memo rather than enumerating
//! Cartesian products of workload choices.

use asap_types::ir::flat::flatten;
use asap_types::ir::{NonASAPOp, QueryRoot};
use asap_types::workload::QueryWorkloadEntry;

use crate::pass1::per_computation_candidates::{
    compose_logical_candidate, PerComputationCandidateError, PerComputationCandidates,
};
use crate::pass2::asap_aware_cse::{
    capability_sharing_candidates, identical_expression_pass2_candidates, pane_candidates,
    rollup_candidates, rollup_choices, rollup_pass2_candidates, sharing_candidates,
    summary_capability_pass2_candidates, topk_reuse_candidates, topk_reuse_pass2_candidates,
    window_composition_pass2_candidates, with_union_scan_labels, MAX_REUSE_CANDIDATES,
};

/// Apply sharing and window rules to one explicit Pass 1 assignment: the
/// composed candidate's roots (unless not viable), then each of its Pass 2
/// candidates'. `entries[q]` supplies query `q`'s evaluation grid.
pub fn stage1_candidates_of_choice<Id: Clone>(
    per_computation: &PerComputationCandidates<Id>,
    entries: &[QueryWorkloadEntry],
    choice: &[usize],
    viable: &dyn Fn(&[QueryRoot]) -> bool,
) -> Vec<Result<Vec<QueryRoot>, PerComputationCandidateError>> {
    let evaluation_grid_ms: Vec<Option<u64>> = entries
        .iter()
        .map(QueryWorkloadEntry::evaluation_grid_ms)
        .collect();
    let roots = compose_logical_candidate(per_computation, choice)
        .map(|roots| roots.into_iter().map(|(_, root)| root).collect::<Vec<_>>());
    let Ok(composed) = &roots else {
        return vec![roots];
    };
    let composed_viable = viable(composed);
    let pass2 = pass2_candidates(composed, &evaluation_grid_ms, composed_viable, viable);
    let mut candidates = Vec::new();
    if composed_viable {
        candidates.push(roots);
    }
    candidates.extend(pass2);
    candidates
}

/// The Pass 2 candidates of the composed candidate with roots `roots`, in
/// enumeration order, each distinct candidate once. `evaluation_grid_ms[q]`
/// is the grid query `q`'s evaluations fall on, if it repeats at a fixed
/// interval ([`QueryWorkloadEntry::evaluation_grid_ms`]).
fn pass2_candidates(
    roots: &[QueryRoot],
    evaluation_grid_ms: &[Option<u64>],
    composed_viable: bool,
    viable: &dyn Fn(&[QueryRoot]) -> bool,
) -> Vec<Result<Vec<QueryRoot>, PerComputationCandidateError>> {
    let mut found = Pass2List {
        seen: vec![flatten(roots).0],
        out: Vec::new(),
        widened: Vec::new(),
        viable,
    };
    if composed_viable {
        found.identical_expression(roots);
    }
    found.window_composition(roots, evaluation_grid_ms);
    found.reuse(roots, evaluation_grid_ms);
    let candidates = capability_sharing_candidates(roots);
    let pass2_candidates = match summary_capability_pass2_candidates(roots, &candidates) {
        Ok(pass2_candidates) => pass2_candidates,
        Err(error) => {
            found.out.push(Err(error));
            found.out.append(&mut found.widened);
            return found.out;
        }
    };
    for pass2_candidate in pass2_candidates {
        match pass2_candidate.roots {
            Ok(roots) => {
                if found.push(&roots) {
                    found.identical_expression(&roots);
                }
                found.window_composition(&roots, evaluation_grid_ms);
                found.reuse(&roots, evaluation_grid_ms);
            }
            // Listed as a candidate that cannot be composed; the other
            // Pass 2 candidates go on.
            Err(error) => found.out.push(Err(error)),
        }
    }
    found.out.append(&mut found.widened);
    found.out
}

/// Pass 2 candidates found so far, and the flattened DAG of every candidate
/// listed (the Pass 1 candidate's included), so each is listed once.
struct Pass2List<'v> {
    seen: Vec<asap_types::ir::flat::FlatDag>,
    out: Vec<Result<Vec<QueryRoot>, PerComputationCandidateError>>,
    /// The identical-expression candidates over widened scans
    /// ([`with_union_scan_labels`]), listed after `out`: a plan both lists
    /// reach the same way (e.g. one per-series build shared by either rule)
    /// ties, and plan selection keeps the earlier one.
    widened: Vec<Result<Vec<QueryRoot>, PerComputationCandidateError>>,
    /// Whether a candidate, or any of its identical-expression candidates,
    /// can have a valid implementation; always `true` for the exhaustive
    /// search.
    viable: &'v dyn Fn(&[QueryRoot]) -> bool,
}

impl Pass2List<'_> {
    /// List the candidate with roots `roots` unless an equal one is listed
    /// or it is not viable. `false` when it is not viable: neither it nor
    /// its identical-expression candidates can be valid, so the caller
    /// skips them. Sharing identical nodes never makes a build eligible
    /// for a materialization it was not eligible for, since the shared
    /// node is read by every reader of the nodes it replaces.
    fn push(&mut self, roots: &[QueryRoot]) -> bool {
        if !(self.viable)(roots) {
            return false;
        }
        let flat = flatten(roots).0;
        if self.seen.contains(&flat) {
            return true;
        }
        self.seen.push(flat);
        self.out.push(Ok(roots.to_vec()));
        true
    }

    /// The identical-expression Pass 2 candidates of `roots`.
    /// When a metric's scans project different labels, also those of
    /// `roots` with the scans widened to every label
    /// ([`with_union_scan_labels`]) that share a scan: the others only read
    /// wider scans than their counterparts over `roots`. They are listed
    /// last (`widened`).
    fn identical_expression(&mut self, roots: &[QueryRoot]) {
        let candidates = sharing_candidates(roots);
        for pass2_candidate in identical_expression_pass2_candidates(roots, &candidates) {
            self.push(&pass2_candidate.roots);
        }
        let widened = match with_union_scan_labels(roots) {
            Ok(Some(widened)) => widened,
            Ok(None) => return,
            Err(error) => {
                self.out.push(Err(error));
                return;
            }
        };
        let start = self.out.len();
        let candidates = sharing_candidates(&widened);
        for pass2_candidate in identical_expression_pass2_candidates(&widened, &candidates) {
            let shares_a_scan = pass2_candidate
                .sharing_candidates
                .iter()
                .any(|c| matches!(c.nodes[0].non_asap(), Some(NonASAPOp::Scan { .. })));
            if shares_a_scan {
                self.push(&pass2_candidate.roots);
            }
        }
        let widened = self.out.split_off(start);
        self.widened.extend(widened);
    }

    /// The window-composition Pass 2 candidates of `roots`, each followed
    /// by its identical-expression Pass 2 candidates.
    fn window_composition(&mut self, roots: &[QueryRoot], evaluation_grid_ms: &[Option<u64>]) {
        let candidates = pane_candidates(roots, evaluation_grid_ms);
        let pass2_candidates = match window_composition_pass2_candidates(roots, &candidates) {
            Ok(pass2_candidates) => pass2_candidates,
            Err(error) => {
                self.out.push(Err(error));
                return;
            }
        };
        for pass2_candidate in pass2_candidates {
            match pass2_candidate.roots {
                Ok(roots) => {
                    if self.push(&roots) {
                        self.identical_expression(&roots);
                    }
                }
                Err(error) => self.out.push(Err(error)),
            }
        }
    }

    /// The reuse Pass 2 candidates of `roots`: the exact top-k reuse
    /// rule's, then each rollup Pass 2 candidate followed by its own exact
    /// top-k reuse and window-composition Pass 2 candidates (the finer
    /// build as tumbling panes, so a deployment can store it and the coarse
    /// computation rolls up its merged state); each followed by its
    /// identical-expression Pass 2 candidates. They are listed eagerly,
    /// like every Pass 2 rule's: more than [`MAX_REUSE_CANDIDATES`] rollup
    /// choices ([`rollup_choices`]) and top-k reuse candidates together list
    /// one error.
    fn reuse(&mut self, roots: &[QueryRoot], evaluation_grid_ms: &[Option<u64>]) {
        let candidates = rollup_candidates(roots);
        // Every rollup subset is combined with every top-k subset: cap the
        // product, not each factor.
        let found = rollup_choices(&candidates).len() + topk_reuse_candidates(roots).len();
        if found > MAX_REUSE_CANDIDATES {
            self.out
                .push(Err(PerComputationCandidateError::TooManyCandidates {
                    counted: "rollup choices and top-k reuse candidates together",
                    found,
                    max: MAX_REUSE_CANDIDATES,
                }));
            return;
        }
        self.topk_reuse(roots);
        let pass2_candidates = match rollup_pass2_candidates(roots, &candidates) {
            Ok(pass2_candidates) => pass2_candidates,
            Err(error) => {
                self.out.push(Err(error));
                return;
            }
        };
        for pass2_candidate in pass2_candidates {
            match pass2_candidate.roots {
                Ok(roots) => {
                    if self.push(&roots) {
                        self.identical_expression(&roots);
                    }
                    self.topk_reuse(&roots);
                    self.window_composition(&roots, evaluation_grid_ms);
                }
                Err(error) => self.out.push(Err(error)),
            }
        }
    }

    /// The exact top-k reuse Pass 2 candidates of `roots`, each followed by
    /// its identical-expression Pass 2 candidates.
    fn topk_reuse(&mut self, roots: &[QueryRoot]) {
        let candidates = topk_reuse_candidates(roots);
        let pass2_candidates = match topk_reuse_pass2_candidates(roots, &candidates) {
            Ok(pass2_candidates) => pass2_candidates,
            Err(error) => {
                self.out.push(Err(error));
                return;
            }
        };
        for pass2_candidate in pass2_candidates {
            match pass2_candidate.roots {
                Ok(roots) => {
                    if self.push(&roots) {
                        self.identical_expression(&roots);
                    }
                }
                Err(error) => self.out.push(Err(error)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pass1::per_computation_candidates::generate_per_computation_candidates;
    use crate::test_support::lower_promql;
    use asap_types::ir::schema::schema_support::with_promql_series_identity;

    /// The composed candidate of `queries`, every target at its first
    /// (exact) candidate.
    fn exact(queries: &[String]) -> Vec<QueryRoot> {
        let roots = queries
            .iter()
            .enumerate()
            .map(|(i, q)| {
                let root = lower_promql(q, asap_types::ir::properties::AccuracyTarget::exact());
                (
                    i,
                    QueryRoot::Operator(with_promql_series_identity(&root).unwrap()),
                )
            })
            .collect();
        let inventory = generate_per_computation_candidates(roots).unwrap();
        compose_logical_candidate(&inventory, &vec![0; inventory.targets.len()])
            .unwrap()
            .into_iter()
            .map(|(_, root)| root)
            .collect()
    }

    /// A rollup Pass 2 candidate is also built over tumbling panes: for a
    /// fine and a coarse sum over one window, some Pass 2 candidate answers
    /// the coarse sum from the fine one and builds the fine one as panes, so
    /// a deployment that keeps no raw data samples can store it and roll the
    /// coarse sum up from its merged state.
    #[test]
    fn a_rollup_is_also_built_over_tumbling_panes() {
        let roots = [
            "sum by (region, service, job) (sum_over_time(data[100s]))",
            "sum by (region) (sum_over_time(data[100s]))",
        ]
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let root = lower_promql(q, asap_types::ir::properties::AccuracyTarget::exact());
            (
                i,
                QueryRoot::Operator(with_promql_series_identity(&root).unwrap()),
            )
        })
        .collect();
        let inventory = generate_per_computation_candidates(roots).unwrap();
        let entries: Vec<asap_types::workload::QueryWorkloadEntry> = (0..2)
            .map(|_| asap_types::workload::QueryWorkloadEntry {
                query: Default::default(),
                requirements: Default::default(),
                predictability: asap_types::workload::Predictability::Predictable {
                    known_at: None,
                },
                recurrence: asap_types::workload::QueryRecurrence::Repeated(
                    asap_types::workload::RepeatedDemand::FixedInterval(
                        asap_types::workload::RepetitionInterval(10_000),
                    ),
                ),
                range_evaluation: None,
            })
            .collect();
        let mut choice: Vec<_> = inventory
            .targets
            .iter()
            .map(|target| {
                if target.fused_computation.is_some() {
                    target.choices.candidates.len()
                } else {
                    0
                }
            })
            .collect();
        for target in &inventory.targets {
            if let Some(fused) = &target.fused_computation {
                for &absorbed in &fused.absorbed_targets {
                    choice[absorbed] = 0;
                }
            }
        }
        // Every rollup candidate's roots with the finer build in panes.
        let grid: Vec<_> = entries
            .iter()
            .map(QueryWorkloadEntry::evaluation_grid_ms)
            .collect();
        let composed: Vec<_> = compose_logical_candidate(&inventory, &choice)
            .unwrap()
            .into_iter()
            .map(|(_, root)| root)
            .collect();
        let rolled_in_panes: Vec<_> =
            rollup_pass2_candidates(&composed, &rollup_candidates(&composed))
                .unwrap()
                .into_iter()
                .filter_map(|rolled| rolled.roots.ok())
                .flat_map(|rolled| {
                    let panes = pane_candidates(&rolled, &grid);
                    window_composition_pass2_candidates(&rolled, &panes).unwrap()
                })
                .filter_map(|paned| paned.roots.ok())
                .map(|roots| flatten(&roots).0)
                .collect();
        assert!(!rolled_in_panes.is_empty());
        assert!(
            stage1_candidates_of_choice(&inventory, &entries, &choice, &|_| true)
                .into_iter()
                .filter_map(Result::ok)
                .any(|roots| rolled_in_panes.contains(&flatten(&roots).0))
        );
    }

    /// Rollup and exact top-k reuse candidates multiply (every subset of
    /// one with every subset of the other): past 20 together the candidate
    /// lists one error instead of enumerating 2^21 Pass 2 candidates.
    #[test]
    fn reuse_candidates_past_20_together_are_an_error() {
        let mut queries = Vec::new();
        for i in 0..11 {
            queries.push(format!("sum by (job) (m{i})"));
            queries.push(format!("sum by (job, instance) (m{i})"));
        }
        for i in 0..10 {
            queries.push(format!("topk by (job) (5, sum by (job, instance) (t{i}))"));
            queries.push(format!("topk by (job) (10, sum by (job, instance) (t{i}))"));
        }
        let roots = exact(&queries);
        assert_eq!(rollup_candidates(&roots).len(), 11);
        assert_eq!(topk_reuse_candidates(&roots).len(), 10);
        let mut found = Pass2List {
            seen: vec![flatten(&roots).0],
            out: Vec::new(),
            widened: Vec::new(),
            viable: &|_| true,
        };
        found.reuse(&roots, &[]);
        let [failed] = found.out.as_slice() else {
            panic!("one error, {} candidates", found.out.len())
        };
        assert!(matches!(
            failed,
            Err(PerComputationCandidateError::TooManyCandidates {
                max: MAX_REUSE_CANDIDATES,
                ..
            })
        ));
        let Err(error) = failed else { unreachable!() };
        assert_eq!(
            error.to_string(),
            format!("found 21 rollup choices and top-k reuse candidates together, more than {MAX_REUSE_CANDIDATES}")
        );
    }
}
