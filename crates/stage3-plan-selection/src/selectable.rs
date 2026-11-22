//! What the selection may select
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! section 5, "Execution contexts, by example"): the distinct execution
//! contexts $\kappa$ of a workload, every execution of a candidate in a
//! context, the edges $y_{\kappa,u,c,i}$ between executions, and per query
//! output the executions that may answer it ([`SelectableExecutions::new`]).
//!
//! - A query is evaluated in the evaluation context of its grid
//!   (`QueryWorkloadEntry::evaluation_grid`); queries on one grid share it, a range query's
//!   steps included.
//! - A candidate with $\ell$ = None or `OneEvaluation` runs once per context
//!   that selects it ($x_{\kappa,v}$), the context of its consumer; at
//!   ingestion time, where each arriving row is computed once whatever
//!   reads it (`asap_cost::computations`), in the one ingestion context
//!   `ExecutionContext::EVERY_ARRIVING_ROW`.
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
//!   (`asap_types::stages::anchor_is_evaluation_time`).
//! - An input of a consumer in $\kappa$ reads a producer, over an admissible
//!   edge, only where the producer delivers on $\kappa$
//!   (`PhysicalASAPMemoCandidates::delivers_on`).

use std::collections::{BTreeSet, HashMap};

use asap_types::deployment_capabilities::FinalResultsCache;

use asap_types::ir::operator::AtModifier;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::plan::materialized_view::RefreshWindow;
use asap_types::plan::time_window::TimeIndexedPlan;
use asap_types::stages::{
    anchor_is_evaluation_time, producer_satisfies_physical_requirements, queries_of_context,
    query_reads, EvaluationGrid, ExecutionContext, ExecutionContextId, MemoGroupId,
    PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdgeId,
};
use asap_types::workload::{greatest_common_divisor, QueryWorkloadEntry};

/// A query, by index in the workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct QueryId(pub usize);

/// What reads an execution's rows back ([`SelectableExecutions::reads_back`]): a selectable edge
/// out of it, by position in [`SelectableExecutions::edges`], or a query
/// output it may answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reader {
    Edge(usize),
    Output(QueryId),
}

/// One run of candidate `candidate` in context `context`: a node of the
/// plan. $x^{\mathrm{mat}}_v$ is the execution of $v$ in
/// `Materialized(v)`, $x_{\kappa,v}$ that of $v$ in $\kappa$.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Execution {
    pub(crate) context: ExecutionContextId,
    pub(crate) candidate: PhysicalCandidateId,
}

/// One $y_{\kappa,u,c,i}$ the selection may choose: edge `edge` from the
/// producer execution `producer` into the consumer execution `consumer`,
/// executions by position in [`SelectableExecutions::executions`];
/// $\kappa$ is the consumer's context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SelectableEdge {
    pub(crate) consumer: usize,
    pub(crate) edge: PhysicalEdgeId,
    pub(crate) producer: usize,
}

/// A context's request anchor ([`SelectableExecutions::request_anchor`]):
/// the anchor of the `TimeShift` whose inputs it runs, and the queries
/// whose requests it runs for.
type RequestAnchor = Option<(AtModifier, Vec<QueryId>)>;

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
    request_anchor_of_context: Vec<RequestAnchor>,
    /// The id of each context with its request anchor.
    context_ids: HashMap<(ExecutionContext, RequestAnchor), ExecutionContextId>,
    /// Per memo group, each (query, anchor) whose walk over the memo reaches
    /// it ([`query_reads`]): the anchor of the `TimeShift` above it it is
    /// read per request below, `None` in the query's evaluation context.
    reads_of_group: HashMap<MemoGroupId, Vec<(usize, Option<AtModifier>)>>,
    /// Every execution reached from a query output, ascending.
    executions: Vec<Execution>,
    /// Every $y_{\kappa,u,c,i}$, ascending.
    edges: Vec<SelectableEdge>,
    /// Per execution, by position, and per input of its candidate, the
    /// positions in [`Self::edges`] of the edges into it.
    edges_into: Vec<Vec<Vec<usize>>>,
    /// Per execution, by position, the positions in [`Self::edges`] of the
    /// edges out of it.
    edges_out_of: Vec<Vec<usize>>,
    /// Each query output reference (a query, and the group its operator
    /// root, or one operator of its scalar root, reads), in
    /// `Memo::output_groups` order, with the executions, by position, that may
    /// answer it: $z_{q,v}$ exists only for them.
    answering: Vec<(QueryId, Vec<usize>)>,
    /// Per query, by [`QueryId`], the evaluation context it is evaluated
    /// in: `Evaluation { grid: entry.evaluation_grid() }`, no request
    /// anchor; `None` for a query with no output reference. Its answering
    /// executions may run elsewhere (an `AcrossEvaluations` one in its own
    /// `Materialized(v)`), so this is not read off them.
    ///
    /// Example: a query every 60 s is evaluated in the context of grid
    /// 60 s at phase 0.
    evaluation_context_of_query: Vec<Option<ExecutionContextId>>,
    /// Per query, by [`QueryId`], how far back from a request's end its
    /// evaluations' inputs are kept beyond their look-back, in ms
    /// ([`QueryWorkloadEntry::retained_range_span_ms`]).
    ///
    /// Example: a 1 h range evaluation at 15 s steps every 15 s: 3,600,000
    /// ms, or 15,000 ms when the data plane caches final query results.
    retained_span_ms_of_query: Vec<u64>,
}

impl SelectableExecutions {
    /// The contexts, executions and edges of `physical` for the queries
    /// `entries` (by query index), reached from `outputs` (each query output
    /// reference, a query and the group it reads, `Memo::output_groups`), and
    /// per output the executions that may answer it: a candidate of its
    /// group a query-time consumer may read
    /// (`producer_satisfies_physical_requirements`) that delivers on the
    /// query's context. `final_results_cache` (whether the data plane
    /// caches final query results) sets how far back each query's
    /// evaluations are read ([`Self::retained_span_ms_of_query`]).
    pub(crate) fn new(
        physical: &PhysicalASAPMemoCandidates,
        entries: &[QueryWorkloadEntry],
        outputs: &[(QueryId, MemoGroupId)],
        final_results_cache: FinalResultsCache,
    ) -> Self {
        let mut selectable = Self {
            reads_of_group: reads_of_groups(physical, entries, outputs),
            contexts: Vec::new(),
            request_anchor_of_context: Vec::new(),
            context_ids: HashMap::new(),
            executions: Vec::new(),
            edges: Vec::new(),
            edges_into: Vec::new(),
            edges_out_of: Vec::new(),
            answering: Vec::new(),
            evaluation_context_of_query: vec![None; entries.len()],
            retained_span_ms_of_query: entries
                .iter()
                .map(|entry| entry.retained_range_span_ms(final_results_cache))
                .collect(),
        };
        let mut reached: BTreeSet<Execution> = BTreeSet::new();
        let mut pending: Vec<Execution> = Vec::new();
        let mut answering = Vec::with_capacity(outputs.len());
        for &(query, group) in outputs {
            let context = selectable.context_id(
                ExecutionContext::Evaluation {
                    grid: entries[query.0].evaluation_grid(),
                },
                None,
            );
            selectable.evaluation_context_of_query[query.0] = Some(context);
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
                if let Some(execution) = selectable.execution_reading(
                    physical,
                    context,
                    PhysicalCandidateId(v),
                    false,
                    false,
                ) {
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
            let reads_open_window = reads_open_windows(physical, c);
            for input in 0..physical.input_count(c) {
                for &edge in physical.admissible_edges_into(c, input) {
                    let u = physical.edge(edge).producer_candidate_id;
                    let off_the_grid = physical.operator(c).reads_input_off_the_grid(input);
                    if let Some(producer) = selectable.execution_reading(
                        physical,
                        context,
                        u,
                        off_the_grid,
                        reads_open_window,
                    ) {
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
            .map(|(consumer, edge, producer)| SelectableEdge {
                consumer: position(consumer),
                edge,
                producer: position(producer),
            })
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
        selectable.edges_out_of = vec![Vec::new(); selectable.executions.len()];
        for (position, edge) in selectable.edges.iter().enumerate() {
            selectable.edges_into[edge.consumer][physical.edge(edge.edge).input_index_of_consumer]
                .push(position);
            selectable.edges_out_of[edge.producer].push(position);
        }
        selectable
    }

    /// The id of `context` with `request_anchor`
    /// ([`Self::request_anchor_of_context`]), added to the list when new.
    fn context_id(
        &mut self,
        context: ExecutionContext,
        request_anchor: RequestAnchor,
    ) -> ExecutionContextId {
        let id = ExecutionContextId(self.contexts.len());
        *self
            .context_ids
            .entry((context, request_anchor.clone()))
            .or_insert_with(|| {
                self.contexts.push(context);
                self.request_anchor_of_context.push(request_anchor);
                id
            })
    }

    /// The execution of producer `u` a consumer whose inputs run in
    /// `context` reads, when `u` delivers on it (an input read off the grid
    /// on purpose, `read_off_the_grid`, only when stored at ingestion
    /// time): its own for an
    /// `AcrossEvaluations` producer, the one ingestion context
    /// `ExecutionContext::EVERY_ARRIVING_ROW` for another ingestion-time producer, otherwise
    /// its run in `context`. A compressed-samples chain read for decodes of
    /// its open window (`reads_open_window`) delivers on every context:
    /// the decode reads its sealed windows and the open one, and trims.
    fn execution_reading(
        &mut self,
        physical: &PhysicalASAPMemoCandidates,
        context: ExecutionContextId,
        u: PhysicalCandidateId,
        read_off_the_grid: bool,
        reads_open_window: bool,
    ) -> Option<Execution> {
        let producer = &physical.physical_candidates[u.0];
        let open_window_read =
            reads_open_window && is_compressed_samples_chain(physical.operator(u));
        // Read off the grid on purpose (`Operator::reads_input_off_the_grid`):
        // only windows stored at ingestion time, anchored at the epoch; a
        // chain built at query time ends its windows at the evaluation
        // time, not on the epoch grid.
        if read_off_the_grid {
            let epoch_anchored = producer.physical_properties.timing()
                == ExecutionTiming::IngestionTime
                && producer.physical_properties.is_across_evaluations();
            if !epoch_anchored {
                return None;
            }
        } else if !open_window_read && !physical.delivers_on(u, &self.contexts[context.0]) {
            return None;
        }
        let properties = producer.physical_properties;
        let context = if properties.is_across_evaluations() {
            self.context_id(ExecutionContext::Materialized(u), None)
        } else if properties.timing() == ExecutionTiming::IngestionTime {
            self.context_id(ExecutionContext::EVERY_ARRIVING_ROW, None)
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
    /// window, the one the installed plan refreshes it on
    /// (`RefreshWindow::computed_from` over the candidates: `sum(TimeRange(5m))`
    /// every 5 min, a 60 s chain over `TimeShift(30s)` at phase 30 s); a `TimeShift` anchored at
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
        let group = candidate.logical_candidate_id.group;
        let properties = candidate.physical_properties;
        if properties.timing() == ExecutionTiming::IngestionTime
            && properties.is_across_evaluations()
        {
            return match RefreshWindow::computed_from(physical, consumer.candidate) {
                Some(window) => self.context_id(ExecutionContext::IngestionPipeline(window), None),
                None => consumer.context,
            };
        }
        let (Some(at), ExecutionContext::Evaluation { grid }) = (
            physical.operator(consumer.candidate).request_anchor(),
            self.contexts[consumer.context.0],
        ) else {
            return consumer.context;
        };
        let queries: Vec<QueryId> =
            queries_of_context(grid.period_ms, self.readers_of(group, |_| true), entries)
                .into_iter()
                .map(QueryId)
                .collect();
        if anchor_is_evaluation_time(at, queries.iter().map(|query| &entries[query.0])) {
            return consumer.context;
        }
        let period_ms = queries.iter().fold(0, |period_ms, query| {
            greatest_common_divisor(period_ms, entries[query.0].request_time_grid_ms(at))
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

    /// The queries whose walk reaches `group` ([`Self::reads_of_group`]),
    /// each once, reading it with an anchor `anchored` keeps.
    pub(crate) fn readers_of(
        &self,
        group: MemoGroupId,
        anchored: impl Fn(Option<AtModifier>) -> bool,
    ) -> BTreeSet<usize> {
        self.reads_of_group
            .get(&group)
            .into_iter()
            .flatten()
            .filter(|(_, anchor)| anchored(*anchor))
            .map(|&(query, _)| query)
            .collect()
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

    /// The evaluation context query `query` is evaluated in
    /// ([`Self::evaluation_context_of_query`]); `None` for a query with no
    /// output reference.
    pub(crate) fn evaluation_context(&self, query: QueryId) -> Option<ExecutionContextId> {
        self.evaluation_context_of_query
            .get(query.0)
            .copied()
            .flatten()
    }

    /// Whether the context with id `id` is a root evaluation context: a
    /// query's own (`Evaluation`, no request anchor), not a request
    /// context, `Materialized(v)` or an ingestion pipeline.
    pub(crate) fn is_root_evaluation_context(&self, id: ExecutionContextId) -> bool {
        matches!(self.contexts[id.0], ExecutionContext::Evaluation { .. })
            && self.request_anchor_of_context[id.0].is_none()
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

    /// Every $y_{\kappa,u,c,i}$, ascending.
    pub(crate) fn edges(&self) -> &[SelectableEdge] {
        &self.edges
    }

    /// The positions in [`Self::edges`] of the edges out of the execution
    /// at position `producer`.
    pub(crate) fn edges_out_of(&self, producer: usize) -> &[usize] {
        &self.edges_out_of[producer]
    }

    /// Each query output reference with the executions, by position, that
    /// may answer it, in `Memo::output_groups` order.
    pub(crate) fn answering(&self) -> &[(QueryId, Vec<usize>)] {
        &self.answering
    }

    /// Every read of an execution's rows, as (the execution read, by
    /// position in [`Self::executions`]; its reader; how far back it reads,
    /// in ms, `None` when unbounded): each selectable edge into its
    /// producer, `need_ms_per_edge` by edge position
    /// (`crate::retention::edge_retention_need_ms`), and each query output
    /// into each execution that may answer it, the query's retained span
    /// ([`Self::retained_span_ms_of_query`]). Costing ($h_v$ at its largest), (R), the least $h_v$ of a
    /// selection and the greedy bound each fold it.
    pub(crate) fn reads_back<'a>(
        &'a self,
        need_ms_per_edge: &'a [Option<u64>],
    ) -> impl Iterator<Item = (usize, Reader, Option<u64>)> + 'a {
        let edges =
            self.edges().iter().zip(need_ms_per_edge).enumerate().map(
                |(position, (edge, &need_ms))| (edge.producer, Reader::Edge(position), need_ms),
            );
        let outputs = self.answering().iter().flat_map(move |(query, answering)| {
            let need_ms = self.retained_span_ms_of_query[query.0];
            answering
                .iter()
                .map(move |&index| (index, Reader::Output(*query), Some(need_ms)))
        });
        edges.chain(outputs)
    }

    /// Per query, by [`QueryId`], how far back from a request's end its
    /// evaluations' inputs are kept beyond their look-back, in ms
    /// ([`QueryWorkloadEntry::retained_range_span_ms`]).
    pub(crate) fn retained_span_ms_of_query(&self) -> &[u64] {
        &self.retained_span_ms_of_query
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

/// Per memo group of `physical`, each (query, anchor) whose walk from the
/// groups its outputs read (`outputs`) reaches it ([`query_reads`]),
/// through the inputs of every candidate of a group.
fn reads_of_groups(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    outputs: &[(QueryId, MemoGroupId)],
) -> HashMap<MemoGroupId, Vec<(usize, Option<AtModifier>)>> {
    let memo = &physical.logical_candidates.memo;
    let query_count = outputs
        .iter()
        .map(|(query, _)| query.0 + 1)
        .max()
        .unwrap_or(0);
    let mut starts = vec![Vec::new(); query_count];
    for &(query, group) in outputs {
        starts[query.0].push(group);
    }
    let inputs_of = |group: MemoGroupId| -> Vec<MemoGroupId> {
        let inputs: BTreeSet<MemoGroupId> = memo.groups()[group.0 as usize]
            .candidates
            .iter()
            .flat_map(|candidate| candidate.expression.operator.children())
            .copied()
            .collect();
        inputs.into_iter().collect()
    };
    query_reads(
        &starts,
        inputs_of,
        |group| memo.groups()[group.0 as usize].expression.request_anchor(),
        |_| false,
        entries,
    )
}

/// Whether `operator` builds a compressed-samples chain.
fn is_compressed_samples_chain(operator: &Operator<MemoGroupId>) -> bool {
    matches!(
        operator,
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            summary_type: FieldDataType::CompressedSamples,
            ..
        })
    )
}

/// Whether candidate `c` reads a compressed-samples chain only for decodes
/// of its open window: such a decode itself, or a `TimeShift` every
/// admissible reader of which does (through further shifts). A shift a
/// sealed decode also reads keeps the chain's delivery check; so does one
/// on a cycle of shifts (no decode reads it).
fn reads_open_windows(physical: &PhysicalASAPMemoCandidates, c: PhysicalCandidateId) -> bool {
    let mut seen = BTreeSet::from([c]);
    let mut pending = vec![c];
    while let Some(v) = pending.pop() {
        match physical.operator(v) {
            Operator::ASAP(ASAPOp::DecodeSamplesInRange {
                reads_unsealed_window: true,
                ..
            }) => {}
            Operator::NonASAP(NonASAPOp::TimeShift { .. }) => {
                let readers = physical.readers_of(v);
                if readers.is_empty() {
                    return false;
                }
                for reader in readers {
                    if !seen.insert(reader) {
                        return false;
                    }
                    pending.push(reader);
                }
            }
            _ => return false,
        }
    }
    true
}
