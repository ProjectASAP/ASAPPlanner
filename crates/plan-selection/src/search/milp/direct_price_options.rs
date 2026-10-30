//! Joint selection of supplier-dependent local pricing interfaces.
//! Pricing options describe one physical candidate, not independent executions.
use super::direct_cost::{NodeCostProfile, QueryScheduleRates};
use asap_types::stages::PhysicalCandidateId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SupplierPriceBinding {
    pub producer: PhysicalCandidateId,
    pub price_option: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct CandidatePriceOption {
    pub profile: NodeCostProfile,
    pub compatible_input_tuples: Vec<Vec<SupplierPriceBinding>>,
    pub required_range_span_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub(crate) struct JointPriceProfile {
    pub schedules: Option<QueryScheduleRates>,
    pub query_rate: f64,
    pub request_latency_seconds: f64,
    pub request_cost: f64,
    pub candidates: Vec<Vec<CandidatePriceOption>>,
    pub query_range_spans_ms: Vec<u64>,
}

use super::direct_cost::DirectCostProfile;
use super::model::{CandidateProposal, CandidateSolveMetrics, ObservedAssignment};
use crate::search::candidates::dag::*;
use asap_types::stages::*;
use good_lp::{constraint, variable, Constraint, Expression, ProblemVariables, Variable};

/// Internal option nodes share an original physical identity. At most one may
/// activate, so two readers cannot obtain incompatible views of one producer.
pub(super) struct PriceOptionProjection<'a> {
    graph: &'a DagOfPhysicalCandidatesWithCosts,
    origins: Vec<PhysicalCandidateId>,
    options: Vec<Vec<PhysicalCandidateId>>,
    observations: &'a [ObservedAssignment],
    input_tuples: Vec<(PhysicalCandidateId, Vec<Vec<PhysicalCandidateId>>)>,
    range_contexts: Vec<(PhysicalCandidateId, u64)>,
    query_spans: &'a [u64],
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

    pub(super) fn constrain_input_tuples(
        &self,
        variables: &mut ProblemVariables,
        active: &[Variable],
        edges: &[(ProducerConsumerConnection, Variable)],
        constraints: &mut Vec<Constraint>,
        check_deadline: impl Fn() -> Result<(), CandidateSelectionError>,
    ) -> Result<(), CandidateSelectionError> {
        let mut by_consumer = std::collections::BTreeMap::<_, Vec<_>>::new();
        for (edge, selected) in edges {
            check_deadline()?;
            by_consumer
                .entry(edge.consumer)
                .or_default()
                .push((edge, selected));
        }
        for (consumer, tuples) in &self.input_tuples {
            check_deadline()?;
            let mut selectors = Vec::new();
            let mut support = std::collections::BTreeMap::<_, Expression>::new();
            for tuple in tuples {
                check_deadline()?;
                let selector = variables.add(variable().binary());
                selectors.push(selector);
                for (position, producer) in tuple.iter().enumerate() {
                    *support
                        .entry((position, *producer))
                        .or_insert_with(|| Expression::from(0.0)) += selector;
                }
            }
            constraints.push(constraint!(
                selectors.iter().copied().sum::<Expression>() == active[consumer.0]
            ));
            for (edge, selected) in by_consumer.get(consumer).into_iter().flatten() {
                check_deadline()?;
                let matches = support
                    .get(&(edge.requirement, edge.producer))
                    .cloned()
                    .unwrap_or_else(|| Expression::from(0.0));
                constraints.push(constraint!(**selected == matches));
            }
        }
        Ok(())
    }

    pub(super) fn constrain_reader_contexts(
        &self,
        active: &[Variable],
        reach: &[Vec<Option<Variable>>],
        constraints: &mut Vec<Constraint>,
        check_deadline: impl Fn() -> Result<(), CandidateSelectionError>,
    ) -> Result<(), CandidateSelectionError> {
        for (candidate, span) in &self.range_contexts {
            check_deadline()?;
            let mut witnesses = Expression::from(0.0);
            for (query, row) in reach.iter().enumerate() {
                check_deadline()?;
                if let Some(reached) = row[candidate.0] {
                    if self.query_spans[query] > *span {
                        constraints.push(constraint!(reached == 0));
                    }
                    if self.query_spans[query] == *span {
                        witnesses += reached;
                    }
                }
            }
            constraints.push(constraint!(active[candidate.0] <= witnesses));
        }
        Ok(())
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
    graph: &DagOfPhysicalCandidatesWithCosts,
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
        input_tuples: Vec::new(),
        range_contexts: Vec::new(),
        query_spans: &profile.query_range_spans_ms,
    };
    let mut expanded = DagOfPhysicalCandidatesWithCosts::default();
    let mut nodes = Vec::new();
    for (id, options) in profile.candidates.iter().enumerate() {
        let mut ids = Vec::new();
        for option in options {
            ids.push(PhysicalCandidateId(nodes.len()));
            projection.origins.push(PhysicalCandidateId(id));
            nodes.push(option.profile.clone());
            expanded.candidates.push(PhysicalCandidateWithCost {
                supplier_candidates_per_input: Vec::new(),
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
        let required = &graph.candidates[id].supplier_candidates_per_input;
        for (index, option) in options.iter().enumerate() {
            let consumer = projection.options[id][index];
            if let Some(span) = option.required_range_span_ms {
                if profile.query_range_spans_ms.len() != graph.query_roots.len() {
                    return Err(invalid("range contexts do not match query roots"));
                }
                projection.range_contexts.push((consumer, span));
            }
            let mut tuples = Vec::new();
            for tuple in &option.compatible_input_tuples {
                if tuple.len() != required.len() {
                    return Err(invalid("price input tuple arity mismatch"));
                }
                let mut mapped = Vec::new();
                for (position, binding) in tuple.iter().enumerate() {
                    if !required[position].candidates.contains(&binding.producer) {
                        return Err(invalid("price tuple references incompatible supplier"));
                    }
                    mapped.push(
                        *projection
                            .options
                            .get(binding.producer.0)
                            .and_then(|options| options.get(binding.price_option))
                            .ok_or_else(|| {
                                invalid("price tuple references missing supplier option")
                            })?,
                    );
                }
                if !tuples.contains(&mapped) {
                    tuples.push(mapped);
                }
            }
            for position in 0..required.len() {
                let mut suppliers = Vec::new();
                for tuple in &tuples {
                    if !suppliers.contains(&tuple[position]) {
                        suppliers.push(tuple[position]);
                    }
                }
                expanded.candidates[consumer.0]
                    .supplier_candidates_per_input
                    .push(InputSupplierCandidates {
                        candidates: suppliers.into(),
                    });
            }
            // Unary choices and single tuples are already exact under the
            // existing one-producer-per-input constraints; no extra binaries.
            let cartesian_size = expanded.candidates[consumer.0]
                .supplier_candidates_per_input
                .iter()
                .try_fold(1usize, |size, input| {
                    size.checked_mul(input.candidates.len())
                });
            if required.len() > 1 && tuples.len() > 1 && cartesian_size != Some(tuples.len()) {
                projection.input_tuples.push((consumer, tuples));
            }
        }
    }
    for root in &graph.query_roots {
        expanded.query_roots.push(QueryOutputCandidates {
            candidates: root
                .candidates
                .iter()
                .flat_map(|id| projection.options[id.0].iter().copied())
                .collect(),
        });
    }
    if expanded
        .query_roots
        .iter()
        .any(|root| root.candidates.is_empty())
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

    fn candidate(inputs: &[usize]) -> PhysicalCandidateWithCost {
        PhysicalCandidateWithCost {
            local_cost: 0.0,
            supplier_candidates_per_input: if inputs.is_empty() {
                vec![]
            } else {
                vec![InputSupplierCandidates {
                    candidates: inputs
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
            required_range_span_ms: None,
            compatible_input_tuples: producer
                .into_iter()
                .map(|(id, price_option)| {
                    vec![SupplierPriceBinding {
                        producer: PhysicalCandidateId(id),
                        price_option,
                    }]
                })
                .collect(),
        }
    }
    fn fixture() -> (DagOfPhysicalCandidatesWithCosts, JointPriceProfile) {
        let graph = DagOfPhysicalCandidatesWithCosts {
            candidates: vec![
                candidate(&[]),
                candidate(&[]),
                candidate(&[0, 1]),
                candidate(&[2]),
                candidate(&[2]),
            ],
            query_roots: vec![
                QueryOutputCandidates {
                    candidates: vec![PhysicalCandidateId(3)],
                },
                QueryOutputCandidates {
                    candidates: vec![PhysicalCandidateId(4)],
                },
            ],
            ..Default::default()
        };
        let profile = JointPriceProfile {
            query_range_spans_ms: vec![],
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
    // Tuple correlation forbids mixing the cheap first input from one tuple
    // with the cheap second input of another tuple.
    #[test]
    fn correlated_inputs_cannot_form_an_unpriced_cartesian_combination() {
        let mut consumer = candidate(&[0, 1]);
        consumer
            .supplier_candidates_per_input
            .push(consumer.supplier_candidates_per_input[0].clone());
        let graph = DagOfPhysicalCandidatesWithCosts {
            candidates: vec![candidate(&[]), candidate(&[]), consumer],
            query_roots: vec![QueryOutputCandidates {
                candidates: vec![PhysicalCandidateId(2)],
            }],
            ..Default::default()
        };
        let binding = |id| SupplierPriceBinding {
            producer: PhysicalCandidateId(id),
            price_option: 0,
        };
        let mut reader = option(1.0, false, None);
        reader.compatible_input_tuples =
            vec![vec![binding(0), binding(1)], vec![binding(1), binding(0)]];
        let profile = JointPriceProfile {
            candidates: vec![
                vec![option(0.0, true, None)],
                vec![option(10.0, true, None)],
                vec![reader],
            ],
            schedules: None,
            query_rate: 1.0,
            request_latency_seconds: 0.0,
            request_cost: 0.0,
            query_range_spans_ms: vec![0],
        };
        let solved = solve_joint_price_proposal_with_metrics(
            &graph,
            &[],
            None,
            &mut Default::default(),
            &profile,
        )
        .unwrap()
        .unwrap();
        assert!((solved.direct_modeled_cost.unwrap() - 11.0).abs() < 1e-9);
        assert_eq!(solved.selected.connections.len(), 2);
        assert_ne!(
            solved.selected.connections[0].producer,
            solved.selected.connections[1].producer
        );
    }
}
