//! Bounded evaluator refinement over candidate connections, without enumerating
//! complete plans up front. Full-assignment cuts are a correctness fallback;
//! they make no claim of scalable demand modeling or support for every cost model.
use std::time::{Duration, Instant};

use super::candidate_connections::{
    solve_candidate_proposal_with_metrics, CandidateDependencyGraph, CandidateSolveMetrics,
    ObservedAssignment, SelectedCandidateGraph,
};

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
}

/// The callback returns an actual total cost for a validated feasible plan, or
/// `None` for a proven infeasible assignment. Errors stop search, preserving the
/// incumbent. Its cost must include every configured cost component and must be
/// deterministic for an assignment. Local graph costs are certified lower bounds,
/// not estimates. A violated certification is an error, never silently repaired.
/// Time bounds are cooperative: a blocking evaluator cannot be preempted here.
pub(super) fn search_candidates(
    graph: &CandidateDependencyGraph,
    limits: CandidateSearchLimits,
    mut evaluate: impl FnMut(&SelectedCandidateGraph) -> Result<Option<f64>, String>,
) -> Result<CandidateSearchResult, String> {
    graph.validate().map_err(|error| error.to_string())?;
    let started = Instant::now();
    let mut observations = Vec::new();
    let mut incumbent: Option<EvaluatedCandidate> = None;
    let mut lower_bound: f64 = 0.0;
    let mut refinements = 0;
    let mut solve_metrics = CandidateSolveMetrics::default();
    let mut validation_seconds = 0.0;
    let status = loop {
        let remaining = limits.wall_time.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break CandidateSearchStatus::TimeLimit;
        }
        if refinements >= limits.max_refinements {
            break CandidateSearchStatus::RefinementLimit;
        }
        let proposal = match solve_candidate_proposal_with_metrics(
            graph,
            &observations,
            Some(remaining.as_secs_f64()),
            &mut solve_metrics,
        ) {
            Ok(Some(proposal)) => proposal,
            Ok(None) => {
                if incumbent.is_some() {
                    return Err("refined model excluded a validated feasible assignment".into());
                }
                break CandidateSearchStatus::Infeasible;
            }
            Err(error) => {
                break if started.elapsed() >= limits.wall_time {
                    CandidateSearchStatus::TimeLimit
                } else {
                    CandidateSearchStatus::SolverStopped(error.to_string())
                };
            }
        };
        lower_bound = lower_bound.max(proposal.model_lower_bound);
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
        let actual_cost = match evaluated {
            Ok(cost) => cost,
            Err(error) => break CandidateSearchStatus::EvaluatorStopped(error),
        };
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
