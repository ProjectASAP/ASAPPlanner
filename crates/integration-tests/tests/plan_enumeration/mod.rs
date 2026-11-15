//! A test oracle for Stage 3: every plan of a physical memo (Stage 2's
//! output), built and costed without Stage 3's selectable executions, cost
//! coefficients or MILP, each with `asap_cost::plan_cost` and checked
//! against the planner's install rules (`PhysicalASAPDAG::materialized_views`
//! and `PhysicalASAPDAG::raw_data_retention`).
//!
//! A plan is one execution per (execution context, physical candidate) it
//! uses (concepts doc, section 5), each input of an execution read from one
//! producer execution over an admissible physical edge, each query output
//! answered by one execution of its group:
//!
//! - a query output is answered in the query's evaluation context (its grid,
//!   `QueryWorkloadEntry::evaluation_grid_ms`, at phase 0) by a candidate a
//!   query-time consumer may read (`producer_satisfies_physical_requirements`);
//! - a producer is read in a context only where it delivers on it
//!   (`PhysicalASAPMemoCandidates::delivers_on`);
//! - a producer materialized across evaluations runs once, in
//!   `Materialized(v)`; another ingestion-time producer in the one ingestion
//!   context `ExecutionContext::EVERY_ARRIVING_ROW`; any other in its
//!   consumer's context;
//! - the inputs of an ingestion-time candidate materialized across
//!   evaluations run on the ingestion pipeline of its refresh window
//!   (`RefreshWindow::computed_from` over the candidates); of any other
//!   candidate, in its own context;
//! - a query-time result that is not materialized has one reader per
//!   context (`PhysicalProperties::has_one_reader_per_context`);
//! - the plan is acyclic, and a query reads no candidate it must not read
//!   (`LogicalASAPMemoCandidates::query_must_not_read`) anywhere below its
//!   output.
//!
//! Each node retaining rows (`PhysicalASAPDAGNode::retains_rows`) retains
//! the least its plan's readers need: `asap_cost::read_back_ms` over the
//! built plan, each reader's range-evaluation span the longest of the
//! queries reaching it in the plan, and at least the span of a query it
//! answers directly; `None` when read unbounded.
//!
//! Not modeled: a `TimeShift` anchored with `@ end()` or `@ start()` (its
//! inputs run once per request, in a context of its own), and accuracy
//! constraint (K) (Stage 3 does not have it yet). A memo with an anchored
//! shift is refused ([`EnumerationError::AnchoredShift`]).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use asap_cost::CostInputs;
use asap_types::ir::properties::{DataPrimitive, ExecutionDataState, ExecutionTiming};
use asap_types::ir::QueryRoot;
use asap_types::plan::materialized_view::{InstallableDagError, RefreshWindow};
use asap_types::plan::physical_asap_dag::{
    PhysicalASAPDAG, PhysicalASAPDAGNode, PhysicalASAPNodeId,
};
use asap_types::plan::view_coverage;
use asap_types::stages::{
    producer_satisfies_physical_requirements, EvaluationGrid, ExecutionContext, MemoGroupId,
    PhysicalASAPMemoCandidates, PhysicalCandidateId,
};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

/// One execution: a candidate in a context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Execution {
    context: ExecutionContext,
    candidate: PhysicalCandidateId,
}

/// A partial plan while it is enumerated.
#[derive(Debug, Clone, Default)]
struct PartialPlan {
    executions: Vec<Execution>,
    position: HashMap<Execution, usize>,
    /// Per execution, per input, the producer execution chosen.
    producers: Vec<Vec<Option<usize>>>,
    /// Per execution, how many edges and query outputs read it.
    reads: Vec<usize>,
    /// Per query output (`Memo::output_groups` order), the execution
    /// answering it.
    answers: Vec<Option<usize>>,
    /// The choices left: query outputs, then inputs of executions added.
    pending: Vec<Slot>,
}

#[derive(Debug, Clone, Copy)]
enum Slot {
    Output(usize),
    Input { consumer: usize, input: usize },
}

/// One enumerated plan, with its `plan_cost` per second (cost model 1).
#[derive(Debug, Clone)]
pub struct EnumeratedPlan {
    pub plan: PhysicalASAPDAG,
    pub cost_per_second: f64,
    /// The first install rule it fails, `None` when it installs.
    pub install_error: Option<InstallableDagError>,
}

/// Every plan of a physical memo, summarized.
#[derive(Debug, Default)]
pub struct Enumeration {
    /// Complete choices enumerated.
    pub choices: usize,
    /// Of them, plans refused before costing: a cycle, an exact-only read
    /// of a sketch, or an invalid DAG.
    pub refused: usize,
    /// Of them, plans `plan_cost` could not cost, and the first error.
    pub not_costable: usize,
    pub first_cost_error: Option<String>,
    /// The cheapest plan that installs.
    pub cheapest_installable: Option<EnumeratedPlan>,
    /// The cheapest plan, installable or not.
    pub cheapest: Option<EnumeratedPlan>,
    /// How many costed plans install, and how many do not.
    pub installable: usize,
    pub not_installable: usize,
    /// The [`plan_signature`] of every plan built (before costing).
    pub plan_signatures: HashSet<String>,
}

#[derive(Debug, PartialEq)]
pub enum EnumerationError {
    /// More than the cap of complete choices: the oracle cannot run.
    TooManyPlans(usize),
    /// A `TimeShift` anchored with `@ end()` or `@ start()`: not modeled.
    AnchoredShift(PhysicalCandidateId),
}

impl std::fmt::Display for EnumerationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyPlans(cap) => write!(f, "more than {cap} complete choices"),
            Self::AnchoredShift(v) => write!(f, "candidate v{} is an anchored TimeShift", v.0),
        }
    }
}

/// Every plan of `physical` for `entries` (query `q` is `entries[q]`), each
/// costed with `plan_cost` under `cost_inputs`, at most `max_choices`
/// complete choices.
pub fn enumerate_plans(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    cost_inputs: &CostInputs,
    max_choices: usize,
) -> Result<Enumeration, EnumerationError> {
    if let Some(v) = (0..physical.physical_candidates.len())
        .map(PhysicalCandidateId)
        .find(|&v| physical.operator(v).request_anchor().is_some())
    {
        return Err(EnumerationError::AnchoredShift(v));
    }
    let outputs = physical.logical_candidates.memo.output_groups();
    let start = PartialPlan {
        answers: vec![None; outputs.len()],
        pending: (0..outputs.len()).rev().map(Slot::Output).collect(),
        ..PartialPlan::default()
    };
    let mut enumerator = Enumerator {
        physical,
        entries,
        data,
        cost_inputs,
        outputs: &outputs,
        max_choices,
        found: Enumeration::default(),
    };
    enumerator.extend(start)?;
    Ok(enumerator.found)
}

struct Enumerator<'a> {
    physical: &'a PhysicalASAPMemoCandidates,
    entries: &'a [QueryWorkloadEntry],
    data: &'a DataWorkload,
    cost_inputs: &'a CostInputs,
    outputs: &'a [(usize, MemoGroupId)],
    max_choices: usize,
    found: Enumeration,
}

impl Enumerator<'_> {
    fn extend(&mut self, mut partial: PartialPlan) -> Result<(), EnumerationError> {
        let Some(slot) = partial.pending.pop() else {
            self.found.choices += 1;
            if self.found.choices > self.max_choices {
                return Err(EnumerationError::TooManyPlans(self.max_choices));
            }
            self.complete(&partial);
            return Ok(());
        };
        for (context, candidate) in self.options(&partial, slot) {
            let mut next = partial.clone();
            let execution = Execution { context, candidate };
            let index = match next.position.get(&execution) {
                Some(&index) => {
                    let one_reader = self.physical.physical_candidates[candidate.0]
                        .physical_properties
                        .has_one_reader_per_context();
                    if one_reader && next.reads[index] >= 1 {
                        continue;
                    }
                    index
                }
                None => {
                    let index = next.executions.len();
                    next.executions.push(execution);
                    next.position.insert(execution, index);
                    let inputs = self.physical.input_count(candidate);
                    next.producers.push(vec![None; inputs]);
                    next.reads.push(0);
                    for input in (0..inputs).rev() {
                        next.pending.push(Slot::Input {
                            consumer: index,
                            input,
                        });
                    }
                    index
                }
            };
            next.reads[index] += 1;
            match slot {
                Slot::Output(output) => next.answers[output] = Some(index),
                Slot::Input { consumer, input } => next.producers[consumer][input] = Some(index),
            }
            self.extend(next)?;
        }
        Ok(())
    }

    /// The executions (context, candidate) that may fill `slot`.
    fn options(
        &self,
        partial: &PartialPlan,
        slot: Slot,
    ) -> Vec<(ExecutionContext, PhysicalCandidateId)> {
        let physical = self.physical;
        match slot {
            Slot::Output(output) => {
                let (query, group) = self.outputs[output];
                let context = ExecutionContext::Evaluation {
                    grid: EvaluationGrid {
                        period_ms: self.entries[query].evaluation_grid_ms(),
                        phase_ms: 0,
                    },
                };
                (0..physical.physical_candidates.len())
                    .map(PhysicalCandidateId)
                    .filter(|&v| {
                        let candidate = &physical.physical_candidates[v.0];
                        candidate.logical_candidate_id.group == group
                            && producer_satisfies_physical_requirements(
                                candidate,
                                ExecutionTiming::QueryTime,
                            )
                    })
                    .filter_map(|v| self.execution_reading(context, v))
                    .collect()
            }
            Slot::Input { consumer, input } => {
                let consumer = partial.executions[consumer];
                let context = self.input_context(consumer);
                physical
                    .admissible_edges_into(consumer.candidate, input)
                    .iter()
                    .map(|&edge| physical.edge(edge).producer_candidate_id)
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .filter_map(|u| self.execution_reading(context, u))
                    .collect()
            }
        }
    }

    /// The execution of `u` a consumer whose inputs run in `context` reads,
    /// when `u` delivers on it.
    fn execution_reading(
        &self,
        context: ExecutionContext,
        u: PhysicalCandidateId,
    ) -> Option<(ExecutionContext, PhysicalCandidateId)> {
        if !self.physical.delivers_on(u, &context) {
            return None;
        }
        let properties = self.physical.physical_candidates[u.0].physical_properties;
        let context = if properties.is_across_evaluations() {
            ExecutionContext::Materialized(u)
        } else if properties.timing() == ExecutionTiming::IngestionTime {
            ExecutionContext::EVERY_ARRIVING_ROW
        } else {
            context
        };
        Some((context, u))
    }

    /// The context the inputs of `consumer` run in.
    fn input_context(&self, consumer: Execution) -> ExecutionContext {
        let properties =
            self.physical.physical_candidates[consumer.candidate.0].physical_properties;
        if properties.timing() == ExecutionTiming::IngestionTime
            && properties.is_across_evaluations()
        {
            if let Some(window) = RefreshWindow::computed_from(self.physical, consumer.candidate) {
                return ExecutionContext::IngestionPipeline(window);
            }
        }
        consumer.context
    }

    /// Builds, checks and costs one complete choice.
    fn complete(&mut self, partial: &PartialPlan) {
        let Some(plan) = self.build(partial) else {
            self.found.refused += 1;
            return;
        };
        self.found.plan_signatures.insert(plan_signature(&plan));
        let cost_per_second =
            match asap_cost::plan_cost(&plan, self.entries, self.data, self.cost_inputs) {
                Ok((cost, _)) => cost.cost_per_second.expect("cost model 1"),
                Err(error) => {
                    self.found.not_costable += 1;
                    self.found
                        .first_cost_error
                        .get_or_insert_with(|| format!("{error:?}"));
                    return;
                }
            };
        let install_error = plan
            .materialized_views()
            .err()
            .or_else(|| plan.raw_data_retention().err());
        let enumerated = EnumeratedPlan {
            plan,
            cost_per_second,
            install_error,
        };
        let cheaper = |best: &Option<EnumeratedPlan>| {
            best.as_ref()
                .is_none_or(|best| enumerated.cost_per_second < best.cost_per_second)
        };
        if enumerated.install_error.is_none() {
            self.found.installable += 1;
            if cheaper(&self.found.cheapest_installable) {
                self.found.cheapest_installable = Some(enumerated.clone());
            }
        } else {
            self.found.not_installable += 1;
        }
        if cheaper(&self.found.cheapest) {
            self.found.cheapest = Some(enumerated);
        }
    }

    /// The plan of a complete choice, `None` when it has a cycle, a query
    /// reads a candidate it must not read, or the DAG is invalid.
    fn build(&self, partial: &PartialPlan) -> Option<PhysicalASAPDAG> {
        let physical = self.physical;
        let count = partial.executions.len();
        let producers = |e: usize| partial.producers[e].iter().map(|p| p.expect("complete"));
        // (X): no query reads, below its output, a candidate it must not.
        for (output, &(query, _)) in self.outputs.iter().enumerate() {
            let mut seen = vec![false; count];
            let mut stack = vec![partial.answers[output].expect("complete")];
            while let Some(e) = stack.pop() {
                if std::mem::replace(&mut seen[e], true) {
                    continue;
                }
                let logical = physical.physical_candidates[partial.executions[e].candidate.0]
                    .logical_candidate_id;
                if physical
                    .logical_candidates
                    .query_must_not_read(query, logical)
                {
                    return None;
                }
                stack.extend(producers(e));
            }
        }
        // Producers first (Kahn's order); a cycle leaves executions out.
        let mut waiting: Vec<usize> = (0..count).map(|e| partial.producers[e].len()).collect();
        let mut consumers: Vec<Vec<usize>> = vec![Vec::new(); count];
        for e in 0..count {
            for p in producers(e) {
                consumers[p].push(e);
            }
        }
        let mut ready: Vec<usize> = (0..count).rev().filter(|&e| waiting[e] == 0).collect();
        let mut node_of = vec![usize::MAX; count];
        let mut order = Vec::with_capacity(count);
        while let Some(e) = ready.pop() {
            node_of[e] = order.len();
            order.push(e);
            for &c in &consumers[e] {
                waiting[c] -= 1;
                if waiting[c] == 0 {
                    ready.push(c);
                }
            }
        }
        if order.len() != count {
            return None;
        }
        let nodes: Vec<PhysicalASAPDAGNode> = order
            .iter()
            .enumerate()
            .map(|(id, &e)| {
                let inputs: Vec<PhysicalASAPNodeId> = producers(e).map(|p| node_of[p]).collect();
                plan_node(physical, partial.executions[e].candidate, id, &inputs)
            })
            .collect();
        let node_of_output: HashMap<(usize, MemoGroupId), PhysicalASAPNodeId> = self
            .outputs
            .iter()
            .enumerate()
            .map(|(output, &key)| (key, node_of[partial.answers[output].expect("complete")]))
            .collect();
        let mut roots = Vec::new();
        let mut scalar_roots = Vec::new();
        for (query, memo_query) in physical
            .logical_candidates
            .memo
            .queries()
            .iter()
            .enumerate()
        {
            let node = |group: &MemoGroupId| node_of_output[&(query, *group)];
            match &memo_query.root {
                QueryRoot::Operator(group) => roots.push(node(group)),
                QueryRoot::Scalar(expression) => scalar_roots.push((
                    query,
                    expression.map_operator_refs(&mut |group| node(group)),
                )),
            }
        }
        let mut plan = PhysicalASAPDAG::new(nodes, roots, scalar_roots).ok()?;
        set_least_retention(&mut plan, self.entries);
        plan.validate().ok()?;
        Some(plan)
    }
}

/// Candidate `v` as plan node `id` reading `inputs`: its operator, timing,
/// lifetime, θ, its group's schema, and, materialized across evaluations,
/// its group's coverage; no retention.
fn plan_node(
    physical: &PhysicalASAPMemoCandidates,
    v: PhysicalCandidateId,
    id: PhysicalASAPNodeId,
    inputs: &[PhysicalASAPNodeId],
) -> PhysicalASAPDAGNode {
    let memo = &physical.logical_candidates.memo;
    let candidate = &physical.physical_candidates[v.0];
    let group = &memo.groups()[candidate.logical_candidate_id.group.0 as usize].group_properties;
    let mut inputs = inputs.iter();
    let payload = physical
        .operator(v)
        .map_children(|_| *inputs.next().expect("one node per input"));
    PhysicalASAPDAGNode {
        id,
        output_state: ExecutionDataState {
            timing: candidate.physical_properties.timing(),
            primitive: DataPrimitive::of(&payload, group.result_kind),
        },
        payload,
        lifetime: candidate.physical_properties.lifetime(),
        parameters: memo
            .candidate(candidate.logical_candidate_id)
            .logical_properties
            .parameters()
            .cloned(),
        output_schema: group.schema.clone(),
        retention_ms: None,
        selected_physical_candidate: Some(v),
        view_coverage: candidate
            .physical_properties
            .is_across_evaluations()
            .then(|| view_coverage(memo, candidate.logical_candidate_id.group))
            .flatten(),
    }
}

/// Per node of `plan`, the queries whose walk from their roots reaches it.
pub fn queries_reaching(plan: &PhysicalASAPDAG) -> BTreeMap<PhysicalASAPNodeId, BTreeSet<usize>> {
    let mut reaching: BTreeMap<PhysicalASAPNodeId, BTreeSet<usize>> = BTreeMap::new();
    for (query, starts) in query_starts(plan).into_iter().enumerate() {
        let mut stack = starts;
        while let Some(id) = stack.pop() {
            if reaching.entry(id).or_default().insert(query) {
                stack.extend(
                    plan.edges
                        .iter()
                        .filter(|e| e.consumer == id)
                        .map(|e| e.producer),
                );
            }
        }
    }
    reaching
}

/// Per query, the nodes it reads directly: its operator root, or the
/// operators its scalar root reads.
fn query_starts(plan: &PhysicalASAPDAG) -> Vec<Vec<PhysicalASAPNodeId>> {
    let queries = plan.roots.len() + plan.scalar_roots.len();
    let mut starts = vec![Vec::new(); queries];
    let scalar: BTreeMap<usize, Vec<PhysicalASAPNodeId>> = plan
        .scalar_roots
        .iter()
        .map(|(query, expression)| {
            (
                *query,
                expression.operator_refs().into_iter().copied().collect(),
            )
        })
        .collect();
    let mut roots = plan.roots.iter();
    for (query, start) in starts.iter_mut().enumerate() {
        *start = match scalar.get(&query) {
            Some(nodes) => nodes.clone(),
            None => vec![*roots.next().expect("one root per operator query")],
        };
    }
    starts
}

/// Sets each node of `plan` that retains rows to the least its readers in
/// `plan` need (module documentation).
pub fn set_least_retention(plan: &mut PhysicalASAPDAG, entries: &[QueryWorkloadEntry]) {
    let reaching = queries_reaching(plan);
    let span_ms = |query: usize| {
        entries
            .get(query)
            .and_then(|e| e.range_evaluation)
            .map_or(0, |r| r.span_ms)
    };
    let longest_span_ms = |id: PhysicalASAPNodeId| {
        reaching
            .get(&id)
            .into_iter()
            .flatten()
            .map(|&q| span_ms(q))
            .max()
            .unwrap_or(0)
    };
    let starts = query_starts(plan);
    let retention: Vec<(PhysicalASAPNodeId, Option<u64>)> = plan
        .nodes
        .iter()
        .filter(|node| node.retains_rows())
        .map(|node| {
            let direct_ms = starts
                .iter()
                .enumerate()
                .filter(|(_, start)| start.contains(&node.id))
                .map(|(query, _)| span_ms(query))
                .max()
                .unwrap_or(0);
            let read_back = asap_cost::read_back_ms(&*plan, node.id, longest_span_ms);
            (node.id, read_back.map(|ms| ms.max(direct_ms)))
        })
        .collect();
    for (id, retention_ms) in retention {
        if let Some(node) = plan.nodes.iter_mut().find(|node| node.id == id) {
            node.retention_ms = retention_ms;
        }
    }
}

/// A plan's executions and edges as text: per query start, its node's
/// candidate and, recursively, its inputs' in input order. Two plans with
/// one signature select the same candidates over the same edges.
///
/// Example: `v7(v3(v0)) | v7(v3(v0))` for two queries answered by one
/// node.
pub fn plan_signature(plan: &PhysicalASAPDAG) -> String {
    fn of(plan: &PhysicalASAPDAG, id: PhysicalASAPNodeId) -> String {
        let node = plan
            .nodes
            .iter()
            .find(|node| node.id == id)
            .expect("a node");
        let inputs: Vec<String> = node
            .payload
            .children()
            .into_iter()
            .map(|&input| of(plan, input))
            .collect();
        format!(
            "v{}({})",
            node.selected_physical_candidate.map_or(usize::MAX, |v| v.0),
            inputs.join(",")
        )
    }
    query_starts(plan)
        .iter()
        .map(|start| {
            start
                .iter()
                .map(|&id| of(plan, id))
                .collect::<Vec<_>>()
                .join("+")
        })
        .collect::<Vec<_>>()
        .join(" | ")
}
