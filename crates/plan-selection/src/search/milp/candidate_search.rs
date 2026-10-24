//! Bounded evaluator refinement over candidate connections, without enumerating
//! complete plans up front. Certified query cuts reuse size-independent accuracy
//! failures; full-assignment cuts remain a correctness fallback;
//! they make no claim of scalable demand modeling or support for every cost model.
use std::time::{Duration, Instant};

use super::candidate_connections::{
    solve_candidate_proposal_with_metrics, CandidateDependencyGraph, CandidateProposal,
    CandidateSelectionError, CandidateSolveMetrics, ObservedAssignment, RejectedQuerySubgraph,
    SelectedCandidateGraph,
};

#[derive(Debug, Default)]
pub(super) struct CandidateEvaluation {
    pub actual_cost: Option<f64>,
    pub rejected_queries: Vec<RejectedQuerySubgraph>,
}

impl From<Option<f64>> for CandidateEvaluation {
    fn from(actual_cost: Option<f64>) -> Self {
        Self {
            actual_cost,
            rejected_queries: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct CandidateSearchLimits {
    /// Maximum evaluator calls (feasible and infeasible proposals both count).
    pub max_refinements: usize,
    pub wall_time: Duration,
}

#[derive(Debug, Clone)]
pub(super) struct EvaluatedCandidate {
    pub selected: SelectedCandidateGraph,
    pub actual_cost: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CandidateSearchStatus {
    /// Proven against the refined finite model, subject to solver tolerances.
    Optimal,
    Infeasible,
    RefinementLimit,
    TimeLimit,
    SolverStopped(String),
    EvaluatorStopped(String),
    NumericalStall,
}

#[derive(Debug)]
pub(super) struct CandidateSearchResult {
    pub incumbent: Option<EvaluatedCandidate>,
    pub status: CandidateSearchStatus,
    /// Best certified model lower bound observed; zero is the initial bound.
    pub lower_bound: f64,
    /// None without an incumbent. Zero-cost incumbent has zero gap only at bound zero.
    pub relative_gap: Option<f64>,
    pub refinements: usize,
    pub solve_metrics: CandidateSolveMetrics,
    /// Includes physical reconstruction, accuracy/latency checks and pricing.
    pub validation_seconds: f64,
    /// Complete query closures rejected by the original size-independent proof.
    pub certified_query_cuts: usize,
}

/// The callback returns an actual total cost for a validated feasible plan, or
/// `None` for a proven infeasible assignment. Errors stop search, preserving the
/// incumbent. Its cost must include every configured cost component and must be
/// deterministic for an assignment. Local graph costs are certified lower bounds,
/// not estimates. A violated certification is an error, never silently repaired.
/// Time bounds are cooperative: a blocking evaluator cannot be preempted here.
#[cfg(test)]
pub(super) fn search_candidates(
    graph: &CandidateDependencyGraph,
    limits: CandidateSearchLimits,
    mut evaluate: impl FnMut(&SelectedCandidateGraph) -> Result<Option<f64>, String>,
) -> Result<CandidateSearchResult, String> {
    search_with_structural_baseline(graph, limits, false, None, |selected| {
        evaluate(selected).map(Into::into)
    })
}

/// Seed the same certified-cost search with a bounded minimum-active-node plan.
/// This structural objective favors shared producers, but is not a cost estimate
/// or lower bound. Its proposals undergo the same evaluator and exact cuts.
#[cfg(test)]
pub(super) fn search_candidates_with_baseline(
    graph: &CandidateDependencyGraph,
    limits: CandidateSearchLimits,
    evaluate: impl FnMut(&SelectedCandidateGraph) -> Result<Option<f64>, String>,
) -> Result<CandidateSearchResult, String> {
    search_candidates_with_weighted_baseline(graph, limits, None, evaluate)
}

/// Optional weights order baseline proposals only; they never modify cost bounds.
#[cfg(test)]
pub(super) fn search_candidates_with_weighted_baseline(
    graph: &CandidateDependencyGraph,
    limits: CandidateSearchLimits,
    weights: Option<&[f64]>,
    evaluate: impl FnMut(&SelectedCandidateGraph) -> Result<Option<f64>, String>,
) -> Result<CandidateSearchResult, String> {
    let mut evaluate = evaluate;
    search_candidates_with_feedback(graph, limits, weights, |selected| {
        evaluate(selected).map(Into::into)
    })
}

pub(super) fn search_candidates_with_feedback(
    graph: &CandidateDependencyGraph,
    limits: CandidateSearchLimits,
    weights: Option<&[f64]>,
    evaluate: impl FnMut(&SelectedCandidateGraph) -> Result<CandidateEvaluation, String>,
) -> Result<CandidateSearchResult, String> {
    search_with_structural_baseline(graph, limits, true, weights, evaluate)
}

fn search_with_structural_baseline(
    graph: &CandidateDependencyGraph,
    limits: CandidateSearchLimits,
    structural_baseline: bool,
    baseline_weights: Option<&[f64]>,
    evaluate: impl FnMut(&SelectedCandidateGraph) -> Result<CandidateEvaluation, String>,
) -> Result<CandidateSearchResult, String> {
    search_with_solver(
        graph,
        limits,
        structural_baseline,
        baseline_weights,
        evaluate,
        solve_candidate_proposal_with_metrics,
    )
}

fn search_with_solver(
    graph: &CandidateDependencyGraph,
    limits: CandidateSearchLimits,
    structural_baseline: bool,
    baseline_weights: Option<&[f64]>,
    mut evaluate: impl FnMut(&SelectedCandidateGraph) -> Result<CandidateEvaluation, String>,
    mut propose: impl FnMut(
        &CandidateDependencyGraph,
        &[ObservedAssignment],
        Option<f64>,
        &mut CandidateSolveMetrics,
    ) -> Result<Option<CandidateProposal>, CandidateSelectionError>,
) -> Result<CandidateSearchResult, String> {
    graph.validate().map_err(|error| error.to_string())?;
    let started = Instant::now();
    let mut observations = Vec::new();
    let mut incumbent: Option<EvaluatedCandidate> = None;
    let mut lower_bound: f64 = 0.0;
    let mut refinements = 0;
    let mut solve_metrics = CandidateSolveMetrics::default();
    let mut validation_seconds = 0.0;
    if baseline_weights.is_some_and(|weights| weights.len() != graph.candidates.len()) {
        return Err("baseline weights must be indexed by candidate ID".into());
    }
    let mut heuristic_graph = structural_baseline.then(|| {
        let mut heuristic = graph.clone();
        for (id, candidate) in heuristic.candidates.iter_mut().enumerate() {
            candidate.local_cost = baseline_weights.map_or(1.0, |weights| weights[id]);
        }
        heuristic
    });
    if let Some(heuristic) = &heuristic_graph {
        heuristic.validate().map_err(|error| error.to_string())?;
    }
    // At most three validations and one fifth of the shared wall budget,
    // capped at two seconds; stop the baseline once it supplies an incumbent.
    let baseline_budget = (limits.wall_time / 5).min(Duration::from_secs(2));
    let mut baseline_attempts = 0;
    let status = loop {
        let remaining = limits.wall_time.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break CandidateSearchStatus::TimeLimit;
        }
        if refinements >= limits.max_refinements {
            break CandidateSearchStatus::RefinementLimit;
        }
        let use_baseline = heuristic_graph.is_some()
            && incumbent.is_none()
            && baseline_attempts < 3
            && started.elapsed() < baseline_budget;
        let (proposal_graph, proposal_observations, solve_budget) = if use_baseline {
            baseline_attempts += 1;
            // A structural proposal is never repriced by monetary observations.
            // Tried assignments are skipped here only; their validated costs
            // remain in the certified model used after this heuristic phase.
            let skipped = observations
                .iter()
                .map(|observation: &ObservedAssignment| {
                    let mut selected = observation.selected.clone();
                    selected.modeled_cost = selected
                        .selected_candidates
                        .iter()
                        .map(|id| {
                            heuristic_graph
                                .as_ref()
                                .expect("baseline graph exists")
                                .candidates[id.0]
                                .local_cost
                        })
                        .sum();
                    ObservedAssignment {
                        selected,
                        exact_cost: None,
                        rejected_queries: observation.rejected_queries.clone(),
                    }
                })
                .collect::<Vec<_>>();
            (
                heuristic_graph.as_ref().expect("baseline graph exists"),
                skipped,
                remaining.min(baseline_budget.saturating_sub(started.elapsed())),
            )
        } else {
            (graph, observations.clone(), remaining)
        };
        let mut proposal = match propose(
            proposal_graph,
            &proposal_observations,
            Some(solve_budget.as_secs_f64()),
            &mut solve_metrics,
        ) {
            Ok(Some(proposal)) => proposal,
            Ok(None) if use_baseline => {
                heuristic_graph = None;
                continue;
            }
            Ok(None) => {
                if incumbent.is_some() {
                    return Err("refined model excluded a validated feasible assignment".into());
                }
                break CandidateSearchStatus::Infeasible;
            }
            Err(CandidateSelectionError::Solver(_))
                if use_baseline && started.elapsed() >= baseline_budget =>
            {
                heuristic_graph = None;
                continue;
            }
            Err(error) => {
                break if started.elapsed() >= limits.wall_time {
                    CandidateSearchStatus::TimeLimit
                } else {
                    CandidateSearchStatus::SolverStopped(error.to_string())
                };
            }
        };
        if use_baseline {
            proposal.selected.modeled_cost = proposal
                .selected
                .selected_candidates
                .iter()
                .map(|id| graph.candidates[id.0].local_cost)
                .sum();
        } else if let Some(bound) = proposal.model_lower_bound {
            lower_bound = lower_bound.max(bound);
        }
        if let Some(best) = &incumbent {
            if closes_gap(lower_bound, best.actual_cost) {
                break CandidateSearchStatus::Optimal;
            }
        }
        if started.elapsed() >= limits.wall_time {
            break CandidateSearchStatus::TimeLimit;
        }
        // Exact cuts must raise a repeated feasible proposal to its observed cost.
        // If numerical tolerances prevent convergence, stop instead of re-evaluating forever.
        if observations.iter().any(|observation: &ObservedAssignment| {
            same_assignment(&observation.selected, &proposal.selected)
        }) {
            break CandidateSearchStatus::NumericalStall;
        }
        refinements += 1;
        let validation_started = Instant::now();
        let evaluated = evaluate(&proposal.selected);
        validation_seconds += validation_started.elapsed().as_secs_f64();
        let feedback = match evaluated {
            Ok(cost) => cost,
            Err(error) => break CandidateSearchStatus::EvaluatorStopped(error),
        };
        let actual_cost = feedback.actual_cost;
        if actual_cost.is_some() && !feedback.rejected_queries.is_empty() {
            return Err("feasible evaluator result cannot reject query subgraphs".into());
        }
        if let Some(cost) = actual_cost {
            if !cost.is_finite() || cost < 0.0 {
                return Err("evaluator returned a nonfinite or negative cost".into());
            }
            if cost < proposal.selected.modeled_cost {
                return Err(format!(
                    "actual cost {cost} is below certified local bound {}",
                    proposal.selected.modeled_cost
                ));
            }
            if lower_bound > cost && !closes_gap(lower_bound, cost) {
                return Err("model lower bound exceeds validated feasible cost".into());
            }
            if incumbent
                .as_ref()
                .is_none_or(|best| cost < best.actual_cost)
            {
                incumbent = Some(EvaluatedCandidate {
                    selected: proposal.selected.clone(),
                    actual_cost: cost,
                });
            }
        }
        observations.push(ObservedAssignment {
            selected: proposal.selected,
            exact_cost: actual_cost,
            rejected_queries: feedback.rejected_queries,
        });
        if started.elapsed() >= limits.wall_time {
            break CandidateSearchStatus::TimeLimit;
        }
        if incumbent
            .as_ref()
            .is_some_and(|best| closes_gap(lower_bound, best.actual_cost))
        {
            break CandidateSearchStatus::Optimal;
        }
    };
    let relative_gap = incumbent.as_ref().map(|best| {
        if best.actual_cost == 0.0 {
            0.0
        } else {
            ((best.actual_cost - lower_bound) / best.actual_cost).max(0.0)
        }
    });
    Ok(CandidateSearchResult {
        incumbent,
        status,
        lower_bound,
        relative_gap,
        refinements,
        solve_metrics,
        validation_seconds,
        certified_query_cuts: observations.iter().map(|o| o.rejected_queries.len()).sum(),
    })
}

fn closes_gap(lower: f64, upper: f64) -> bool {
    (upper - lower).abs() <= 1e-9 * upper.abs().max(lower.abs())
}

fn same_assignment(left: &SelectedCandidateGraph, right: &SelectedCandidateGraph) -> bool {
    left.selected_candidates.len() == right.selected_candidates.len()
        && left
            .selected_candidates
            .iter()
            .all(|id| right.selected_candidates.contains(id))
        && left.connections.len() == right.connections.len()
        && left
            .connections
            .iter()
            .all(|edge| right.connections.contains(edge))
        && left.query_roots.len() == right.query_roots.len()
        && left
            .query_roots
            .iter()
            .all(|root| right.query_roots.contains(root))
}

#[cfg(test)]
mod tests {
    use super::super::candidate_connections::{
        PhysicalCandidate, PhysicalCandidateId, ProducerRequirement, QueryRootCandidates,
    };
    use super::*;
    use asap_logical_asap_optimization::memo::MemoGroupId;

    fn choices(costs: &[f64]) -> CandidateDependencyGraph {
        CandidateDependencyGraph {
            candidates: costs
                .iter()
                .enumerate()
                .map(|(id, &local_cost)| PhysicalCandidate {
                    group: MemoGroupId(id as u32),
                    local_cost,
                    producer_requirements: vec![],
                })
                .collect(),
            query_roots: vec![QueryRootCandidates {
                compatible_candidates: (0..costs.len()).map(PhysicalCandidateId).collect(),
            }],
            ..Default::default()
        }
    }
    fn limits(max_refinements: usize) -> CandidateSearchLimits {
        CandidateSearchLimits {
            max_refinements,
            wall_time: Duration::from_secs(10),
        }
    }

    // A later unproven solve can improve the validated incumbent without replacing
    // the earlier certified bound or falsely closing the optimality gap.
    #[test]
    fn nonoptimal_proposal_preserves_bound_and_improves_incumbent() {
        use super::super::candidate_connections::{validate_selected_graph, QueryRootAssignment};
        let graph = choices(&[2.0, 3.0]);
        let mut round = 0;
        let result = search_with_solver(
            &graph,
            limits(2),
            false,
            None,
            |selected| {
                Ok(Some(if selected.selected_candidates[0].0 == 0 {
                    20.0
                } else {
                    10.0
                })
                .into())
            },
            |graph, observations, _, _| {
                assert_eq!(observations.len(), round);
                let selected = SelectedCandidateGraph {
                    selected_candidates: vec![PhysicalCandidateId(round)],
                    connections: Vec::new(),
                    query_roots: vec![QueryRootAssignment {
                        query_root: 0,
                        candidate: PhysicalCandidateId(round),
                    }],
                    modeled_cost: graph.candidates[round].local_cost,
                };
                validate_selected_graph(graph, &selected).unwrap();
                let model_lower_bound = (round == 0).then_some(2.0);
                round += 1;
                Ok(Some(CandidateProposal {
                    selected,
                    model_lower_bound,
                }))
            },
        )
        .unwrap();
        assert_eq!(result.status, CandidateSearchStatus::RefinementLimit);
        assert_eq!(result.incumbent.unwrap().actual_cost, 10.0);
        assert_eq!(result.lower_bound, 2.0);
        assert_eq!(result.relative_gap, Some(0.8));
        assert_eq!(result.refinements, 2);
    }

    #[test]
    fn certified_feedback_survives_heuristic_phase_without_changing_cost_bounds() {
        let mut graph = choices(&[0.0, 0.0, 0.0, 0.0]);
        graph.query_roots = vec![
            QueryRootCandidates {
                compatible_candidates: vec![PhysicalCandidateId(0), PhysicalCandidateId(1)],
            },
            QueryRootCandidates {
                compatible_candidates: vec![PhysicalCandidateId(2), PhysicalCandidateId(3)],
            },
        ];
        let mut bad_evaluations = 0;
        let result =
            search_candidates_with_feedback(
                &graph,
                limits(10),
                Some(&[1.0, 10.0, 1.0, 2.0]),
                |selected| {
                    assert_eq!(selected.modeled_cost, 0.0);
                    if selected.query_roots.iter().any(|root| {
                        root.query_root == 0 && root.candidate == PhysicalCandidateId(0)
                    }) {
                        bad_evaluations += 1;
                        Ok(CandidateEvaluation {
                            actual_cost: None,
                            rejected_queries: vec![RejectedQuerySubgraph::from_query_outputs(
                                selected,
                                &[0],
                            )?],
                        })
                    } else {
                        Ok(Some(2.0).into())
                    }
                },
            )
            .unwrap();
        assert_eq!(bad_evaluations, 1);
        assert_eq!(result.certified_query_cuts, 1);
        assert_eq!(result.incumbent.unwrap().actual_cost, 2.0);
        assert_eq!(result.status, CandidateSearchStatus::Optimal);
    }

    // Structural node counts must never be reported as monetary lower bounds.
    #[test]
    fn structural_seed_uses_original_bound_and_favors_shared_nodes() {
        let mut graph = choices(&[0.25, 0.25, 0.25]);
        graph.query_roots = vec![
            QueryRootCandidates {
                compatible_candidates: vec![PhysicalCandidateId(0), PhysicalCandidateId(2)],
            },
            QueryRootCandidates {
                compatible_candidates: vec![PhysicalCandidateId(1), PhysicalCandidateId(2)],
            },
        ];
        let result = search_candidates_with_baseline(&graph, limits(1), |selected| {
            assert_eq!(selected.selected_candidates, vec![PhysicalCandidateId(2)]);
            assert_eq!(selected.modeled_cost, 0.25);
            Ok(Some(0.5))
        })
        .unwrap();
        assert_eq!(result.status, CandidateSearchStatus::RefinementLimit);
        assert_eq!(result.lower_bound, 0.0);
        assert_eq!(result.relative_gap, Some(1.0));
        assert_eq!(result.incumbent.unwrap().actual_cost, 0.5);
        assert_eq!(result.refinements, 1);
        assert_eq!(result.solve_metrics.solve_calls, 1);
    }

    // A supplied baseline preference changes proposal order, never the cost model.
    #[test]
    fn weighted_seed_changes_order_without_becoming_a_bound() {
        let graph = choices(&[0.1, 0.2]);
        let result = search_candidates_with_weighted_baseline(
            &graph,
            limits(1),
            Some(&[100.0, 1.0]),
            |selected| {
                assert_eq!(selected.query_roots[0].candidate, PhysicalCandidateId(1));
                assert_eq!(selected.modeled_cost, 0.2);
                Ok(Some(0.25))
            },
        )
        .unwrap();
        assert_eq!(result.lower_bound, 0.0);
        assert_eq!(result.relative_gap, Some(1.0));
        assert_eq!(result.incumbent.unwrap().actual_cost, 0.25);
        assert_eq!(graph.candidates[0].local_cost, 0.1);
        assert!(search_candidates_with_weighted_baseline(
            &graph,
            limits(1),
            Some(&[f64::NAN, 1.0]),
            |_| panic!("invalid weights must not be evaluated")
        )
        .is_err());
    }

    // The same certified bound check applies to the initial structural proposal.
    #[test]
    fn structural_seed_cannot_violate_original_cost_bound() {
        let error = search_candidates_with_baseline(&choices(&[2.0]), limits(1), |selected| {
            assert_eq!(selected.modeled_cost, 2.0);
            Ok(Some(1.0))
        })
        .unwrap_err();
        assert!(error.contains("certified local bound"));
    }

    // Infeasible heuristic proposals are excluded only after actual validation.
    #[test]
    fn structural_seed_rejections_continue_and_internal_errors_stop() {
        let mut calls = 0;
        let result = search_candidates_with_baseline(&choices(&[0.0; 2]), limits(2), |_| {
            calls += 1;
            Ok((calls == 2).then_some(0.5))
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert!(result.incumbent.is_some());
        assert_eq!(result.lower_bound, 0.0);
        let stopped = search_candidates_with_baseline(&choices(&[0.0; 2]), limits(3), |_| {
            Err("broken identity map".into())
        })
        .unwrap();
        assert_eq!(stopped.refinements, 1);
        assert!(matches!(
            stopped.status,
            CandidateSearchStatus::EvaluatorStopped(_)
        ));
    }

    // A baseline validation that overruns the shared deadline still retains its plan.
    #[test]
    fn structural_seed_timeout_preserves_validated_incumbent() {
        let result = search_candidates_with_baseline(
            &choices(&[0.0]),
            CandidateSearchLimits {
                max_refinements: 10,
                wall_time: Duration::from_secs(1),
            },
            |_| {
                std::thread::sleep(Duration::from_millis(1100));
                Ok(Some(0.5))
            },
        )
        .unwrap();
        assert_eq!(result.status, CandidateSearchStatus::TimeLimit);
        assert_eq!(result.refinements, 1);
        assert_eq!(result.incumbent.unwrap().actual_cost, 0.5);
        assert_eq!(result.lower_bound, 0.0);
    }

    // Actual evaluation can reverse the ordering suggested by certified local bounds.
    #[test]
    fn evaluator_can_prefer_a_costlier_local_choice() {
        let graph = choices(&[1.0, 2.0]);
        let result = search_candidates(&graph, limits(10), |selected| {
            Ok(Some(if selected.query_roots[0].candidate.0 == 0 {
                100.0
            } else {
                3.0
            }))
        })
        .unwrap();
        assert_eq!(result.status, CandidateSearchStatus::Optimal);
        assert_eq!(result.incumbent.unwrap().actual_cost, 3.0);
        assert_eq!(result.relative_gap, Some(0.0));
    }

    // An infeasible assignment is excluded, not returned as an incumbent.
    #[test]
    fn invalid_proposals_are_excluded() {
        let graph = choices(&[1.0, 2.0]);
        let result = search_candidates(&graph, limits(10), |selected| {
            Ok((selected.query_roots[0].candidate.0 == 1).then_some(2.0))
        })
        .unwrap();
        assert_eq!(result.status, CandidateSearchStatus::Optimal);
        assert_eq!(result.refinements, 2);
        assert_eq!(result.solve_metrics.solve_calls, 2);
        assert!(result.solve_metrics.model_construction_seconds > 0.0);
        assert!(result.solve_metrics.solver_seconds > 0.0);
        assert!(result.validation_seconds > 0.0);
        assert_eq!(result.incumbent.unwrap().actual_cost, 2.0);
        let impossible = search_candidates(&graph, limits(10), |_| Ok(None)).unwrap();
        assert_eq!(impossible.status, CandidateSearchStatus::Infeasible);
        assert!(impossible.incumbent.is_none());
        assert_eq!(impossible.solve_metrics.solve_calls, 3);
    }

    // Adding a candidate changes the context: positive-only activation cuts are unsound.
    #[test]
    fn observed_cost_cut_includes_absent_candidate_decisions() {
        let mut graph = choices(&[1.0, 1.0]);
        graph.candidates[1]
            .producer_requirements
            .push(ProducerRequirement {
                compatible_producers: vec![PhysicalCandidateId(0)],
            });
        let result = search_candidates(&graph, limits(10), |selected| {
            Ok(Some(if selected.selected_candidates.len() == 1 {
                100.0
            } else {
                2.0
            }))
        })
        .unwrap();
        assert_eq!(result.status, CandidateSearchStatus::Optimal);
        assert_eq!(result.incumbent.unwrap().actual_cost, 2.0);
    }

    // A refinement limit preserves the validated incumbent without claiming optimality.
    #[test]
    fn bounded_refinement_preserves_incumbent_and_reports_gap() {
        let result =
            search_candidates(&choices(&[1.0, 2.0]), limits(1), |_| Ok(Some(100.0))).unwrap();
        assert_eq!(result.status, CandidateSearchStatus::RefinementLimit);
        assert_eq!(result.incumbent.unwrap().actual_cost, 100.0);
        assert!(result.relative_gap.unwrap() > 0.9);
        let no_time = search_candidates(
            &choices(&[1.0]),
            CandidateSearchLimits {
                wall_time: Duration::ZERO,
                ..limits(10)
            },
            |_| panic!("deadline must stop evaluation"),
        )
        .unwrap();
        assert_eq!(no_time.status, CandidateSearchStatus::TimeLimit);
        assert!(no_time.incumbent.is_none());
        assert_eq!(no_time.relative_gap, None);
    }

    // All tiny choices are priced independently as an exhaustive optimum reference.
    #[test]
    fn refined_optimum_matches_tiny_exhaustive_costs() {
        let costs = [12.0_f64, 7.0, 18.0, 5.0];
        let result = search_candidates(&choices(&[0.0; 4]), limits(10), |selected| {
            Ok(Some(costs[selected.query_roots[0].candidate.0]))
        })
        .unwrap();
        assert_eq!(result.status, CandidateSearchStatus::Optimal);
        assert_eq!(
            result.incumbent.unwrap().actual_cost,
            costs.into_iter().fold(f64::INFINITY, f64::min)
        );
    }

    // Root and connection decisions matter even when every candidate stays selected.
    #[test]
    fn observed_context_distinguishes_connections_and_output_assignments() {
        let mut graph = choices(&[0.0; 3]);
        graph.candidates[2]
            .producer_requirements
            .push(ProducerRequirement {
                compatible_producers: vec![PhysicalCandidateId(0), PhysicalCandidateId(1)],
            });
        graph.query_roots = (0..3)
            .map(|id| QueryRootCandidates {
                compatible_candidates: vec![PhysicalCandidateId(id)],
            })
            .collect();
        graph.query_roots.push(QueryRootCandidates {
            compatible_candidates: vec![PhysicalCandidateId(0), PhysicalCandidateId(1)],
        });
        let mut seen = Vec::new();
        let result = search_candidates(&graph, limits(10), |selected| {
            let context = (
                selected.connections[0].producer.0,
                selected
                    .query_roots
                    .iter()
                    .find(|root| root.query_root == 3)
                    .unwrap()
                    .candidate
                    .0,
            );
            assert!(!seen.contains(&context));
            seen.push(context);
            Ok(Some(10.0 + context.0 as f64 + 2.0 * context.1 as f64))
        })
        .unwrap();
        assert_eq!(result.status, CandidateSearchStatus::Optimal);
        assert_eq!(result.incumbent.unwrap().actual_cost, 10.0);
        assert_eq!(seen.len(), 4);
    }

    // An evaluator failure preserves earlier validated work; invalid bounds are errors.
    #[test]
    fn evaluator_failure_preserves_incumbent_and_bad_certification_fails() {
        let mut evaluations = 0;
        let result = search_candidates(&choices(&[1.0, 2.0]), limits(10), |_| {
            evaluations += 1;
            if evaluations == 1 {
                Ok(Some(100.0))
            } else {
                Err("validation unavailable".into())
            }
        })
        .unwrap();
        assert!(matches!(
            result.status,
            CandidateSearchStatus::EvaluatorStopped(_)
        ));
        assert_eq!(result.incumbent.unwrap().actual_cost, 100.0);
        assert!(
            search_candidates(&choices(&[2.0]), limits(10), |_| Ok(Some(1.0)))
                .unwrap_err()
                .contains("certified local bound")
        );
    }
    // The callback cannot be preempted, but a late validated result remains usable.
    #[test]
    fn callback_overrun_preserves_validated_incumbent() {
        let result = search_candidates(
            &choices(&[1.0]),
            CandidateSearchLimits {
                max_refinements: 10,
                wall_time: Duration::from_secs(2),
            },
            |_| {
                std::thread::sleep(Duration::from_millis(2100));
                Ok(Some(10.0))
            },
        )
        .unwrap();
        assert_eq!(result.status, CandidateSearchStatus::TimeLimit);
        assert_eq!(result.incumbent.unwrap().actual_cost, 10.0);
    }
}
