//! Stage 1 (logical ASAP-aware optimization) over the Pass 1 inventory:
//! the whole-workload CandidateLogicalASAPDAGs, one at a time, each
//! composed Pass 1 candidate followed by its Pass 2 candidates
//! ([`stage1_logical_asap_optimization`]). Stage 2 and Stage 3 run over
//! them in `asap-plan-selection`'s Stage 1 to Stage 3 driver.

use std::rc::Rc;

use asap_types::ir::flat::flatten;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::workload::QueryRecurrence;

use crate::asap_aware_cse::{
    capability_sharing_candidates, identical_expression_pass2_candidates, pane_candidates,
    rollup_candidates, rollup_pass2_candidates, sharing_candidates,
    summary_capability_pass2_candidates, topk_reuse_candidates, topk_reuse_pass2_candidates,
    window_composition_pass2_candidates, PaneCandidate, RollupCandidate, SharingCandidate,
    TopKReuseCandidate, MAX_REUSE_CANDIDATES,
};
use crate::per_computation_candidates::{
    compose_logical_candidate, PerComputationCandidateError, PerComputationCandidates,
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

/// Stage 1 (logical ASAP-aware optimization) over the Pass 1 inventory, one
/// candidate at a time: each composed Pass 1 candidate (every choice of a
/// per-computation candidate for every target, the last target varying
/// fastest), followed by its Pass 2 candidates: the identical-expression
/// rule's, then the window-composition rule's, then the reuse rules'
/// (exact top-k reuse and rollup), then each summary-capability
/// candidate followed by its own identical-expression and
/// window-composition candidates; every window-composition candidate is
/// followed by its own identical-expression candidates. `recurrence[q]` is
/// how often query `q` is evaluated: the window-composition rule splits
/// only builds of queries that repeat at a fixed interval. A Pass 2
/// candidate equal to an earlier one of the same Pass 1 candidate (same
/// nodes, same sharing) is listed once. Lazy and uncapped: a caller that
/// limits the candidates stops consuming it.
pub fn stage1_logical_asap_optimization<'a, Id: Clone>(
    per_computation: &'a PerComputationCandidates<Id>,
    recurrence: &[QueryRecurrence],
) -> impl Iterator<Item = Stage1Candidate> + 'a {
    let interval_ms: Vec<Option<u64>> = recurrence
        .iter()
        .map(QueryRecurrence::fixed_interval_ms)
        .collect();
    let mut choice = Some(vec![0; per_computation.targets.len()]);
    let mut queue = std::collections::VecDeque::new();
    let mut position = 0usize;
    std::iter::from_fn(move || {
        while queue.is_empty() {
            let current = choice.as_mut()?;
            let skip = per_computation.absorbed_choice_varies(current);
            let this = current.clone();
            if !next_choice(per_computation, current) {
                choice = None;
            }
            if skip {
                continue;
            }
            let roots = compose_logical_candidate(per_computation, &this)
                .map(|roots| roots.into_iter().map(|(_, root)| root).collect::<Vec<_>>());
            let from = position + queue.len();
            let pass2 = match &roots {
                Ok(roots) => pass2_candidates(roots, &interval_ms),
                Err(_) => Vec::new(),
            };
            queue.push_back(Stage1Candidate {
                choice: this.clone(),
                roots,
                pass2: None,
            });
            for Pass2 {
                rules,
                sharing_candidates,
                roots,
            } in pass2
            {
                queue.push_back(Stage1Candidate {
                    choice: this.clone(),
                    roots,
                    pass2: Some(Pass2Candidate {
                        from_pass1_position: from,
                        shared_builds: rules.shared_builds,
                        pane_candidates: rules.pane_candidates,
                        rollup_candidates: rules.rollup_candidates,
                        topk_reuse_candidates: rules.topk_reuse_candidates,
                        sharing_candidates,
                    }),
                });
            }
        }
        position += 1;
        queue.pop_front()
    })
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
/// enumeration order, each distinct candidate once. `interval_ms[q]` is the
/// fixed interval of query `q`, if it repeats at one.
fn pass2_candidates(roots: &[QueryRoot], interval_ms: &[Option<u64>]) -> Vec<Pass2> {
    let mut found = Pass2List {
        seen: vec![flatten(roots).0],
        out: Vec::new(),
    };
    let none = Rules::default();
    found.identical_expression(&none, roots);
    found.window_composition(&none, roots, interval_ms);
    found.reuse(&none, roots);
    let candidates = capability_sharing_candidates(roots);
    let pass2_candidates = match summary_capability_pass2_candidates(roots, &candidates) {
        Ok(pass2_candidates) => pass2_candidates,
        Err(error) => {
            found.out.push(Pass2::failed(&none, error));
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
                found.push(&rules, Vec::new(), &roots);
                found.identical_expression(&rules, &roots);
                found.window_composition(&rules, &roots, interval_ms);
            }
            // Listed as a candidate that cannot be composed; the other
            // Pass 2 candidates go on.
            Err(error) => found.out.push(Pass2::failed(&rules, error)),
        }
    }
    found.out
}

/// Pass 2 candidates found so far, and the flattened DAG of every candidate
/// listed (the Pass 1 candidate's included), so each is listed once.
struct Pass2List {
    seen: Vec<asap_types::ir::flat::FlatDag>,
    out: Vec<Pass2>,
}

impl Pass2List {
    fn push(
        &mut self,
        rules: &Rules,
        sharing_candidates: Vec<SharingCandidate>,
        roots: &[QueryRoot],
    ) {
        let flat = flatten(roots).0;
        if self.seen.contains(&flat) {
            return;
        }
        self.seen.push(flat);
        self.out.push(Pass2 {
            rules: rules.clone(),
            sharing_candidates,
            roots: Ok(roots.to_vec()),
        });
    }

    /// The identical-expression Pass 2 candidates of `roots`, which the
    /// other rules changed as `rules` says.
    fn identical_expression(&mut self, rules: &Rules, roots: &[QueryRoot]) {
        let candidates = sharing_candidates(roots);
        for pass2_candidate in identical_expression_pass2_candidates(roots, &candidates) {
            self.push(
                rules,
                pass2_candidate.sharing_candidates,
                &pass2_candidate.roots,
            );
        }
    }

    /// The window-composition Pass 2 candidates of `roots`, which the other
    /// rules changed as `rules` says, each followed by its
    /// identical-expression Pass 2 candidates.
    fn window_composition(
        &mut self,
        rules: &Rules,
        roots: &[QueryRoot],
        interval_ms: &[Option<u64>],
    ) {
        let candidates = pane_candidates(roots, interval_ms);
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
                ..rules.clone()
            };
            match pass2_candidate.roots {
                Ok(roots) => {
                    self.push(&rules, Vec::new(), &roots);
                    self.identical_expression(&rules, &roots);
                }
                Err(error) => self.out.push(Pass2::failed(&rules, error)),
            }
        }
    }

    /// The reuse Pass 2 candidates of `roots`: the exact top-k reuse
    /// rule's, then each rollup Pass 2 candidate followed by its own exact
    /// top-k reuse Pass 2 candidates; each followed by its
    /// identical-expression Pass 2 candidates. They are listed eagerly,
    /// like every Pass 2 rule's: more than [`MAX_REUSE_CANDIDATES`] rollup
    /// and top-k reuse candidates together list one error.
    fn reuse(&mut self, rules: &Rules, roots: &[QueryRoot]) {
        let candidates = rollup_candidates(roots);
        // Every rollup subset is combined with every top-k subset: cap the
        // product, not each factor.
        if candidates.len() + topk_reuse_candidates(roots).len() > MAX_REUSE_CANDIDATES {
            self.out.push(Pass2::failed(
                rules,
                PerComputationCandidateError::Unsupported(
                    "more than 20 rollup and top-k reuse candidates together (2^n Pass 2 candidates)",
                ),
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
                    self.push(&rules, Vec::new(), &roots);
                    self.identical_expression(&rules, &roots);
                    self.topk_reuse(&rules, &roots);
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
                    self.push(&rules, Vec::new(), &roots);
                    self.identical_expression(&rules, &roots);
                }
                Err(error) => self.out.push(Pass2::failed(&rules, error)),
            }
        }
    }
}

/// Mixed-radix increment of `choice`, each digit over its target's
/// candidates and then its fused computation's: the last target varies fastest.
/// `false` when a carry out of the first target ends the enumeration.
fn next_choice<Id>(inventory: &PerComputationCandidates<Id>, choice: &mut [usize]) -> bool {
    for (digit, target) in choice.iter_mut().zip(&inventory.targets).rev() {
        *digit += 1;
        if *digit < target.choice_count() {
            return true;
        }
        *digit = 0;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::per_computation_candidates::generate_per_computation_candidates;
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
        };
        found.reuse(&Rules::default(), &roots);
        let [failed] = found.out.as_slice() else {
            panic!("one error, {} candidates", found.out.len())
        };
        assert!(matches!(
            failed.roots,
            Err(PerComputationCandidateError::Unsupported(_))
        ));
    }
}
