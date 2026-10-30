//! Stage 3's DAG of Stage 2's physical candidates with their costs: what the
//! MILP selects over. Stage 2's output carries no cost; lower bounds build
//! this from it ([`PhysicalCandidatesWithCosts::new`]) and fill the costs.
use std::collections::HashSet;
use std::rc::Rc;

use asap_types::ir::OperatorNode;
use asap_types::stages::{
    validate_selected_connections, PhysicalASAPMemoCandidates, PhysicalCandidateId,
    ProducerCandidates, ReaderConstraint, SelectedCandidateGraph,
};

/// One required input, distinct even when another input accepts the same
/// producers.
#[derive(Debug, Clone)]
pub(crate) struct InputProducerCandidates {
    /// The candidates that may produce it.
    pub candidates: ProducerCandidates,
}

/// One physical candidate, indexed like
/// [`PhysicalASAPMemoCandidates::candidates`], with its cost over the same
/// fixed horizon.
#[derive(Debug, Clone)]
pub(crate) struct PhysicalCandidateWithCost {
    /// Local work and state only; producer costs must not appear here again.
    pub local_cost: f64,
    /// Per input, the candidates that may produce it (as Stage 2's
    /// `PhysicalCandidate::producer_candidates_per_input`).
    pub producer_candidates_per_input: Vec<InputProducerCandidates>,
}

/// One output reference of the query roots.
#[derive(Debug, Clone)]
pub(crate) struct QueryOutputCandidates {
    /// The candidates allowed to produce it.
    pub candidates: Vec<PhysicalCandidateId>,
}

/// Additive usage over a fixed accounting context, owned once per candidate.
/// Peak temporaries and connection-dependent demand require a different model.
#[derive(Debug, Clone)]
pub(crate) struct ResourceBudget {
    pub name: String,
    pub capacity: f64,
    /// Same index order as the graph's candidates.
    pub candidate_usage: Vec<f64>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DagOfPhysicalCandidatesWithCosts {
    /// By [`PhysicalCandidateId`].
    pub candidates: Vec<PhysicalCandidateWithCost>,
    /// One per output reference of the query roots, in order.
    pub query_roots: Vec<QueryOutputCandidates>,
    /// Resource limits the selection must respect.
    pub resource_budgets: Vec<ResourceBudget>,
    /// Legality that depends on the selected readers.
    pub reader_constraints: Vec<ReaderConstraint>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CandidateSelectionError {
    #[error("direct cost model is not representable: {0}")]
    UnsupportedDirectCost(String),
    #[error("invalid DAG of physical candidates with costs: {0}")]
    InvalidGraph(String),
    #[error("candidate selection solver: {0}")]
    Solver(String),
}

// Positive costs must fit within the supported ratio before scaling the
// smallest to unit magnitude; rejecting a wider range is an
// explicit model restriction, not proof of infeasibility or a completeness claim.
// Resource rows use the same conservative positive coefficient range.
pub(crate) const MINIMUM_RELATIVE_COEFFICIENT: f64 = 1e-9;

impl DagOfPhysicalCandidatesWithCosts {
    pub(crate) fn validate(&self) -> Result<(), CandidateSelectionError> {
        let validate_ids = |ids: &[PhysicalCandidateId], context: &str| {
            let mut seen = HashSet::new();
            for id in ids {
                if id.0 >= self.candidates.len() {
                    return Err(CandidateSelectionError::InvalidGraph(format!(
                        "{context} references missing candidate {}",
                        id.0
                    )));
                }
                if !seen.insert(*id) {
                    return Err(CandidateSelectionError::InvalidGraph(format!(
                        "{context} repeats candidate {}",
                        id.0
                    )));
                }
            }
            Ok(())
        };
        for budget in &self.resource_budgets {
            if budget.name.is_empty()
                || !budget.capacity.is_finite()
                || budget.capacity < 0.0
                || budget.candidate_usage.len() != self.candidates.len()
                || budget
                    .candidate_usage
                    .iter()
                    .any(|usage| !usage.is_finite() || *usage < 0.0)
                || !budget.candidate_usage.iter().sum::<f64>().is_finite()
            {
                return Err(CandidateSelectionError::InvalidGraph(format!(
                    "resource budget {:?} needs finite nonnegative capacity and one finite nonnegative usage per candidate", budget.name
                )));
            }
        }
        for budget in &self.resource_budgets {
            let scale = budget
                .candidate_usage
                .iter()
                .copied()
                .fold(budget.capacity, f64::max);
            if scale > 0.0
                && budget
                    .candidate_usage
                    .iter()
                    .chain(std::iter::once(&budget.capacity))
                    .any(|value| *value > 0.0 && *value / scale < MINIMUM_RELATIVE_COEFFICIENT)
            {
                return Err(CandidateSelectionError::InvalidGraph(format!(
                    "resource budget {:?} exceeds the supported positive coefficient range ({MINIMUM_RELATIVE_COEFFICIENT}); restrict or reformulate the resource model", budget.name
                )));
            }
        }
        for requirement in &self.reader_constraints {
            let (candidate, outputs) = match requirement {
                ReaderConstraint::ActivationRequires {
                    candidate,
                    required_candidate,
                } => {
                    if required_candidate.0 >= self.candidates.len()
                        || candidate == required_candidate
                    {
                        return Err(CandidateSelectionError::InvalidGraph(
                            "activation constraint references missing or identical candidate"
                                .into(),
                        ));
                    }
                    (*candidate, Vec::new())
                }
                ReaderConstraint::MinimumQueryReaders {
                    candidate,
                    query_outputs,
                    ..
                }
                | ReaderConstraint::HighestSharedQueryMaterialization {
                    candidate,
                    query_outputs,
                } => {
                    let outputs: Vec<_> = query_outputs.iter().flatten().copied().collect();
                    let unique: HashSet<_> = outputs.iter().copied().collect();
                    if outputs.len() != self.query_roots.len() || unique.len() != outputs.len() {
                        return Err(CandidateSelectionError::InvalidGraph(
                            "query reader buckets must partition every output exactly once".into(),
                        ));
                    }
                    (*candidate, outputs)
                }
                ReaderConstraint::AllowedOutputs { candidate, outputs } => {
                    (*candidate, outputs.clone())
                }
                ReaderConstraint::IncompatibleOutputs {
                    candidate,
                    left,
                    right,
                } => (*candidate, vec![*left, *right]),
            };
            if candidate.0 >= self.candidates.len()
                || outputs
                    .iter()
                    .any(|&output| output >= self.query_roots.len())
            {
                return Err(CandidateSelectionError::InvalidGraph(
                    "reader constraint references missing candidate or output".into(),
                ));
            }
        }
        let mut total_cost = 0.0;
        for (index, candidate) in self.candidates.iter().enumerate() {
            if !candidate.local_cost.is_finite() || candidate.local_cost < 0.0 {
                return Err(CandidateSelectionError::InvalidGraph(format!(
                    "candidate {index} needs a finite nonnegative local cost"
                )));
            }
            total_cost += candidate.local_cost;
            for (requirement, input) in candidate.producer_candidates_per_input.iter().enumerate() {
                validate_ids(
                    &input.candidates,
                    &format!("candidate {index}, requirement {requirement}"),
                )?;
            }
        }
        if !total_cost.is_finite() {
            return Err(CandidateSelectionError::InvalidGraph(
                "total candidate cost overflows".into(),
            ));
        }
        let maximum_cost = self
            .candidates
            .iter()
            .map(|c| c.local_cost)
            .fold(0.0, f64::max);
        if maximum_cost > 0.0
            && self.candidates.iter().any(|candidate| {
                candidate.local_cost > 0.0
                    && candidate.local_cost / maximum_cost < MINIMUM_RELATIVE_COEFFICIENT
            })
        {
            return Err(CandidateSelectionError::InvalidGraph(format!(
                "positive local costs must be at least {MINIMUM_RELATIVE_COEFFICIENT} of the largest cost; wider ranges require a different numerical policy"
            )));
        }
        for (index, root) in self.query_roots.iter().enumerate() {
            validate_ids(&root.candidates, &format!("query root {index}"))?;
        }
        Ok(())
    }
}

/// Check the decoded integral graph instead of trusting rounded solver values:
/// its structure ([`validate_selected_connections`]), resource budgets and
/// modeled cost.
pub(crate) fn validate_selected_graph(
    graph: &DagOfPhysicalCandidatesWithCosts,
    selected: &SelectedCandidateGraph,
) -> Result<(), String> {
    let query_output_candidates: Vec<&[PhysicalCandidateId]> = graph
        .query_roots
        .iter()
        .map(|root| root.candidates.as_slice())
        .collect();
    validate_selected_connections(
        graph.candidates.len(),
        |candidate| {
            graph.candidates[candidate.0]
                .producer_candidates_per_input
                .len()
        },
        |candidate, input| {
            graph.candidates[candidate.0]
                .producer_candidates_per_input
                .get(input)
                .map(|requirement| &requirement.candidates)
        },
        &query_output_candidates,
        &graph.reader_constraints,
        selected,
    )?;
    let active: HashSet<_> = selected.selected_candidates.iter().copied().collect();
    for budget in &graph.resource_budgets {
        let usage: f64 = active.iter().map(|c| budget.candidate_usage[c.0]).sum();
        if usage > budget.capacity {
            return Err(format!(
                "decoded selection exceeds resource budget {:?}",
                budget.name
            ));
        }
    }
    let expected_cost: f64 = active
        .iter()
        .map(|c| graph.candidates[c.0].local_cost)
        .sum();
    if !selected.modeled_cost.is_finite()
        || (selected.modeled_cost - expected_cost).abs() > 1e-9 * expected_cost.abs()
    {
        return Err("decoded candidate cost disagrees with selected candidates".into());
    }
    Ok(())
}

impl DagOfPhysicalCandidatesWithCosts {
    /// `physical`'s candidates, producers, query outputs and reader
    /// constraints, at zero cost and with no resource budgets.
    pub(crate) fn unpriced(physical: &PhysicalASAPMemoCandidates) -> Self {
        Self {
            candidates: physical
                .candidates
                .iter()
                .map(|candidate| PhysicalCandidateWithCost {
                    local_cost: 0.0,
                    producer_candidates_per_input: candidate
                        .producer_candidates_per_input
                        .iter()
                        .map(|suppliers| InputProducerCandidates {
                            candidates: suppliers.clone(),
                        })
                        .collect(),
                })
                .collect(),
            query_roots: (0..physical.output_reference_count())
                .map(|output| QueryOutputCandidates {
                    candidates: physical.query_output_candidates(output).collect(),
                })
                .collect(),
            resource_budgets: Vec::new(),
            reader_constraints: physical.reader_constraints.clone(),
        }
    }
}

/// Stage 3's search input: Stage 2's candidates, and their DAG with costs,
/// indexed alike by [`PhysicalCandidateId`].
#[derive(Debug, Clone)]
pub(crate) struct PhysicalCandidatesWithCosts {
    /// Stage 2's output: each candidate's operator node, timing and
    /// materialization.
    pub physical: PhysicalASAPMemoCandidates,
    /// What the MILP selects over: the same candidates with their costs.
    pub dag: DagOfPhysicalCandidatesWithCosts,
}

impl PhysicalCandidatesWithCosts {
    /// `physical` with an unpriced DAG ([`DagOfPhysicalCandidatesWithCosts::unpriced`]).
    pub(crate) fn new(physical: PhysicalASAPMemoCandidates) -> Self {
        let dag = DagOfPhysicalCandidatesWithCosts::unpriced(&physical);
        Self { physical, dag }
    }

    /// `selected` against the DAG: its structure, resource budgets and cost
    /// ([`validate_selected_graph`]), which Stage 2's reconstruction no
    /// longer checks.
    pub(crate) fn validate_selected(
        &self,
        selected: &SelectedCandidateGraph,
    ) -> Result<(), String> {
        self.dag.validate().map_err(|error| error.to_string())?;
        validate_selected_graph(&self.dag, selected)
    }

    /// The operator node candidate `id` runs.
    pub(crate) fn node(&self, id: usize) -> &Rc<OperatorNode> {
        self.physical.physical_node(PhysicalCandidateId(id))
    }
}

#[cfg(test)]
impl PhysicalCandidatesWithCosts {
    /// No candidates; tests append them with [`Self::push_for_tests`].
    pub(crate) fn empty_for_tests() -> Self {
        Self::from_dag_for_tests(DagOfPhysicalCandidatesWithCosts::default(), Vec::new())
    }

    /// `dag` with candidate `i` running `nodes[i].0` at timing and
    /// materialization `nodes[i].1`, each on its own operator node `i`
    /// ([`Self::sync_physical_for_tests`]).
    pub(crate) fn from_dag_for_tests(
        dag: DagOfPhysicalCandidatesWithCosts,
        nodes: Vec<(
            Rc<OperatorNode>,
            (
                asap_types::ir::properties::ExecutionTiming,
                asap_types::physical::Materialization,
            ),
        )>,
    ) -> Self {
        assert_eq!(dag.candidates.len(), nodes.len());
        let candidates = nodes
            .iter()
            .enumerate()
            .map(
                |(id, (_, (timing, materialization)))| asap_types::stages::PhysicalCandidate {
                    operator_node: asap_types::stages::OperatorNodeOfCandidateForMemoGroupId(id),
                    timing: *timing,
                    materialization: *materialization,
                    producer_candidates_per_input: Vec::new(),
                    may_produce_query_output: false,
                    may_keep_across_evaluations_per_sole_reader_query: Vec::new(),
                },
            )
            .collect();
        let operator_nodes = nodes
            .into_iter()
            .enumerate()
            .map(
                |(id, (node, _))| asap_types::stages::OperatorNodeOfCandidateForMemoGroup {
                    candidates_for_memo_group: vec![asap_types::stages::CandidateForMemoGroupId {
                        group: asap_types::stages::MemoGroupId(0),
                        index: id,
                    }],
                    is_implementation_root: false,
                    physical_node: node,
                },
            )
            .collect();
        let mut configured = Self {
            physical: PhysicalASAPMemoCandidates::new(
                operator_nodes,
                candidates,
                Vec::new(),
                Vec::new(),
            ),
            dag,
        };
        configured.sync_physical_for_tests();
        configured
    }

    /// Appends a candidate running `node` at `timing` and `materialization`
    /// on its own operator node, at `local_cost`, reading `inputs`; returns
    /// its ID. Call [`Self::sync_physical_for_tests`] once the DAG is complete.
    pub(crate) fn push_for_tests(
        &mut self,
        node: Rc<OperatorNode>,
        timing: asap_types::ir::properties::ExecutionTiming,
        materialization: asap_types::physical::Materialization,
        local_cost: f64,
        inputs: Vec<InputProducerCandidates>,
    ) -> PhysicalCandidateId {
        let id = PhysicalCandidateId(self.physical.candidates.len());
        let operator_node = asap_types::stages::OperatorNodeOfCandidateForMemoGroupId(
            self.physical
                .operator_nodes_of_candidates_for_memo_groups
                .len(),
        );
        self.physical
            .operator_nodes_of_candidates_for_memo_groups
            .push(asap_types::stages::OperatorNodeOfCandidateForMemoGroup {
                candidates_for_memo_group: vec![asap_types::stages::CandidateForMemoGroupId {
                    group: asap_types::stages::MemoGroupId(0),
                    index: operator_node.0,
                }],
                is_implementation_root: false,
                physical_node: node,
            });
        self.physical
            .candidates
            .push(asap_types::stages::PhysicalCandidate {
                operator_node,
                timing,
                materialization,
                producer_candidates_per_input: Vec::new(),
                may_produce_query_output: false,
                may_keep_across_evaluations_per_sole_reader_query: Vec::new(),
            });
        self.dag.candidates.push(PhysicalCandidateWithCost {
            local_cost,
            producer_candidates_per_input: inputs,
        });
        self.sync_physical_for_tests();
        id
    }

    /// Makes `physical` agree with a DAG a test built or edited directly:
    /// producers and reader constraints are the DAG's, and the candidates
    /// each distinct DAG query-root list names become the implementation
    /// roots, with `may_produce_query_output`, of one group (1, 2, … in
    /// order of first appearance) that output references; other candidates'
    /// nodes are private nodes of group 0. Each candidate keeps its operator
    /// node, timing and materialization.
    pub(crate) fn sync_physical_for_tests(&mut self) {
        use asap_types::stages::{CandidateForMemoGroupId, MemoGroupId};
        let mut lists: Vec<Vec<PhysicalCandidateId>> = Vec::new();
        let mut query_roots = Vec::new();
        for root in &self.dag.query_roots {
            let mut list = root.candidates.clone();
            list.sort();
            let group = match lists.iter().position(|known| *known == list) {
                Some(position) => position + 1,
                None => {
                    lists.push(list);
                    lists.len()
                }
            };
            query_roots.push(asap_types::ir::QueryRoot::Operator(MemoGroupId(
                group as u32,
            )));
        }
        let mut group_of = vec![None; self.physical.candidates.len()];
        for (position, list) in lists.iter().enumerate() {
            for id in list {
                assert!(
                    group_of[id.0]
                        .replace(position + 1)
                        .is_none_or(|group| group == position + 1),
                    "test DAG gives candidate {} two different query-root lists",
                    id.0
                );
            }
        }
        for (id, candidate) in self.physical.candidates.iter_mut().enumerate() {
            candidate.producer_candidates_per_input = self.dag.candidates[id]
                .producer_candidates_per_input
                .iter()
                .map(|input| input.candidates.clone())
                .collect();
            candidate.may_produce_query_output = group_of[id].is_some();
            let node = &mut self.physical.operator_nodes_of_candidates_for_memo_groups
                [candidate.operator_node.0];
            node.is_implementation_root = group_of[id].is_some();
            node.candidates_for_memo_group = vec![CandidateForMemoGroupId {
                group: MemoGroupId(group_of[id].unwrap_or(0) as u32),
                index: candidate.operator_node.0,
            }];
        }
        let physical = std::mem::replace(
            &mut self.physical,
            PhysicalASAPMemoCandidates::new(Vec::new(), Vec::new(), Vec::new(), Vec::new()),
        );
        self.physical = PhysicalASAPMemoCandidates::new(
            physical.operator_nodes_of_candidates_for_memo_groups,
            physical.candidates,
            query_roots,
            self.dag.reader_constraints.clone(),
        );
    }

    /// Stage 2's structural pruning of `physical`, with the DAG restricted to
    /// the kept candidates at their costs and resource usage. Each candidate
    /// must run its own operator node (as the test constructors make them),
    /// which then names its index before pruning.
    pub(crate) fn prune_for_tests(&mut self) -> asap_types::stages::PruningMetrics {
        let physical = std::mem::replace(
            &mut self.physical,
            PhysicalASAPMemoCandidates::new(Vec::new(), Vec::new(), Vec::new(), Vec::new()),
        );
        self.physical =
            asap_physical_asap_optimization::catalog::test_support::prune(physical).unwrap();
        let kept: Vec<_> = self
            .physical
            .candidates
            .iter()
            .map(|candidate| candidate.operator_node.0)
            .collect();
        let mut dag = DagOfPhysicalCandidatesWithCosts::unpriced(&self.physical);
        for (candidate, &original) in dag.candidates.iter_mut().zip(&kept) {
            candidate.local_cost = self.dag.candidates[original].local_cost;
        }
        dag.resource_budgets = self
            .dag
            .resource_budgets
            .iter()
            .map(|budget| ResourceBudget {
                candidate_usage: kept.iter().map(|&id| budget.candidate_usage[id]).collect(),
                ..budget.clone()
            })
            .collect();
        self.dag = dag;
        self.physical.pruning_metrics
    }
}

/// An operator node for tests whose candidates' operators do not matter.
#[cfg(test)]
pub(crate) fn placeholder_node_for_tests() -> Rc<OperatorNode> {
    use asap_types::ir::scalar::expr_ir::ScalarValue;
    use asap_types::ir::schema::{DataType, Field, Schema};
    use asap_types::ir::{NonASAPOp, Operator, ScalarExpr};
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Values {
        rows: vec![vec![ScalarExpr::Literal(ScalarValue::Float64(1.0))]],
        schema: Schema::lifted(vec![Field::plain("value", DataType::Float64, false)], None),
    }))
    .unwrap()
}

#[cfg(test)]
impl PhysicalCandidatesWithCosts {
    /// Each candidate's operator node, by candidate ID.
    pub(crate) fn nodes_for_tests(&self) -> Vec<Rc<OperatorNode>> {
        (0..self.physical.candidates.len())
            .map(|id| Rc::clone(self.node(id)))
            .collect()
    }

    /// Makes candidate `id` alone run `node`: it gets its own copy of its
    /// operator node (same group and root flag) with `node` as its physical
    /// node; other candidates of its old operator node are unchanged.
    pub(crate) fn set_node_for_tests(&mut self, id: usize, node: Rc<OperatorNode>) {
        let nodes = &mut self.physical.operator_nodes_of_candidates_for_memo_groups;
        let mut own = nodes[self.physical.candidates[id].operator_node.0].clone();
        own.physical_node = node;
        nodes.push(own);
        self.physical.candidates[id].operator_node =
            asap_types::stages::OperatorNodeOfCandidateForMemoGroupId(nodes.len() - 1);
    }
}

#[cfg(test)]
impl PhysicalCandidatesWithCosts {
    /// Appends a candidate running `node` as another implementation root of
    /// candidate `like`'s group, with `like`'s timing, materialization and
    /// cost, reading `inputs`; returns its ID.
    pub(crate) fn push_root_like_for_tests(
        &mut self,
        node: Rc<OperatorNode>,
        like: PhysicalCandidateId,
        inputs: Vec<InputProducerCandidates>,
    ) -> PhysicalCandidateId {
        let id = PhysicalCandidateId(self.physical.candidates.len());
        let mut operator_nodes = self
            .physical
            .operator_nodes_of_candidates_for_memo_groups
            .clone();
        let mut root = operator_nodes[self.physical.candidates[like.0].operator_node.0].clone();
        root.is_implementation_root = true;
        root.physical_node = node;
        operator_nodes.push(root);
        let mut candidates = self.physical.candidates.clone();
        let mut candidate = candidates[like.0].clone();
        candidate.operator_node =
            asap_types::stages::OperatorNodeOfCandidateForMemoGroupId(operator_nodes.len() - 1);
        candidate.producer_candidates_per_input = inputs
            .iter()
            .map(|input| input.candidates.clone())
            .collect();
        candidates.push(candidate);
        self.physical = PhysicalASAPMemoCandidates::new(
            operator_nodes,
            candidates,
            self.physical.query_roots.clone(),
            self.physical.reader_constraints.clone(),
        );
        self.dag.candidates.push(PhysicalCandidateWithCost {
            local_cost: self.dag.candidates[like.0].local_cost,
            producer_candidates_per_input: inputs,
        });
        id
    }
}
