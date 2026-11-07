//! Execution contexts $\kappa$ (concepts doc, section 5, "Execution
//! identity"): the distinct contexts of a workload, each candidate's
//! executions, and the edges $y_{\kappa,u,c,i}$ may select.
//!
//! - An `AcrossEvaluations` candidate $v$ has one execution, its own
//!   computation ($x^{\mathrm{mat}}_v$). Its inputs run in its computation
//!   context: the ingestion pipeline $I_r$ of its refresh window $r$ when it
//!   is maintained at ingestion time with a bounded window, otherwise
//!   $\kappa_v$ (`ExecutionContext::Materialized`).
//! - A candidate with $\ell$ = None or `OneEvaluation` has one execution per
//!   context $\kappa$ that may read it ($x_{\kappa,v}$). A query's context is
//!   the evaluation $\kappa_T$ of its recurrence and range evaluation, shared
//!   by every query with equal ones. A chain of None and `OneEvaluation`
//!   candidates inherits its consumer's context; an `AcrossEvaluations`
//!   producer switches to its own computation context for its inputs.
//!   A `OneEvaluation` result belongs to one evaluation, so it runs only in
//!   evaluation contexts $\kappa_T$.
//! - An edge $(u, c, i)$ is selected per context of its consumer
//!   ($y_{\kappa,u,c,i}$): within $\kappa$ when $u$ is None or
//!   `OneEvaluation`; from $u$'s one execution when $u$ is
//!   `AcrossEvaluations`, and then only if $u$ delivers on $\kappa$
//!   (`PhysicalCandidate::delivers_on`).
//!
//! Contexts are deduplicated by equality ([`ExecutionContext`] holds an
//! `f64` and cannot be hashed); [`ExecutionContextId`] indexes the list.

use std::collections::{BTreeSet, HashMap};

use asap_types::ir::materialized_view::RefreshWindow;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, TimeRangeKind};
use asap_types::physical::{ExecutionTiming, Lifetime};
use asap_types::stages::{
    output_references, ExecutionContext, ExecutionContextId, Memo, MemoGroupId,
    PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdge, PhysicalEdgeId, RefreshWindowId,
    WindowLayout,
};
use asap_types::workload::QueryWorkloadEntry;

/// One execution of a candidate: a node of the selected DAG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Execution {
    /// An `AcrossEvaluations` candidate's own computation: $x^{\mathrm{mat}}_v$.
    Materialized(PhysicalCandidateId),
    /// A candidate with $\ell$ = None or `OneEvaluation` run in context
    /// $\kappa$: $x_{\kappa,v}$.
    InContext(ExecutionContextId, PhysicalCandidateId),
}

impl Execution {
    /// The candidate it runs.
    pub(crate) fn candidate(self) -> PhysicalCandidateId {
        match self {
            Execution::Materialized(v) | Execution::InContext(_, v) => v,
        }
    }
}

/// The distinct contexts of a workload and every execution and edge the
/// selection may choose ([`Self::new`]).
pub(crate) struct ExecutionContexts {
    /// The distinct contexts, by [`ExecutionContextId`].
    contexts: Vec<ExecutionContext>,
    /// The distinct refresh windows of the ingestion pipelines, by
    /// [`RefreshWindowId`].
    refresh_windows: Vec<RefreshWindow>,
    /// Each query's evaluation context $\kappa_T$, by query index.
    context_of_query: Vec<ExecutionContextId>,
    /// Each `AcrossEvaluations` candidate reached, with the context its
    /// inputs run in.
    computation_context: HashMap<PhysicalCandidateId, ExecutionContextId>,
    /// Every execution reached from a query output, ascending.
    executions: Vec<Execution>,
    /// Every $(\kappa, (u, c, i))$ a consumer execution in $\kappa$ may
    /// select, ascending.
    edges: Vec<(ExecutionContextId, PhysicalEdgeId)>,
}

impl ExecutionContexts {
    /// The contexts, executions and edges of `physical` for the queries
    /// `entries` (by index), reached from each query output: each candidate
    /// of an output group runs in the query's context (an
    /// `AcrossEvaluations` one if it delivers on it); each input of an
    /// execution in $\kappa$ reads, over an admissible edge, a None or
    /// `OneEvaluation` producer in $\kappa$ or an `AcrossEvaluations`
    /// producer that delivers on $\kappa$; an `AcrossEvaluations` execution's
    /// inputs run in its computation context.
    pub(crate) fn new(
        physical: &PhysicalASAPMemoCandidates,
        entries: &[QueryWorkloadEntry],
    ) -> Self {
        let memo = &physical.logical_candidates.memo;
        let mut contexts = Self {
            contexts: Vec::new(),
            refresh_windows: Vec::new(),
            context_of_query: Vec::new(),
            computation_context: HashMap::new(),
            executions: Vec::new(),
            edges: Vec::new(),
        };
        for entry in entries {
            let id = contexts.context_id(ExecutionContext::Evaluation {
                recurrence: entry.recurrence.clone(),
                range_evaluation: entry.range_evaluation,
            });
            contexts.context_of_query.push(id);
        }
        let mut candidates_of_group: HashMap<MemoGroupId, Vec<PhysicalCandidateId>> =
            HashMap::new();
        for (id, candidate) in physical.physical_candidates.iter().enumerate() {
            candidates_of_group
                .entry(candidate.logical_candidate_id.group)
                .or_default()
                .push(PhysicalCandidateId(id));
        }
        let all_edges: Vec<PhysicalEdge> = physical.edges().map(|(_, edge)| edge).collect();
        let mut reached: BTreeSet<Execution> = BTreeSet::new();
        let mut pending: Vec<Execution> = Vec::new();
        for (query, group) in output_references(&physical.query_roots) {
            for &v in candidates_of_group.get(&group).into_iter().flatten() {
                if let Some(execution) = contexts.output_execution(physical, query, v) {
                    if reached.insert(execution) {
                        pending.push(execution);
                    }
                }
            }
        }
        let mut edges = BTreeSet::new();
        while let Some(consumer) = pending.pop() {
            let c = consumer.candidate();
            let context = match consumer {
                Execution::InContext(context, _) => context,
                Execution::Materialized(_) => {
                    let context = contexts.computation_context_of(memo, physical, c);
                    contexts.computation_context.insert(c, context);
                    context
                }
            };
            let input_count = memo
                .candidate(physical.physical_candidates[c.0].logical_candidate_id)
                .expression
                .operator
                .children()
                .len();
            for input in 0..input_count {
                for &edge in physical.admissible_edges_into(c, input) {
                    let u = all_edges[edge.0].producer_candidate_id;
                    if let Some(producer) = contexts.execution_reading(physical, context, u) {
                        edges.insert((context, edge));
                        if reached.insert(producer) {
                            pending.push(producer);
                        }
                    }
                }
            }
        }
        contexts.executions = reached.into_iter().collect();
        contexts.edges = edges.into_iter().collect();
        contexts
    }

    /// The id of `context`, added to the list when new.
    fn context_id(&mut self, context: ExecutionContext) -> ExecutionContextId {
        match self.contexts.iter().position(|known| *known == context) {
            Some(index) => ExecutionContextId(index),
            None => {
                self.contexts.push(context);
                ExecutionContextId(self.contexts.len() - 1)
            }
        }
    }

    /// The execution of candidate `v` of one of query `query`'s output
    /// groups that gives the query's output, in the query's context: a
    /// consumer there reading `v` ([`Self::execution_reading`]), but never
    /// a result computed at ingestion time and not materialized, which no
    /// query reads.
    pub(crate) fn output_execution(
        &self,
        physical: &PhysicalASAPMemoCandidates,
        query: usize,
        v: PhysicalCandidateId,
    ) -> Option<Execution> {
        let context = self.context_of_query(query)?;
        let properties = physical.physical_candidates[v.0].physical_properties;
        if properties.timing() == ExecutionTiming::IngestionTime
            && properties.lifetime() == Lifetime::None
        {
            return None;
        }
        self.execution_reading(physical, context, v)
    }

    /// The execution of producer `u` a consumer in `context` reads: $u$'s
    /// own when it is `AcrossEvaluations` and delivers on `context`; its
    /// run in `context` when it is None, or `OneEvaluation` and `context` is
    /// an evaluation; `None` otherwise.
    fn execution_reading(
        &self,
        physical: &PhysicalASAPMemoCandidates,
        context: ExecutionContextId,
        u: PhysicalCandidateId,
    ) -> Option<Execution> {
        let producer = &physical.physical_candidates[u.0];
        let context_value = &self.contexts[context.0];
        match producer.physical_properties.lifetime() {
            Lifetime::AcrossEvaluations(_) => producer
                .delivers_on(&physical.logical_candidates.memo, context_value)
                .then_some(Execution::Materialized(u)),
            Lifetime::None => Some(Execution::InContext(context, u)),
            Lifetime::OneEvaluation(_) => {
                matches!(context_value, ExecutionContext::Evaluation { .. })
                    .then_some(Execution::InContext(context, u))
            }
        }
    }

    /// The context the inputs of `AcrossEvaluations` candidate `v` run in:
    /// the ingestion pipeline of its refresh window when it is maintained
    /// at ingestion time with a bounded window (as `asap_cost` refreshes
    /// the views of one refresh window in one ingestion run), otherwise its
    /// own computation.
    fn computation_context_of(
        &mut self,
        memo: &Memo,
        physical: &PhysicalASAPMemoCandidates,
        v: PhysicalCandidateId,
    ) -> ExecutionContextId {
        let candidate = &physical.physical_candidates[v.0];
        let refresh_window = (candidate.physical_properties.timing()
            == ExecutionTiming::IngestionTime)
            .then(|| TumblingWindowChainGeometry::of(memo, v, physical))
            .flatten()
            .and_then(|chain| chain.refresh_window());
        let context = match refresh_window {
            Some(window) => {
                let index = match self.refresh_windows.iter().position(|w| *w == window) {
                    Some(index) => index,
                    None => {
                        self.refresh_windows.push(window);
                        self.refresh_windows.len() - 1
                    }
                };
                ExecutionContext::IngestionPipeline(RefreshWindowId(index))
            }
            None => ExecutionContext::Materialized(v),
        };
        self.context_id(context)
    }

    /// The distinct contexts, by [`ExecutionContextId`].
    pub(crate) fn contexts(&self) -> &[ExecutionContext] {
        &self.contexts
    }

    /// Query `query`'s evaluation context.
    pub(crate) fn context_of_query(&self, query: usize) -> Option<ExecutionContextId> {
        self.context_of_query.get(query).copied()
    }

    /// The context the inputs of `AcrossEvaluations` candidate `v` run in,
    /// when `v` is reached.
    pub(crate) fn computation_context(&self, v: PhysicalCandidateId) -> Option<ExecutionContextId> {
        self.computation_context.get(&v).copied()
    }

    /// The context consumer execution `execution` reads its inputs in.
    pub(crate) fn input_context(&self, execution: Execution) -> Option<ExecutionContextId> {
        match execution {
            Execution::InContext(context, _) => Some(context),
            Execution::Materialized(v) => self.computation_context(v),
        }
    }

    /// Every execution reached from a query output, ascending.
    pub(crate) fn executions(&self) -> &[Execution] {
        &self.executions
    }

    /// Every $(\kappa, (u, c, i))$ a consumer execution in $\kappa$ may
    /// select, ascending.
    pub(crate) fn edges(&self) -> &[(ExecutionContextId, PhysicalEdgeId)] {
        &self.edges
    }

    /// The execution of candidate `v` that a consumer in `context` reads,
    /// or a query in `context` outputs: `v`'s own when `AcrossEvaluations`,
    /// otherwise its run in `context`.
    pub(crate) fn execution_in(
        physical: &PhysicalASAPMemoCandidates,
        context: ExecutionContextId,
        v: PhysicalCandidateId,
    ) -> Execution {
        match physical.physical_candidates[v.0]
            .physical_properties
            .lifetime()
        {
            Lifetime::AcrossEvaluations(_) => Execution::Materialized(v),
            Lifetime::None | Lifetime::OneEvaluation(_) => Execution::InContext(context, v),
        }
    }
}

/// Where a tumbling-window build's chain lies in time, as the realizer
/// builds it: windows `width_ms` wide, `window_count` of them over the
/// range its build reads (`range_input`), the newest ending
/// `end_offset_ms` before the evaluation: the offsets of the `TimeShift`s
/// below the range, added up as far as the first with an `@` anchor
/// (`anchored` when there is one).
#[derive(Debug, Clone, Copy)]
pub(crate) struct TumblingWindowChainGeometry {
    pub(crate) width_ms: u64,
    pub(crate) window_count: u64,
    pub(crate) end_offset_ms: i64,
    pub(crate) anchored: bool,
    pub(crate) range_input: MemoGroupId,
}

impl TumblingWindowChainGeometry {
    /// The chain of candidate `v`, when it is a build with a tumbling-window
    /// layout over a range its width divides.
    pub(crate) fn of(
        memo: &Memo,
        v: PhysicalCandidateId,
        physical: &PhysicalASAPMemoCandidates,
    ) -> Option<Self> {
        let logical = memo.candidate(physical.physical_candidates[v.0].logical_candidate_id);
        let Some(&WindowLayout::Tumbling { width_ms }) = logical.logical_properties.window_layout()
        else {
            return None;
        };
        let Operator::ASAP(ASAPOp::SummaryAgg {
            child: range_input, ..
        }) = &logical.expression.operator
        else {
            return None;
        };
        let Operator::NonASAP(NonASAPOp::TimeRange {
            range,
            kind: TimeRangeKind::Range,
            child,
        }) = &memo.groups()[range_input.0 as usize].expression
        else {
            return None;
        };
        let range_ms = u64::try_from(range.as_millis()).ok()?;
        if width_ms == 0 || range_ms % width_ms != 0 {
            return None;
        }
        let mut end_offset_ms = 0i64;
        let mut anchored = false;
        let mut below = *child;
        while let Operator::NonASAP(NonASAPOp::TimeShift { shift, child }) =
            &memo.groups()[below.0 as usize].expression
        {
            if shift.at.is_some() {
                anchored = true;
                break;
            }
            end_offset_ms = end_offset_ms.saturating_add(shift.offset_ms);
            below = *child;
        }
        Some(Self {
            width_ms,
            window_count: range_ms / width_ms,
            end_offset_ms,
            anchored,
            range_input: *range_input,
        })
    }

    /// The refresh window of its newest tumbling window (`RefreshWindow::of_window`),
    /// as `PhysicalASAPDAG::refresh_window` gives it for the chain; `None`
    /// under an `@` anchor.
    pub(crate) fn refresh_window(&self) -> Option<RefreshWindow> {
        if self.anchored {
            return None;
        }
        RefreshWindow::of_window(self.end_offset_ms, i64::try_from(self.width_ms).ok()?)
    }

    /// $\operatorname{need}_{\kappa,v,c,i}$ in windows: the tumbling windows
    /// a consumer whose queries evaluate `span_ms` back from each request's
    /// end reads, back from the evaluation: its offset, its window and
    /// that span (`asap_cost::tumbling_window_retention_ms`), over the
    /// width, at least the windows its range holds.
    pub(crate) fn retention_need_windows(&self, span_ms: u64) -> u64 {
        let offset_ms = u64::try_from(self.end_offset_ms).unwrap_or(0);
        let reach_ms = offset_ms
            .saturating_add(self.width_ms.saturating_mul(self.window_count))
            .saturating_add(span_ms);
        (reach_ms / self.width_ms).max(self.window_count)
    }
}
