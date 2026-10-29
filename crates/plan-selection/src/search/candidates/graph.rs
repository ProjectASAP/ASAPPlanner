//! Candidate dependency graph, declared constraints, and structural validation.
use super::suppliers::SupplierCandidates;
use asap_logical_asap_optimization::pass2::memo::MemoGroupId;
use std::collections::HashSet;

/// Index into [`CandidateDependencyGraph::candidates`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct PhysicalCandidateId(pub usize);

/// One required input, distinct even when another input accepts the same producers.
#[derive(Debug, Clone)]
pub(crate) struct ProducerRequirement {
    pub compatible_producers: SupplierCandidates,
}

/// One executable choice with costs already expressed over the same fixed horizon.
#[derive(Debug, Clone)]
pub(crate) struct PhysicalCandidate {
    /// Logical ownership does not impose one selected candidate per group.
    pub group: MemoGroupId,
    /// Local work and state only; producer costs must not appear here again.
    pub local_cost: f64,
    pub producer_requirements: Vec<ProducerRequirement>,
}

/// Candidates allowed to provide one required query output.
#[derive(Debug, Clone)]
pub(crate) struct QueryRootCandidates {
    pub compatible_candidates: Vec<PhysicalCandidateId>,
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

/// Legality conditioned on selected downstream outputs, not potential consumers.
/// References index `query_roots`; multiple references of one query stay distinct.
#[derive(Debug, Clone)]
pub(crate) enum ReaderConstraint {
    /// Symmetry only: interchangeable execution-scope copies activate in order.
    /// Both copies still need genuine consumers; this does not create a DAG edge.
    ActivationRequires {
        candidate: PhysicalCandidateId,
        required_candidate: PhysicalCandidateId,
    },
    /// Each bucket is one query's output references; together they partition
    /// every output exactly once. Empty buckets represent literal-only queries.
    MinimumQueryReaders {
        candidate: PhysicalCandidateId,
        query_outputs: Vec<Vec<usize>>,
        minimum: usize,
    },
    /// A non-summary query materialization must be a root or have an
    /// immediate selected consumer reached by at most one query (Stage 2).
    HighestSharedQueryMaterialization {
        candidate: PhysicalCandidateId,
        query_outputs: Vec<Vec<usize>>,
    },
    AllowedOutputs {
        candidate: PhysicalCandidateId,
        outputs: Vec<usize>,
    },
    IncompatibleOutputs {
        candidate: PhysicalCandidateId,
        left: usize,
        right: usize,
    },
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CandidateDependencyGraph {
    pub candidates: Vec<PhysicalCandidate>,
    pub query_roots: Vec<QueryRootCandidates>,
    pub resource_budgets: Vec<ResourceBudget>,
    pub reader_constraints: Vec<ReaderConstraint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProducerConsumerConnection {
    pub producer: PhysicalCandidateId,
    pub consumer: PhysicalCandidateId,
    /// Index in the consumer's `producer_requirements`.
    pub requirement: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QueryRootAssignment {
    /// Index in the graph's `query_roots`.
    pub query_root: usize,
    pub candidate: PhysicalCandidateId,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SelectedCandidateGraph {
    pub selected_candidates: Vec<PhysicalCandidateId>,
    pub connections: Vec<ProducerConsumerConnection>,
    pub query_roots: Vec<QueryRootAssignment>,
    pub modeled_cost: f64,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CandidateSelectionError {
    #[error("direct cost model is not representable: {0}")]
    UnsupportedDirectCost(String),
    #[error("invalid candidate dependency graph: {0}")]
    InvalidGraph(String),
    #[error("candidate selection solver: {0}")]
    Solver(String),
}

// Positive costs must fit within the supported ratio before scaling the
// smallest to unit magnitude; rejecting a wider range is an
// explicit model restriction, not proof of infeasibility or a completeness claim.
// Resource rows use the same conservative positive coefficient range.
pub(crate) const MINIMUM_RELATIVE_COEFFICIENT: f64 = 1e-9;

impl CandidateDependencyGraph {
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
            for (requirement, input) in candidate.producer_requirements.iter().enumerate() {
                validate_ids(
                    &input.compatible_producers,
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
            validate_ids(&root.compatible_candidates, &format!("query root {index}"))?;
        }
        Ok(())
    }
}

/// A certified size-independent failure of a query's dependency closure.
/// The evaluator certifies semantics; this module verifies the structural witness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RejectedQuerySubgraph {
    pub query_roots: Vec<QueryRootAssignment>,
    pub connections: Vec<ProducerConsumerConnection>,
    /// Producers the witness stops at, with no edge into them: the evaluator
    /// certified the failure holds whatever feeds those inputs.
    pub stopped_at: Vec<PhysicalCandidateId>,
}

impl RejectedQuerySubgraph {
    /// The complete selected upstream closure of `outputs`.
    #[cfg(test)]
    pub fn from_query_outputs(
        selected: &SelectedCandidateGraph,
        outputs: &[usize],
    ) -> Result<Self, String> {
        Self::from_query_outputs_above(selected, outputs, |_| false)
    }

    /// The selected upstream closure of `outputs`, without the edges into a
    /// producer `stop` selects or anything below it.
    pub fn from_query_outputs_above(
        selected: &SelectedCandidateGraph,
        outputs: &[usize],
        stop: impl Fn(PhysicalCandidateId) -> bool,
    ) -> Result<Self, String> {
        if outputs.is_empty()
            || outputs
                .iter()
                .enumerate()
                .any(|(i, id)| outputs[..i].contains(id))
        {
            return Err("query cut requires distinct, nonempty output references".into());
        }
        let query_roots = outputs
            .iter()
            .map(|output| {
                selected
                    .query_roots
                    .iter()
                    .find(|root| root.query_root == *output)
                    .copied()
                    .ok_or_else(|| format!("query cut references missing output {output}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut pending: Vec<_> = query_roots.iter().map(|root| root.candidate).collect();
        let mut visited = Vec::new();
        let mut connections = Vec::new();
        let mut stopped_at = Vec::new();
        while let Some(candidate) = pending.pop() {
            if visited.contains(&candidate) {
                continue;
            }
            visited.push(candidate);
            for edge in selected
                .connections
                .iter()
                .filter(|edge| edge.consumer == candidate)
            {
                if stop(edge.producer) {
                    if !stopped_at.contains(&edge.producer) {
                        stopped_at.push(edge.producer);
                    }
                    continue;
                }
                connections.push(*edge);
                pending.push(edge.producer);
            }
        }
        Ok(Self {
            query_roots,
            connections,
            stopped_at,
        })
    }

    pub(crate) fn validate(&self, selected: &SelectedCandidateGraph) -> Result<(), String> {
        let outputs: Vec<_> = self
            .query_roots
            .iter()
            .map(|root| root.query_root)
            .collect();
        let expected = Self::from_query_outputs_above(selected, &outputs, |producer| {
            self.stopped_at.contains(&producer)
        })?;
        if self.query_roots != expected.query_roots
            || !same_set(&self.connections, &expected.connections)
            || !same_set(&self.stopped_at, &expected.stopped_at)
        {
            return Err(
                "query cut must contain the selected upstream closure above its stopped producers"
                    .into(),
            );
        }
        Ok(())
    }
}

/// Whether `a` has no repeats and the same items as `b`.
fn same_set<T: PartialEq>(a: &[T], b: &[T]) -> bool {
    a.len() == b.len()
        && a.iter()
            .enumerate()
            .all(|(i, item)| !a[..i].contains(item) && b.contains(item))
}

/// Check the decoded integral graph instead of trusting rounded solver values.
pub(crate) fn validate_selected_graph(
    graph: &CandidateDependencyGraph,
    selected: &SelectedCandidateGraph,
) -> Result<(), String> {
    let invalid =
        || "decoded candidate selection violates dependency or root constraints".to_string();
    let active: HashSet<_> = selected.selected_candidates.iter().copied().collect();
    if active.len() != selected.selected_candidates.len()
        || active.iter().any(|c| c.0 >= graph.candidates.len())
        || selected.query_roots.len() != graph.query_roots.len()
    {
        return Err(invalid());
    }
    for budget in &graph.resource_budgets {
        let usage: f64 = active.iter().map(|c| budget.candidate_usage[c.0]).sum();
        if usage > budget.capacity {
            return Err(format!(
                "decoded selection exceeds resource budget {:?}",
                budget.name
            ));
        }
    }
    let mut root_seen = vec![false; graph.query_roots.len()];
    for root in &selected.query_roots {
        let Some(allowed) = graph.query_roots.get(root.query_root) else {
            return Err(invalid());
        };
        if root_seen[root.query_root]
            || !active.contains(&root.candidate)
            || !allowed.compatible_candidates.contains(&root.candidate)
        {
            return Err(invalid());
        }
        root_seen[root.query_root] = true;
    }
    let mut producers: Vec<Vec<Option<PhysicalCandidateId>>> = graph
        .candidates
        .iter()
        .map(|c| vec![None; c.producer_requirements.len()])
        .collect();
    for connection in &selected.connections {
        if !active.contains(&connection.producer) || !active.contains(&connection.consumer) {
            return Err(invalid());
        }
        let Some(requirement) = graph.candidates[connection.consumer.0]
            .producer_requirements
            .get(connection.requirement)
        else {
            return Err(invalid());
        };
        let target = &mut producers[connection.consumer.0][connection.requirement];
        if target.is_some()
            || !requirement
                .compatible_producers
                .contains(&connection.producer)
        {
            return Err(invalid());
        }
        *target = Some(connection.producer);
    }
    if active
        .iter()
        .any(|c| producers[c.0].iter().any(Option::is_none))
    {
        return Err(invalid());
    }
    // Kahn's algorithm checks cycles without recursion on a potentially deep DAG.
    let mut outstanding: Vec<_> = producers.iter().map(Vec::len).collect();
    let mut consumers = vec![Vec::new(); graph.candidates.len()];
    for connection in &selected.connections {
        consumers[connection.producer.0].push(connection.consumer.0);
    }
    let mut ready: Vec<_> = active
        .iter()
        .filter(|c| outstanding[c.0] == 0)
        .map(|c| c.0)
        .collect();
    let mut visited = 0;
    while let Some(producer) = ready.pop() {
        visited += 1;
        for &consumer in &consumers[producer] {
            outstanding[consumer] -= 1;
            if outstanding[consumer] == 0 {
                ready.push(consumer);
            }
        }
    }
    if visited != active.len() {
        return Err(invalid());
    }
    let mut reachable = HashSet::new();
    let mut pending: Vec<_> = selected
        .query_roots
        .iter()
        .map(|root| root.candidate)
        .collect();
    while let Some(candidate) = pending.pop() {
        if reachable.insert(candidate) {
            pending.extend(producers[candidate.0].iter().flatten().copied());
        }
    }
    // Reuse one traversal per output across all reader-compatibility checks.
    let mut output_reach = Vec::new();
    if !graph.reader_constraints.is_empty() {
        output_reach = vec![HashSet::new(); graph.query_roots.len()];
        for root in &selected.query_roots {
            let reached = &mut output_reach[root.query_root];
            let mut pending = vec![root.candidate];
            while let Some(candidate) = pending.pop() {
                if reached.insert(candidate) {
                    pending.extend(producers[candidate.0].iter().flatten().copied());
                }
            }
        }
    }
    let output_reaches =
        |output: usize, target: PhysicalCandidateId| output_reach[output].contains(&target);
    for requirement in &graph.reader_constraints {
        let violated = match requirement {
            ReaderConstraint::ActivationRequires {
                candidate,
                required_candidate,
            } => active.contains(candidate) && !active.contains(required_candidate),
            ReaderConstraint::MinimumQueryReaders {
                candidate,
                query_outputs,
                minimum,
            } => {
                active.contains(candidate)
                    && query_outputs
                        .iter()
                        .filter(|outputs| {
                            outputs
                                .iter()
                                .any(|&output| output_reaches(output, *candidate))
                        })
                        .count()
                        < *minimum
            }
            ReaderConstraint::HighestSharedQueryMaterialization {
                candidate,
                query_outputs,
            } => {
                active.contains(candidate)
                    && !selected
                        .query_roots
                        .iter()
                        .any(|root| root.candidate == *candidate)
                    && !selected.connections.iter().any(|edge| {
                        edge.producer == *candidate
                            && query_outputs
                                .iter()
                                .filter(|outputs| {
                                    outputs
                                        .iter()
                                        .any(|output| output_reaches(*output, edge.consumer))
                                })
                                .count()
                                <= 1
                    })
            }
            ReaderConstraint::AllowedOutputs { candidate, outputs } => (0..graph.query_roots.len())
                .any(|output| !outputs.contains(&output) && output_reaches(output, *candidate)),
            ReaderConstraint::IncompatibleOutputs {
                candidate,
                left,
                right,
            } => output_reaches(*left, *candidate) && output_reaches(*right, *candidate),
        };
        if violated {
            return Err("decoded selection violates selected-reader compatibility".into());
        }
    }
    if reachable != active {
        return Err(invalid());
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
