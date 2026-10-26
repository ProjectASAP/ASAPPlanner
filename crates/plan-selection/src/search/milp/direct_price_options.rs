//! Joint selection of supplier-dependent local pricing interfaces.
//! Pricing options describe one physical candidate, not independent executions.
use super::direct_cost::{NodeCostProfile, QueryScheduleRates};
use crate::search::candidates::graph::PhysicalCandidateId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SupplierPriceBinding {
    pub producer: PhysicalCandidateId,
    pub price_option: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct CandidatePriceOption {
    pub profile: NodeCostProfile,
    pub compatible_suppliers: Vec<SupplierPriceBinding>,
}

#[derive(Debug, Clone)]
pub(crate) struct JointPriceProfile {
    pub schedules: Option<QueryScheduleRates>,
    pub query_rate: f64,
    pub request_latency_seconds: f64,
    pub request_cost: f64,
    pub candidates: Vec<Vec<CandidatePriceOption>>,
}

use super::direct_cost::DirectCostProfile;
use super::model::{CandidateProposal, CandidateSolveMetrics, ObservedAssignment};
use crate::search::candidates::graph::*;
use good_lp::{constraint, Constraint, Expression, Variable};

/// Internal option nodes share an original physical identity. At most one may
/// activate, so two readers cannot obtain incompatible views of one producer.
pub(super) struct PriceOptionProjection<'a> {
    graph: &'a CandidateDependencyGraph,
    origins: Vec<PhysicalCandidateId>,
    options: Vec<Vec<PhysicalCandidateId>>,
    observations: &'a [ObservedAssignment],
}

impl PriceOptionProjection<'_> {
    fn connection(&self, edge: ProducerConsumerConnection) -> ProducerConsumerConnection {
        ProducerConsumerConnection {
            producer: self.origins[edge.producer.0],
            consumer: self.origins[edge.consumer.0],
            requirement: edge.requirement,
        }
    }

    fn root(&self, root: QueryRootAssignment) -> QueryRootAssignment {
        QueryRootAssignment {
            candidate: self.origins[root.candidate.0],
            ..root
        }
    }

    pub(super) fn constrain_activation(
        &self,
        active: &[Variable],
        constraints: &mut Vec<Constraint>,
    ) {
        let sum = |id: PhysicalCandidateId| -> Expression {
            self.options[id.0]
                .iter()
                .map(|option| active[option.0])
                .sum()
        };
        for (id, options) in self.options.iter().enumerate() {
            if options.len() > 1 {
                constraints.push(constraint!(sum(PhysicalCandidateId(id)) <= 1.0));
            }
        }
        for rule in &self.graph.reader_constraints {
            if let ReaderConstraint::ActivationRequires {
                candidate,
                required_candidate,
            } = rule
            {
                constraints.push(constraint!(sum(*candidate) <= sum(*required_candidate)));
            }
        }
    }

    /// A rejected physical DAG remains rejected across all of its price-option
    /// encodings. Cuts refer to sums over the original activation/edge identity.
    pub(super) fn constrain_observations(
        &self,
        active: &[Variable],
        edges: &[(ProducerConsumerConnection, Variable)],
        roots: &[(QueryRootAssignment, Variable)],
        constraints: &mut Vec<Constraint>,
    ) {
        for observation in self.observations {
            for witness in &observation.rejected_queries {
                let mut changed = Expression::from(
                    (witness.connections.len() + witness.query_roots.len()) as f64,
                );
                for (edge, variable) in edges {
                    if witness.connections.contains(&self.connection(*edge)) {
                        changed -= *variable;
                    }
                }
                for (root, variable) in roots {
                    if witness.query_roots.contains(&self.root(*root)) {
                        changed -= *variable;
                    }
                }
                constraints.push(constraint!(changed >= 1.0));
            }
            if observation.exact_cost.is_some() {
                continue;
            }
            let selected = &observation.selected;
            let mut mismatch = Expression::from(
                (selected.selected_candidates.len()
                    + selected.connections.len()
                    + selected.query_roots.len()) as f64,
            );
            for (id, variable) in active.iter().enumerate() {
                mismatch += if selected.selected_candidates.contains(&self.origins[id]) {
                    -1.0
                } else {
                    1.0
                } * *variable;
            }
            for (edge, variable) in edges {
                mismatch += if selected.connections.contains(&self.connection(*edge)) {
                    -1.0
                } else {
                    1.0
                } * *variable;
            }
            for (root, variable) in roots {
                mismatch += if selected.query_roots.contains(&self.root(*root)) {
                    -1.0
                } else {
                    1.0
                } * *variable;
            }
            constraints.push(constraint!(mismatch >= 1.0));
        }
    }
}

pub(crate) fn solve_joint_price_proposal_with_metrics(
    graph: &CandidateDependencyGraph,
    observations: &[ObservedAssignment],
    remaining_seconds: Option<f64>,
    metrics: &mut CandidateSolveMetrics,
    profile: &JointPriceProfile,
) -> Result<Option<CandidateProposal>, CandidateSelectionError> {
    let started = std::time::Instant::now();
    graph.validate()?;
    super::model::validate_observations(graph, observations)?;
    let invalid = |message: &str| CandidateSelectionError::InvalidGraph(message.into());
    if profile.candidates.len() != graph.candidates.len() {
        return Err(invalid(
            "joint price options do not match candidate inventory",
        ));
    }
    let mut projection = PriceOptionProjection {
        graph,
        origins: Vec::new(),
        options: Vec::new(),
        observations,
    };
    let mut expanded = CandidateDependencyGraph::default();
    let mut nodes = Vec::new();
    for (id, options) in profile.candidates.iter().enumerate() {
        let mut ids = Vec::new();
        for option in options {
            ids.push(PhysicalCandidateId(nodes.len()));
            projection.origins.push(PhysicalCandidateId(id));
            nodes.push(option.profile.clone());
            expanded.candidates.push(PhysicalCandidate {
                producer_requirements: Vec::new(),
                ..graph.candidates[id].clone()
            });
        }
        projection.options.push(ids);
    }
    for (id, options) in profile.candidates.iter().enumerate() {
        if remaining_seconds.is_some_and(|s| started.elapsed().as_secs_f64() >= s) {
            return Err(CandidateSelectionError::Solver(
                "time budget exhausted expanding price options".into(),
            ));
        }
        let required = &graph.candidates[id].producer_requirements;
        if !options.is_empty() && required.len() > 1 {
            return Err(invalid(
                "joint unary price profile has multiple required inputs",
            ));
        }
        for (index, option) in options.iter().enumerate() {
            if required.is_empty() && !option.compatible_suppliers.is_empty() {
                return Err(invalid("source price option has supplier bindings"));
            }
            if let Some(input) = required.first() {
                let mut suppliers = Vec::new();
                for binding in &option.compatible_suppliers {
                    if !input.compatible_producers.contains(&binding.producer) {
                        return Err(invalid("price option references incompatible supplier"));
                    }
                    let supplier = projection
                        .options
                        .get(binding.producer.0)
                        .and_then(|options| options.get(binding.price_option))
                        .ok_or_else(|| {
                            invalid("price option references missing supplier option")
                        })?;
                    if !suppliers.contains(supplier) {
                        suppliers.push(*supplier);
                    }
                }
                expanded.candidates[projection.options[id][index].0]
                    .producer_requirements
                    .push(ProducerRequirement {
                        compatible_producers: suppliers.into(),
                    });
            }
        }
    }
    for root in &graph.query_roots {
        expanded.query_roots.push(QueryRootCandidates {
            compatible_candidates: root
                .compatible_candidates
                .iter()
                .flat_map(|id| projection.options[id.0].iter().copied())
                .collect(),
        });
    }
    if expanded
        .query_roots
        .iter()
        .any(|root| root.compatible_candidates.is_empty())
    {
        return Ok(None);
    }
    for budget in &graph.resource_budgets {
        expanded.resource_budgets.push(ResourceBudget {
            name: budget.name.clone(),
            capacity: budget.capacity,
            candidate_usage: projection
                .origins
                .iter()
                .map(|id| budget.candidate_usage[id.0])
                .collect(),
        });
    }
    for rule in &graph.reader_constraints {
        let (candidate, mapped): (_, Box<dyn Fn(PhysicalCandidateId) -> ReaderConstraint>) =
            match rule {
                ReaderConstraint::ActivationRequires { .. } => continue,
                ReaderConstraint::MinimumQueryReaders {
                    candidate,
                    query_outputs,
                    minimum,
                } => (
                    *candidate,
                    Box::new(|candidate| ReaderConstraint::MinimumQueryReaders {
                        candidate,
                        query_outputs: query_outputs.clone(),
                        minimum: *minimum,
                    }),
                ),
                ReaderConstraint::HighestSharedQueryMaterialization {
                    candidate,
                    query_outputs,
                } => (
                    *candidate,
                    Box::new(
                        |candidate| ReaderConstraint::HighestSharedQueryMaterialization {
                            candidate,
                            query_outputs: query_outputs.clone(),
                        },
                    ),
                ),
                ReaderConstraint::AllowedOutputs { candidate, outputs } => (
                    *candidate,
                    Box::new(|candidate| ReaderConstraint::AllowedOutputs {
                        candidate,
                        outputs: outputs.clone(),
                    }),
                ),
                ReaderConstraint::IncompatibleOutputs {
                    candidate,
                    left,
                    right,
                } => (
                    *candidate,
                    Box::new(|candidate| ReaderConstraint::IncompatibleOutputs {
                        candidate,
                        left: *left,
                        right: *right,
                    }),
                ),
            };
        expanded
            .reader_constraints
            .extend(projection.options[candidate.0].iter().map(|id| mapped(*id)));
    }
    let fixed = DirectCostProfile {
        nodes,
        schedules: profile.schedules.clone(),
        query_rate: profile.query_rate,
        request_latency_seconds: profile.request_latency_seconds,
        request_cost: profile.request_cost,
    };
    metrics.model_construction_seconds += started.elapsed().as_secs_f64();
    let remaining = remaining_seconds.map(|s| (s - started.elapsed().as_secs_f64()).max(0.0));
    metrics.joint_price_solve_calls += 1;
    let proposal = super::model::solve_with_objective(
        &expanded,
        &[],
        remaining,
        metrics,
        Some(&fixed),
        Some(&projection),
    )?;
    proposal
        .map(|mut proposal| {
            let selected = &mut proposal.selected;
            selected.selected_candidates = selected
                .selected_candidates
                .iter()
                .map(|id| projection.origins[id.0])
                .collect();
            selected.selected_candidates.sort();
            if selected
                .selected_candidates
                .windows(2)
                .any(|pair| pair[0] == pair[1])
            {
                return Err(invalid(
                    "multiple price options selected for one physical candidate",
                ));
            }
            selected.connections = selected
                .connections
                .iter()
                .map(|edge| projection.connection(*edge))
                .collect();
            selected.query_roots = selected
                .query_roots
                .iter()
                .map(|root| projection.root(*root))
                .collect();
            selected.modeled_cost = selected
                .selected_candidates
                .iter()
                .map(|id| graph.candidates[id.0].local_cost)
                .sum();
            validate_selected_graph(graph, selected)
                .map_err(CandidateSelectionError::InvalidGraph)?;
            Ok(proposal)
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_logical_asap_optimization::memo::MemoGroupId;

    fn candidate(inputs: &[usize]) -> PhysicalCandidate {
        PhysicalCandidate {
            group: MemoGroupId(0),
            local_cost: 0.0,
            producer_requirements: if inputs.is_empty() {
                vec![]
            } else {
                vec![ProducerRequirement {
                    compatible_producers: inputs
                        .iter()
                        .map(|id| PhysicalCandidateId(*id))
                        .collect::<Vec<_>>()
                        .into(),
                }]
            },
        }
    }
    fn option(
        cost: f64,
        materialized: bool,
        producer: Option<(usize, usize)>,
    ) -> CandidatePriceOption {
        CandidatePriceOption {
            profile: NodeCostProfile {
                execution_cost: cost,
                materialized,
                ..Default::default()
            },
            compatible_suppliers: producer
                .into_iter()
                .map(|(id, price_option)| SupplierPriceBinding {
                    producer: PhysicalCandidateId(id),
                    price_option,
                })
                .collect(),
        }
    }
    fn fixture() -> (CandidateDependencyGraph, JointPriceProfile) {
        let graph = CandidateDependencyGraph {
            candidates: vec![
                candidate(&[]),
                candidate(&[]),
                candidate(&[0, 1]),
                candidate(&[2]),
                candidate(&[2]),
            ],
            query_roots: vec![
                QueryRootCandidates {
                    compatible_candidates: vec![PhysicalCandidateId(3)],
                },
                QueryRootCandidates {
                    compatible_candidates: vec![PhysicalCandidateId(4)],
                },
            ],
            ..Default::default()
        };
        let profile = JointPriceProfile {
            schedules: None,
            query_rate: 1.0,
            request_latency_seconds: 0.0,
            request_cost: 0.0,
            candidates: vec![
                vec![option(1.0, false, None)],
                vec![option(5.0, false, None)],
                vec![
                    option(0.0, true, Some((0, 0))),
                    option(0.0, true, Some((1, 0))),
                ],
                vec![
                    option(1.0, false, Some((2, 0))),
                    option(100.0, false, Some((2, 1))),
                ],
                vec![
                    option(100.0, false, Some((2, 0))),
                    option(1.0, false, Some((2, 1))),
                ],
            ],
        };
        (graph, profile)
    }

    // Readers of the same shared state must agree on its upstream price interface.
    #[test]
    fn shared_producer_has_one_joint_price_configuration() {
        let (graph, profile) = fixture();
        let proposal = solve_joint_price_proposal_with_metrics(
            &graph,
            &[],
            None,
            &mut CandidateSolveMetrics::default(),
            &profile,
        )
        .unwrap()
        .unwrap();
        assert_eq!(proposal.direct_modeled_cost, Some(102.0));
        assert!(proposal
            .selected
            .selected_candidates
            .contains(&PhysicalCandidateId(0)));
        assert!(!proposal
            .selected
            .selected_candidates
            .contains(&PhysicalCandidateId(1)));
        assert_eq!(
            proposal
                .selected
                .selected_candidates
                .iter()
                .filter(|id| id.0 == 2)
                .count(),
            1
        );
    }

    // Rejecting an original connection assignment excludes all internal encodings.
    #[test]
    fn projected_rejection_selects_the_other_supplier() {
        let (graph, profile) = fixture();
        let first = solve_joint_price_proposal_with_metrics(
            &graph,
            &[],
            None,
            &mut CandidateSolveMetrics::default(),
            &profile,
        )
        .unwrap()
        .unwrap();
        let observations = vec![ObservedAssignment {
            selected: first.selected,
            exact_cost: None,
            rejected_queries: vec![],
        }];
        let second = solve_joint_price_proposal_with_metrics(
            &graph,
            &observations,
            None,
            &mut CandidateSolveMetrics::default(),
            &profile,
        )
        .unwrap()
        .unwrap();
        assert_eq!(second.direct_modeled_cost, Some(106.0));
        assert!(second
            .selected
            .selected_candidates
            .contains(&PhysicalCandidateId(1)));
    }
    // Resource and reader rules still refer to the physical candidate identity.
    #[test]
    fn original_capacity_and_reader_constraints_survive_option_expansion() {
        let (mut graph, profile) = fixture();
        graph.resource_budgets.push(ResourceBudget {
            name: "exclude first source".into(),
            capacity: 0.0,
            candidate_usage: vec![1.0, 0.0, 0.0, 0.0, 0.0],
        });
        graph
            .reader_constraints
            .push(ReaderConstraint::MinimumQueryReaders {
                candidate: PhysicalCandidateId(2),
                query_outputs: vec![vec![0], vec![1]],
                minimum: 2,
            });
        let proposal = solve_joint_price_proposal_with_metrics(
            &graph,
            &[],
            None,
            &mut CandidateSolveMetrics::default(),
            &profile,
        )
        .unwrap()
        .unwrap();
        assert_eq!(proposal.direct_modeled_cost, Some(106.0));
    }
}
