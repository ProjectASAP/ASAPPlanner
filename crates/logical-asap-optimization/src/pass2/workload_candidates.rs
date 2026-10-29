//! Logical sharing and window rewrites for an explicitly chosen computation assignment.
//! Production search organizes local candidates in the memo rather than enumerating
//! Cartesian products of workload choices.

use std::rc::Rc;

use asap_types::ir::flat::flatten;
use asap_types::ir::{NonASAPOp, OperatorNode, QueryRoot};
use asap_types::workload::QueryWorkloadEntry;

use crate::pass1::per_computation_candidates::{
    compose_logical_candidate, PerComputationCandidateError, PerComputationCandidates,
};
use crate::pass2::asap_aware_cse::{
    capability_sharing_candidates, identical_expression_pass2_candidates, pane_candidates,
    rollup_candidates, rollup_choices, rollup_pass2_candidates, sharing_candidates,
    summary_capability_pass2_candidates, topk_reuse_candidates, topk_reuse_pass2_candidates,
    window_composition_pass2_candidates, with_union_scan_labels, PaneCandidate, RollupCandidate,
    SharedPaneChain, SharingCandidate, TopKReuseCandidate, MAX_REUSE_CANDIDATES,
};

/// What a Pass 2 candidate changed of the Pass 1 candidate it derives
/// from: each set of summary builds in `shared_builds` made one build sized
/// for all its readers (the summary-capability rule), then the build of
/// each of `pane_candidates` built as tumbling panes (the
/// window-composition rule), or each of `rollup_candidates` and
/// `topk_reuse_candidates` answered from another computation (the rollup
/// and exact top-k reuse rules), then the nodes of each of
/// `sharing_candidates` made one node (the identical-expression rule). It
/// has its Pass 1 candidate's `choice`; display labels are the devtools'.
#[derive(Debug, Clone)]
pub struct Pass2Candidate {
    /// The position, in Stage 1 enumeration order from 0, of the Pass 1
    /// candidate it derives from: `L<from_pass1_position + 1>`.
    pub from_pass1_position: usize,
    /// Summary-capability rule: each set of the Pass 1 candidate's summary
    /// builds made one build.
    pub shared_builds: Vec<Vec<Rc<OperatorNode>>>,
    /// Window-composition rule: the builds built as tumbling panes.
    pub pane_candidates: Vec<PaneCandidate>,
    /// Window-composition rule: the chains each shared by several of
    /// `pane_candidates`, each member merging its newest panes.
    pub shared_pane_chains: Vec<SharedPaneChain>,
    /// Rollup rule: the Pass 1 candidate's coarse builds answered from
    /// finer ones.
    pub rollup_candidates: Vec<RollupCandidate>,
    /// Exact top-k reuse rule: the top-k computations that read one ranking.
    pub topk_reuse_candidates: Vec<TopKReuseCandidate>,
    /// Identical-expression rule: the identical nodes made one node.
    pub sharing_candidates: Vec<SharingCandidate>,
}

/// One Stage 1 candidate in enumeration order, before Stage 2 numbers it.
#[derive(Debug)]
pub struct Stage1Candidate {
    /// The Pass 1 choice (`targets[t].computation_for(choice[t])`: target
    /// `t`'s candidate, or its fused computation's); a Pass 2 candidate has
    /// its Pass 1 candidate's.
    pub choice: Vec<usize>,
    pub roots: Result<Vec<QueryRoot>, PerComputationCandidateError>,
    /// For a Pass 2 candidate: what it changed of its Pass 1 candidate.
    pub pass2: Option<Pass2Candidate>,
}

/// Apply sharing and window rules to one explicit Pass 1 assignment.
/// `entries[q]` supplies query `q`'s evaluation grid. Returned Pass 2
/// candidates record position zero as their source assignment.
pub fn stage1_candidates_of_choice<Id: Clone>(
    per_computation: &PerComputationCandidates<Id>,
    entries: &[QueryWorkloadEntry],
    choice: &[usize],
    viable: &dyn Fn(&[QueryRoot]) -> bool,
) -> Vec<Stage1Candidate> {
    let evaluation_grid_ms: Vec<Option<u64>> = entries
        .iter()
        .map(QueryWorkloadEntry::evaluation_grid_ms)
        .collect();
    choice_candidates(per_computation, choice, &evaluation_grid_ms, 0, viable)
}

/// [`stage1_candidates_of_choice`] with each query's evaluation grid, its
/// Pass 2 candidates recording `from` as their Pass 1 candidate's position.
fn choice_candidates<Id: Clone>(
    per_computation: &PerComputationCandidates<Id>,
    choice: &[usize],
    evaluation_grid_ms: &[Option<u64>],
    from: usize,
    viable: &dyn Fn(&[QueryRoot]) -> bool,
) -> Vec<Stage1Candidate> {
    let roots = compose_logical_candidate(per_computation, choice)
        .map(|roots| roots.into_iter().map(|(_, root)| root).collect::<Vec<_>>());
    let (pass2, composed_viable) = match &roots {
        Ok(roots) => {
            let composed_viable = viable(roots);
            (
                pass2_candidates(roots, evaluation_grid_ms, composed_viable, viable),
                composed_viable,
            )
        }
        Err(_) => (Vec::new(), true),
    };
    let mut candidates = Vec::new();
    if composed_viable {
        candidates.push(Stage1Candidate {
            choice: choice.to_vec(),
            roots,
            pass2: None,
        });
    }
    for Pass2 {
        rules,
        sharing_candidates,
        roots,
    } in pass2
    {
        candidates.push(Stage1Candidate {
            choice: choice.to_vec(),
            roots,
            pass2: Some(Pass2Candidate {
                from_pass1_position: from,
                shared_builds: rules.shared_builds,
                pane_candidates: rules.pane_candidates,
                shared_pane_chains: rules.shared_pane_chains,
                rollup_candidates: rules.rollup_candidates,
                topk_reuse_candidates: rules.topk_reuse_candidates,
                sharing_candidates,
            }),
        });
    }
    candidates
}

/// A Pass 2 candidate: what the rules other than the identical-expression
/// rule changed, the identical nodes it shares, and its roots.
struct Pass2 {
    rules: Rules,
    sharing_candidates: Vec<SharingCandidate>,
    roots: Result<Vec<QueryRoot>, PerComputationCandidateError>,
}

/// What the rules other than the identical-expression rule changed: the
/// summary builds shared (summary-capability rule), the builds built as
/// tumbling panes (window-composition rule) and the computations answered
/// from another (rollup and exact top-k reuse rules).
#[derive(Debug, Clone, Default)]
struct Rules {
    shared_builds: Vec<Vec<Rc<OperatorNode>>>,
    pane_candidates: Vec<PaneCandidate>,
    shared_pane_chains: Vec<SharedPaneChain>,
    rollup_candidates: Vec<RollupCandidate>,
    topk_reuse_candidates: Vec<TopKReuseCandidate>,
}

impl Pass2 {
    fn failed(rules: &Rules, error: PerComputationCandidateError) -> Self {
        Self {
            rules: rules.clone(),
            sharing_candidates: Vec::new(),
            roots: Err(error),
        }
    }
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
) -> Vec<Pass2> {
    let mut found = Pass2List {
        seen: vec![flatten(roots).0],
        out: Vec::new(),
        widened: Vec::new(),
        viable,
    };
    let none = Rules::default();
    if composed_viable {
        found.identical_expression(&none, roots);
    }
    found.window_composition(&none, roots, evaluation_grid_ms);
    found.reuse(&none, roots, evaluation_grid_ms);
    let candidates = capability_sharing_candidates(roots);
    let pass2_candidates = match summary_capability_pass2_candidates(roots, &candidates) {
        Ok(pass2_candidates) => pass2_candidates,
        Err(error) => {
            found.out.push(Pass2::failed(&none, error));
            found.out.append(&mut found.widened);
            return found.out;
        }
    };
    for pass2_candidate in pass2_candidates {
        let rules = Rules {
            shared_builds: pass2_candidate.shared_builds,
            ..Rules::default()
        };
        match pass2_candidate.roots {
            Ok(roots) => {
                if found.push(&rules, Vec::new(), &roots) {
                    found.identical_expression(&rules, &roots);
                }
                found.window_composition(&rules, &roots, evaluation_grid_ms);
                found.reuse(&rules, &roots, evaluation_grid_ms);
            }
            // Listed as a candidate that cannot be composed; the other
            // Pass 2 candidates go on.
            Err(error) => found.out.push(Pass2::failed(&rules, error)),
        }
    }
    found.out.append(&mut found.widened);
    found.out
}

/// Pass 2 candidates found so far, and the flattened DAG of every candidate
/// listed (the Pass 1 candidate's included), so each is listed once.
struct Pass2List<'v> {
    seen: Vec<asap_types::ir::flat::FlatDag>,
    out: Vec<Pass2>,
    /// The identical-expression candidates over widened scans
    /// ([`with_union_scan_labels`]), listed after `out`: a plan both lists
    /// reach the same way (e.g. one per-series build shared by either rule)
    /// ties, and plan selection keeps the earlier one.
    widened: Vec<Pass2>,
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
    fn push(
        &mut self,
        rules: &Rules,
        sharing_candidates: Vec<SharingCandidate>,
        roots: &[QueryRoot],
    ) -> bool {
        if !(self.viable)(roots) {
            return false;
        }
        let flat = flatten(roots).0;
        if self.seen.contains(&flat) {
            return true;
        }
        self.seen.push(flat);
        self.out.push(Pass2 {
            rules: rules.clone(),
            sharing_candidates,
            roots: Ok(roots.to_vec()),
        });
        true
    }

    /// The identical-expression Pass 2 candidates of `roots`, which the
    /// other rules changed as `rules` says.
    /// When a metric's scans project different labels, also those of
    /// `roots` with the scans widened to every label
    /// ([`with_union_scan_labels`]) that share a scan: the others only read
    /// wider scans than their counterparts over `roots`. They are listed
    /// last (`widened`).
    fn identical_expression(&mut self, rules: &Rules, roots: &[QueryRoot]) {
        let candidates = sharing_candidates(roots);
        for pass2_candidate in identical_expression_pass2_candidates(roots, &candidates) {
            self.push(
                rules,
                pass2_candidate.sharing_candidates,
                &pass2_candidate.roots,
            );
        }
        let widened = match with_union_scan_labels(roots) {
            Ok(Some(widened)) => widened,
            Ok(None) => return,
            Err(error) => {
                self.out.push(Pass2::failed(rules, error));
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
                self.push(
                    rules,
                    pass2_candidate.sharing_candidates,
                    &pass2_candidate.roots,
                );
            }
        }
        let widened = self.out.split_off(start);
        self.widened.extend(widened);
    }

    /// The window-composition Pass 2 candidates of `roots`, which the other
    /// rules changed as `rules` says, each followed by its
    /// identical-expression Pass 2 candidates.
    fn window_composition(
        &mut self,
        rules: &Rules,
        roots: &[QueryRoot],
        evaluation_grid_ms: &[Option<u64>],
    ) {
        let candidates = pane_candidates(roots, evaluation_grid_ms);
        let pass2_candidates = match window_composition_pass2_candidates(roots, &candidates) {
            Ok(pass2_candidates) => pass2_candidates,
            Err(error) => {
                self.out.push(Pass2::failed(rules, error));
                return;
            }
        };
        for pass2_candidate in pass2_candidates {
            let rules = Rules {
                pane_candidates: pass2_candidate.pane_candidates,
                shared_pane_chains: pass2_candidate.shared_chains,
                ..rules.clone()
            };
            match pass2_candidate.roots {
                Ok(roots) => {
                    if self.push(&rules, Vec::new(), &roots) {
                        self.identical_expression(&rules, &roots);
                    }
                }
                Err(error) => self.out.push(Pass2::failed(&rules, error)),
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
    fn reuse(&mut self, rules: &Rules, roots: &[QueryRoot], evaluation_grid_ms: &[Option<u64>]) {
        let candidates = rollup_candidates(roots);
        // Every rollup subset is combined with every top-k subset: cap the
        // product, not each factor.
        let found = rollup_choices(&candidates).len() + topk_reuse_candidates(roots).len();
        if found > MAX_REUSE_CANDIDATES {
            self.out.push(Pass2::failed(
                rules,
                PerComputationCandidateError::TooManyCandidates {
                    counted: "rollup choices and top-k reuse candidates together",
                    found,
                    max: MAX_REUSE_CANDIDATES,
                },
            ));
            return;
        }
        self.topk_reuse(rules, roots);
        let pass2_candidates = match rollup_pass2_candidates(roots, &candidates) {
            Ok(pass2_candidates) => pass2_candidates,
            Err(error) => {
                self.out.push(Pass2::failed(rules, error));
                return;
            }
        };
        for pass2_candidate in pass2_candidates {
            let mut rules = rules.clone();
            rules
                .rollup_candidates
                .extend(pass2_candidate.rollup_candidates);
            match pass2_candidate.roots {
                Ok(roots) => {
                    if self.push(&rules, Vec::new(), &roots) {
                        self.identical_expression(&rules, &roots);
                    }
                    self.topk_reuse(&rules, &roots);
                    self.window_composition(&rules, &roots, evaluation_grid_ms);
                }
                Err(error) => self.out.push(Pass2::failed(&rules, error)),
            }
        }
    }

    /// The exact top-k reuse Pass 2 candidates of `roots`, each followed by
    /// its identical-expression Pass 2 candidates.
    fn topk_reuse(&mut self, rules: &Rules, roots: &[QueryRoot]) {
        let candidates = topk_reuse_candidates(roots);
        let pass2_candidates = match topk_reuse_pass2_candidates(roots, &candidates) {
            Ok(pass2_candidates) => pass2_candidates,
            Err(error) => {
                self.out.push(Pass2::failed(rules, error));
                return;
            }
        };
        for pass2_candidate in pass2_candidates {
            let mut rules = rules.clone();
            rules
                .topk_reuse_candidates
                .extend(pass2_candidate.topk_reuse_candidates);
            match pass2_candidate.roots {
                Ok(roots) => {
                    if self.push(&rules, Vec::new(), &roots) {
                        self.identical_expression(&rules, &roots);
                    }
                }
                Err(error) => self.out.push(Pass2::failed(&rules, error)),
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
                    target.ways_to_compute.len()
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
        assert!(
            stage1_candidates_of_choice(&inventory, &entries, &choice, &|_| true)
                .into_iter()
                .any(|candidate| {
                    candidate.roots.is_ok()
                        && candidate.pass2.as_ref().is_some_and(|pass2| {
                            !pass2.rollup_candidates.is_empty() && !pass2.pane_candidates.is_empty()
                        })
                })
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
        found.reuse(&Rules::default(), &roots, &[]);
        let [failed] = found.out.as_slice() else {
            panic!("one error, {} candidates", found.out.len())
        };
        assert!(matches!(
            failed.roots,
            Err(PerComputationCandidateError::TooManyCandidates {
                max: MAX_REUSE_CANDIDATES,
                ..
            })
        ));
        let Err(error) = &failed.roots else {
            unreachable!()
        };
        assert_eq!(
            error.to_string(),
            format!("found 21 rollup choices and top-k reuse candidates together, more than {MAX_REUSE_CANDIDATES}")
        );
    }
}
