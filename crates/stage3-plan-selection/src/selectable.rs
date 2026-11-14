//! What the selection may select
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! section 5, "Execution contexts, by example"): the distinct execution
//! contexts $\kappa$ of a workload, every execution of a candidate in a
//! context, the edges $y_{\kappa,u,c,i}$ between executions, and per query
//! output the executions that may answer it ([`SelectableExecutions::new`]).
//!
//! - A query is evaluated in the evaluation context of its grid
//!   ([`evaluation_grid`]); queries on one grid share it, a range query's
//!   steps included.
//! - A candidate with $\ell$ = None or `OneEvaluation` runs once per context
//!   that selects it ($x_{\kappa,v}$), the context of its consumer; at
//!   ingestion time, where each arriving row is computed once whatever
//!   reads it (`asap_cost::computations`), in the one ingestion context
//!   [`EVERY_ARRIVING_ROW`].
//! - A candidate with $\ell$ = `AcrossEvaluations` runs once, in its own
//!   context `Materialized(v)` ($x^{\mathrm{mat}}_v = x_{\mathrm{Materialized}(v),v}$).
//!   Its inputs run in that context at query time; at ingestion time its
//!   producers must deliver on the ingestion pipeline of its refresh
//!   window.
//! - The input of a `TimeShift` anchored with `@ end()` or `@ start()` runs
//!   on the grid of the request end (start) times, once per request of a
//!   range query, in a context of its own per anchor: a request's start,
//!   its end and an evaluation are different times, so their results are
//!   not shared. An `@ end()` read by an instant query is anchored at its
//!   evaluation time, so its input runs in the evaluation context
//!   (`asap_cost::plan::execution_demand::anchor_is_evaluation_time`).
//! - An input of a consumer in $\kappa$ reads a producer, over an admissible
//!   edge, only where the producer delivers on $\kappa$
//!   (`PhysicalCandidate::delivers_on`).

use std::collections::BTreeSet;

use asap_cost::plan::execution_demand::{
    anchor_is_evaluation_time, evaluation_grid_ms, request_time_grid_ms,
};
use asap_types::ir::operator::AtModifier;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::{NonASAPOp, Operator, QueryRoot};
use asap_types::plan::materialized_view::RefreshWindow;
use asap_types::stages::{
    output_references, producer_satisfies_physical_requirements, EvaluationGrid, ExecutionContext,
    ExecutionContextId, Lifetime, MemoGroupId, PhysicalASAPMemoCandidates, PhysicalCandidateId,
    PhysicalEdgeId, PhysicalProperties,
};
use asap_types::workload::{greatest_common_divisor, QueryWorkloadEntry};

/// A query, by index in the workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct QueryId(pub usize);

/// The one context of every ingestion-time execution that is not
/// materialized: each second of arriving rows, which it computes once for
/// every ingestion-time view of every refresh window that reads it.
pub(crate) const EVERY_ARRIVING_ROW: ExecutionContext =
    ExecutionContext::IngestionPipeline(RefreshWindow {
        width_ms: 1_000,
        phase_ms: 0,
    });

/// One run of candidate `candidate` in context `context`: a node of the
/// plan. $x^{\mathrm{mat}}_v$ is the execution of $v$ in
/// `Materialized(v)`, $x_{\kappa,v}$ that of $v$ in $\kappa$.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Execution {
    pub(crate) context: ExecutionContextId,
    pub(crate) candidate: PhysicalCandidateId,
}

/// The distinct contexts of a workload and every execution and edge the
/// selection may choose, with the executions that may answer each query
/// output ([`Self::new`]). Executions are referred to by position in
/// [`Self::executions`].
pub(crate) struct SelectableExecutions {
    /// The distinct contexts, by [`ExecutionContextId`].
    contexts: Vec<ExecutionContext>,
    /// Per context, by [`ExecutionContextId`]: for the inputs of a
    /// `TimeShift` anchored with `@ end()` or `@ start()`, the anchor and
    /// the queries whose requests they run for (those on the shift's
    /// evaluation grid that reach it), ascending; `None` for any other
    /// context.
    request_anchor_of_context: Vec<Option<(AtModifier, Vec<QueryId>)>>,
    /// Every execution reached from a query output, ascending.
    executions: Vec<Execution>,
    /// Every $y_{\kappa,u,c,i}$ as (consumer execution, edge $(u,c,i)$,
    /// producer execution), executions by position, ascending; $\kappa$ is
    /// the consumer's context.
    edges: Vec<(usize, PhysicalEdgeId, usize)>,
    /// Per execution, by position, and per input of its candidate, the
    /// positions in [`Self::edges`] of the edges into it.
    edges_into: Vec<Vec<Vec<usize>>>,
    /// Each query output reference (a query, and the group its operator
    /// root, or one operator of its scalar root, reads), in
    /// [`output_groups`] order, with the executions, by position, that may
    /// answer it: $z_{q,v}$ exists only for them.
    pub(crate) answering: Vec<(QueryId, Vec<usize>)>,
}

impl SelectableExecutions {
    /// The contexts, executions and edges of `physical` for the queries
    /// `entries` (by query index), reached from `outputs` (each query output
    /// reference, a query and the group it reads, [`output_groups`]), and
    /// per output the executions that may answer it: a candidate of its
    /// group a query-time consumer may read
    /// (`producer_satisfies_physical_requirements`) that delivers on the
    /// query's context.
    pub(crate) fn new(
        physical: &PhysicalASAPMemoCandidates,
        entries: &[QueryWorkloadEntry],
        outputs: &[(QueryId, MemoGroupId)],
    ) -> Self {
        let mut selectable = Self {
            contexts: Vec::new(),
            request_anchor_of_context: Vec::new(),
            executions: Vec::new(),
            edges: Vec::new(),
            edges_into: Vec::new(),
            answering: Vec::new(),
        };
        let mut reached: BTreeSet<Execution> = BTreeSet::new();
        let mut pending: Vec<Execution> = Vec::new();
        let mut answering = Vec::with_capacity(outputs.len());
        for &(query, group) in outputs {
            let context = selectable.context_id(
                ExecutionContext::Evaluation {
                    grid: evaluation_grid(&entries[query.0]),
                },
                None,
            );
            let mut executions = Vec::new();
            for (v, candidate) in physical.physical_candidates.iter().enumerate() {
                if candidate.logical_candidate_id.group != group
                    || !producer_satisfies_physical_requirements(
                        candidate,
                        ExecutionTiming::QueryTime,
                    )
                {
                    continue;
                }
                if let Some(execution) =
                    selectable.execution_reading(physical, context, PhysicalCandidateId(v))
                {
                    executions.push(execution);
                    if reached.insert(execution) {
                        pending.push(execution);
                    }
                }
            }
            answering.push((query, executions));
        }
        let mut edges = BTreeSet::new();
        while let Some(consumer) = pending.pop() {
            let c = consumer.candidate;
            let context = selectable.input_context(physical, entries, consumer);
            for input in 0..physical.input_count(c) {
                for &edge in physical.admissible_edges_into(c, input) {
                    let u = physical.edge(edge).producer_candidate_id;
                    if let Some(producer) = selectable.execution_reading(physical, context, u) {
                        edges.insert((consumer, edge, producer));
                        if reached.insert(producer) {
                            pending.push(producer);
                        }
                    }
                }
            }
        }
        selectable.executions = reached.into_iter().collect();
        let position = |execution: Execution| {
            selectable
                .executions
                .binary_search(&execution)
                .expect("a reached execution")
        };
        selectable.edges = edges
            .into_iter()
            .map(|(consumer, edge, producer)| (position(consumer), edge, position(producer)))
            .collect();
        selectable.answering = answering
            .into_iter()
            .map(|(query, executions)| (query, executions.into_iter().map(position).collect()))
            .collect();
        selectable.edges_into = selectable
            .executions
            .iter()
            .map(|execution| vec![Vec::new(); physical.input_count(execution.candidate)])
            .collect();
        for (position, &(consumer, edge, _)) in selectable.edges.iter().enumerate() {
            selectable.edges_into[consumer][physical.edge(edge).input_index_of_consumer]
                .push(position);
        }
        selectable
    }

    /// The id of `context` with `request_anchor`
    /// ([`Self::request_anchor_of_context`]), added to the list when new.
    fn context_id(
        &mut self,
        context: ExecutionContext,
        request_anchor: Option<(AtModifier, Vec<QueryId>)>,
    ) -> ExecutionContextId {
        match self
            .contexts
            .iter()
            .zip(&self.request_anchor_of_context)
            .position(|known| known == (&context, &request_anchor))
        {
            Some(index) => ExecutionContextId(index),
            None => {
                self.contexts.push(context);
                self.request_anchor_of_context.push(request_anchor);
                ExecutionContextId(self.contexts.len() - 1)
            }
        }
    }

    /// The execution of producer `u` a consumer whose inputs run in
    /// `context` reads, when `u` delivers on it: its own for an
    /// `AcrossEvaluations` producer, the one ingestion context
    /// [`EVERY_ARRIVING_ROW`] for another ingestion-time producer, otherwise
    /// its run in `context`.
    fn execution_reading(
        &mut self,
        physical: &PhysicalASAPMemoCandidates,
        context: ExecutionContextId,
        u: PhysicalCandidateId,
    ) -> Option<Execution> {
        let producer = &physical.physical_candidates[u.0];
        if !producer.delivers_on(physical, &self.contexts[context.0]) {
            return None;
        }
        let properties = producer.physical_properties;
        let context = if properties.is_across_evaluations() {
            self.context_id(ExecutionContext::Materialized(u), None)
        } else if properties.timing() == ExecutionTiming::IngestionTime {
            self.context_id(EVERY_ARRIVING_ROW, None)
        } else {
            context
        };
        Some(Execution {
            context,
            candidate: u,
        })
    }

    /// The context the inputs of `consumer` run in: an `AcrossEvaluations`
    /// candidate's at ingestion time, the ingestion pipeline of its refresh
    /// window (`PhysicalCandidate::own_schedule`); a `TimeShift` anchored at
    /// `@ end()` or `@ start()` in an evaluation context, a context of its
    /// own per anchor, on the grid of its queries' request end (start)
    /// times: a request's start and end, and an evaluation, are different
    /// times (unless the anchor is the queries' evaluation time,
    /// `anchor_is_evaluation_time`); else the consumer's own context.
    fn input_context(
        &mut self,
        physical: &PhysicalASAPMemoCandidates,
        entries: &[QueryWorkloadEntry],
        consumer: Execution,
    ) -> ExecutionContextId {
        let candidate = &physical.physical_candidates[consumer.candidate.0];
        let memo = &physical.logical_candidates.memo;
        let group = candidate.logical_candidate_id.group;
        let properties = candidate.physical_properties;
        if properties.timing() == ExecutionTiming::IngestionTime
            && properties.is_across_evaluations()
        {
            return match candidate.own_schedule(physical) {
                Some(window) => self.context_id(ExecutionContext::IngestionPipeline(window), None),
                None => consumer.context,
            };
        }
        let at = match memo
            .candidate(candidate.logical_candidate_id)
            .expression
            .operator
        {
            Operator::NonASAP(NonASAPOp::TimeShift { shift, .. }) => shift.at,
            _ => None,
        };
        let (
            Some(at @ (AtModifier::End | AtModifier::Start)),
            ExecutionContext::Evaluation { grid },
        ) = (at, self.contexts[consumer.context.0])
        else {
            return consumer.context;
        };
        let queries: Vec<QueryId> = memo.groups()[group.0 as usize]
            .reached_by_queries()
            .iter()
            .filter(|&&query| {
                entries
                    .get(query)
                    .is_some_and(|entry| evaluation_grid(entry) == grid)
            })
            .map(|&query| QueryId(query))
            .collect();
        if anchor_is_evaluation_time(at, queries.iter().map(|query| &entries[query.0])) {
            return consumer.context;
        }
        let period_ms = queries.iter().fold(0, |period_ms, query| {
            greatest_common_divisor(period_ms, request_time_grid_ms(&entries[query.0], at))
        });
        match period_ms {
            0 => consumer.context,
            period_ms => self.context_id(
                ExecutionContext::Evaluation {
                    grid: EvaluationGrid {
                        period_ms,
                        phase_ms: 0,
                    },
                },
                Some((at, queries)),
            ),
        }
    }

    /// Keeps, of the executions answering each query output, those `keep`
    /// keeps (given the query, the execution's position and the
    /// execution).
    pub(crate) fn retain_answering(
        &mut self,
        mut keep: impl FnMut(QueryId, usize, Execution) -> bool,
    ) {
        let executions = &self.executions;
        for (query, answering) in &mut self.answering {
            answering.retain(|&index| keep(*query, index, executions[index]));
        }
    }

    /// The context with id `id`.
    pub(crate) fn context(&self, id: ExecutionContextId) -> ExecutionContext {
        self.contexts[id.0]
    }

    /// For the context with id `id` that runs the inputs of a `TimeShift`
    /// anchored with `@ end()` or `@ start()`: the anchor, and the queries
    /// whose requests it runs for.
    pub(crate) fn request_anchor(
        &self,
        id: ExecutionContextId,
    ) -> Option<(AtModifier, &[QueryId])> {
        self.request_anchor_of_context[id.0]
            .as_ref()
            .map(|(at, queries)| (*at, queries.as_slice()))
    }

    /// Every execution reached from a query output, ascending.
    pub(crate) fn executions(&self) -> &[Execution] {
        &self.executions
    }

    /// Every $y_{\kappa,u,c,i}$ as (consumer execution, edge, producer
    /// execution), executions by position, ascending.
    pub(crate) fn edges(&self) -> &[(usize, PhysicalEdgeId, usize)] {
        &self.edges
    }

    /// The positions in [`Self::edges`] of the edges into input `input` of
    /// the execution at position `consumer`.
    pub(crate) fn edges_into(&self, consumer: usize, input: usize) -> &[usize] {
        &self.edges_into[consumer][input]
    }

    /// The number of inputs of the execution at position `index`.
    pub(crate) fn input_count(&self, index: usize) -> usize {
        self.edges_into[index].len()
    }
}

/// Whether a candidate with `properties` has at most one consumer per
/// context: a query-time result that is not materialized ($\ell$ = None).
/// Consumers in one context that share a result read a `OneEvaluation` or a
/// materialized producer; the consumers of one ingestion run share its
/// rows.
pub(crate) fn has_one_reader_per_context(properties: PhysicalProperties) -> bool {
    properties.lifetime() == Lifetime::None && properties.timing() == ExecutionTiming::QueryTime
}

/// Each query output reference (a query, and the group its operator root,
/// or one operator of its scalar root, reads), in order.
pub(crate) fn output_groups(physical: &PhysicalASAPMemoCandidates) -> Vec<(QueryId, MemoGroupId)> {
    let roots: Vec<QueryRoot<MemoGroupId>> = physical
        .logical_candidates
        .memo
        .queries()
        .iter()
        .map(|query| query.root.clone())
        .collect();
    output_references(&roots)
        .into_iter()
        .map(|(query, group)| (QueryId(query), group))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The evaluation grid of `entry`'s query, the grid of its context
/// ([`evaluation_grid_ms`]), anchored at the epoch.
pub(crate) fn evaluation_grid(entry: &QueryWorkloadEntry) -> EvaluationGrid {
    EvaluationGrid {
        period_ms: evaluation_grid_ms(entry),
        phase_ms: 0,
    }
}
