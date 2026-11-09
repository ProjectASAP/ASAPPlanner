//! Execution contexts $\kappa$ (concepts doc, section 5, "Execution
//! contexts, by example"): the distinct contexts of a workload, every
//! execution of a candidate the selection may choose, and the edges
//! $y_{\kappa,u,c,i}$ between executions.
//!
//! - A query is evaluated in the evaluation context of its grid
//!   ([`evaluation_grid`]); queries on one grid share it, a range query's
//!   steps included.
//! - A candidate with $\ell$ = None or `OneEvaluation` runs once per context
//!   that selects it ($x_{\kappa,v}$), the context of its consumer.
//! - A candidate with $\ell$ = `AcrossEvaluations` runs once, in its own
//!   context `Materialized(v)` ($x^{\mathrm{mat}}_v = x_{\mathrm{Materialized}(v),v}$).
//!   Its inputs run in that context at query time, and in the ingestion
//!   pipeline of its refresh window at ingestion time, which the other
//!   ingestion-time builds of that window share.
//! - The input of a `TimeShift` anchored with `@ end()` or `@ start()` runs
//!   on the grid of the request end (start) times: once per request of a
//!   range query, as an instant query with its recurrence is evaluated.
//! - An input of a consumer in $\kappa$ reads a producer, over an admissible
//!   edge, only where the producer delivers on $\kappa$
//!   (`PhysicalCandidate::delivers_on`).

use std::collections::BTreeSet;

use asap_types::ir::materialized_view::RefreshWindow;
use asap_types::ir::operator::AtModifier;
use asap_types::ir::{NonASAPOp, Operator};
use asap_types::physical::{ExecutionTiming, Lifetime};
use asap_types::stages::{
    producer_satisfies_physical_requirements, EvaluationGrid, ExecutionContext, ExecutionContextId,
    MemoGroupId, PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdgeId,
};
use asap_types::workload::{greatest_common_divisor, QueryWorkloadEntry};

use crate::cost::input_count;
use crate::milp::model::QueryId;

/// One run of candidate `candidate` in context `context`: a node of the
/// plan. $x^{\mathrm{mat}}_v$ is the execution of $v$ in
/// `Materialized(v)`, $x_{\kappa,v}$ that of $v$ in $\kappa$.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Execution {
    pub(crate) context: ExecutionContextId,
    pub(crate) candidate: PhysicalCandidateId,
}

/// The distinct contexts of a workload and every execution and edge the
/// selection may choose ([`Self::new`]).
pub(crate) struct ExecutionContexts {
    /// The distinct contexts, by [`ExecutionContextId`].
    contexts: Vec<ExecutionContext>,
    /// Every execution reached from a query output, ascending.
    executions: Vec<Execution>,
    /// Every $y_{\kappa,u,c,i}$ as (consumer execution, edge $(u,c,i)$,
    /// producer execution), ascending; $\kappa$ is the consumer's context.
    edges: Vec<(Execution, PhysicalEdgeId, Execution)>,
}

impl ExecutionContexts {
    /// The contexts, executions and edges of `physical` for the queries
    /// `entries` (by query index), reached from `outputs` (each query output
    /// reference, a query and the group it reads), and per output the
    /// executions that may answer it: a candidate of its group a query-time
    /// consumer may read (`producer_satisfies_physical_requirements`) that
    /// delivers on the query's context.
    pub(crate) fn new(
        physical: &PhysicalASAPMemoCandidates,
        entries: &[QueryWorkloadEntry],
        outputs: &[(QueryId, MemoGroupId)],
    ) -> (Self, Vec<(QueryId, Vec<Execution>)>) {
        let mut contexts = Self {
            contexts: Vec::new(),
            executions: Vec::new(),
            edges: Vec::new(),
        };
        let mut reached: BTreeSet<Execution> = BTreeSet::new();
        let mut pending: Vec<Execution> = Vec::new();
        let mut output_executions = Vec::with_capacity(outputs.len());
        for &(query, group) in outputs {
            let context = contexts.context_id(ExecutionContext::Evaluation {
                grid: evaluation_grid(&entries[query.0]),
            });
            let mut answering = Vec::new();
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
                    contexts.execution_reading(physical, context, PhysicalCandidateId(v))
                {
                    answering.push(execution);
                    if reached.insert(execution) {
                        pending.push(execution);
                    }
                }
            }
            output_executions.push((query, answering));
        }
        let mut edges = BTreeSet::new();
        while let Some(consumer) = pending.pop() {
            let c = consumer.candidate;
            let context = contexts.input_context(physical, entries, consumer);
            for input in 0..input_count(physical, c) {
                for &edge in physical.admissible_edges_into(c, input) {
                    let u = physical.edge(edge).producer_candidate_id;
                    if let Some(producer) = contexts.execution_reading(physical, context, u) {
                        edges.insert((consumer, edge, producer));
                        if reached.insert(producer) {
                            pending.push(producer);
                        }
                    }
                }
            }
        }
        contexts.executions = reached.into_iter().collect();
        contexts.edges = edges.into_iter().collect();
        (contexts, output_executions)
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

    /// The execution of producer `u` a consumer in `context` reads, when
    /// `u` delivers on it: its own for an `AcrossEvaluations` producer,
    /// otherwise its run in `context`.
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
        let context = match producer.physical_properties.lifetime() {
            Lifetime::AcrossEvaluations(_) => self.context_id(ExecutionContext::Materialized(u)),
            Lifetime::None | Lifetime::OneEvaluation(_) => context,
        };
        Some(Execution {
            context,
            candidate: u,
        })
    }

    /// The context the inputs of `consumer` run in: an `AcrossEvaluations`
    /// candidate's at ingestion time, the ingestion pipeline of its refresh
    /// window (its chain width, else its group's evaluation grid);
    /// a `TimeShift` anchored at `@ end()` or `@ start()` in an evaluation
    /// context, the grid of its queries' request end or start times; else
    /// the consumer's own context.
    fn input_context(
        &mut self,
        physical: &PhysicalASAPMemoCandidates,
        entries: &[QueryWorkloadEntry],
        consumer: Execution,
    ) -> ExecutionContextId {
        let candidate = &physical.physical_candidates[consumer.candidate.0];
        let memo = &physical.logical_candidates.memo;
        let group = candidate.logical_candidate_id.group;
        let at_ingestion = candidate.physical_properties.timing() == ExecutionTiming::IngestionTime;
        if at_ingestion
            && matches!(
                candidate.physical_properties.lifetime(),
                Lifetime::AcrossEvaluations(_)
            )
        {
            let refresh_window = match candidate.chain_tumbling_window_width_ms(physical) {
                Some(width_ms) => Some(RefreshWindow {
                    width_ms,
                    phase_ms: 0,
                }),
                None => physical.logical_candidates.evaluation_grid_of_group[group.0 as usize].map(
                    |grid| RefreshWindow {
                        width_ms: grid.period_ms,
                        phase_ms: grid.phase_ms,
                    },
                ),
            };
            return match refresh_window {
                Some(window) => self.context_id(ExecutionContext::IngestionPipeline(window)),
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
        let period_ms = memo.groups()[group.0 as usize]
            .reached_by_queries()
            .iter()
            .filter_map(|&query| entries.get(query))
            .filter(|entry| evaluation_grid(entry) == grid)
            .fold(0, |period_ms, entry| {
                greatest_common_divisor(period_ms, request_time_grid_ms(entry, at))
            });
        match period_ms {
            0 => consumer.context,
            period_ms => self.context_id(ExecutionContext::Evaluation {
                grid: EvaluationGrid {
                    period_ms,
                    phase_ms: 0,
                },
            }),
        }
    }

    /// Every execution reached from a query output, ascending.
    pub(crate) fn executions(&self) -> &[Execution] {
        &self.executions
    }

    /// Every $y_{\kappa,u,c,i}$ as (consumer execution, edge, producer
    /// execution), ascending.
    pub(crate) fn edges(&self) -> &[(Execution, PhysicalEdgeId, Execution)] {
        &self.edges
    }

    /// The position of `execution` in [`Self::executions`].
    pub(crate) fn index_of(&self, execution: Execution) -> usize {
        self.executions
            .binary_search(&execution)
            .expect("a reached execution")
    }
}

/// The evaluation grid of `entry`'s query, the grid of its context: the
/// times its evaluations fall on (`QueryRecurrence::evaluation_grid_ms` of
/// its range evaluation), anchored at the epoch. Times that are not known
/// may fall on any millisecond: a 1 ms grid, on which only producers stored
/// at every time deliver.
pub(crate) fn evaluation_grid(entry: &QueryWorkloadEntry) -> EvaluationGrid {
    EvaluationGrid {
        period_ms: entry
            .recurrence
            .evaluation_grid_ms(entry.range_evaluation)
            .unwrap_or(1),
        phase_ms: 0,
    }
}

/// The grid, in ms, of the times `entry`'s requests anchor `@ end()` or
/// `@ start()` (`at`) at ([`asap_cost::execution_demand::request_time_grid_ms`]).
fn request_time_grid_ms(entry: &QueryWorkloadEntry, at: AtModifier) -> u64 {
    asap_cost::execution_demand::request_time_grid_ms(&entry.recurrence, entry.range_evaluation, at)
}

/// $\rho$ of `execution`'s context, as how often it runs, from the queries
/// it serves: an ingestion-time execution is costed over one second of
/// arriving rows (1); in an evaluation context, the combined evaluation
/// rate (`asap_cost::execution_demand::evaluation_rate`) of the queries on
/// its grid that reach its group, with their range evaluations, and of
/// the range queries whose request end (start) times are the grid, once per
/// request (an input of a `TimeShift` anchored with `@`); in
/// `Materialized(m)`, one
/// build per window of a query-time chain $m$, else once per request of
/// the queries reaching $m$'s group.
pub(crate) fn executions_per_second(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    contexts: &ExecutionContexts,
    execution: Execution,
    non_repeating_query_horizon_seconds: f64,
) -> Result<f64, String> {
    use asap_cost::execution_demand::evaluation_rate;
    let candidate = &physical.physical_candidates[execution.candidate.0];
    if candidate.physical_properties.timing() == ExecutionTiming::IngestionTime {
        return Ok(1.0);
    }
    let memo = &physical.logical_candidates.memo;
    let reaching = |v: PhysicalCandidateId| {
        let group = physical.physical_candidates[v.0].logical_candidate_id.group;
        memo.groups()[group.0 as usize]
            .reached_by_queries()
            .iter()
            .filter_map(|&query| entries.get(query))
    };
    let rate = match contexts.contexts[execution.context.0] {
        ExecutionContext::Evaluation { grid } => evaluation_rate(
            reaching(execution.candidate).filter_map(|entry| {
                if evaluation_grid(entry) == grid {
                    return Some((&entry.recurrence, entry.range_evaluation));
                }
                let request_time = [AtModifier::End, AtModifier::Start]
                    .into_iter()
                    .any(|at| grid.period_ms == request_time_grid_ms(entry, at));
                (entry.range_evaluation.is_some() && request_time)
                    .then_some((&entry.recurrence, None))
            }),
            non_repeating_query_horizon_seconds,
        ),
        ExecutionContext::Materialized(m) => {
            match physical.physical_candidates[m.0].chain_tumbling_window_width_ms(physical) {
                Some(width_ms) => Ok(1_000.0 / width_ms.max(1) as f64),
                None => evaluation_rate(
                    reaching(m).map(|entry| (&entry.recurrence, None)),
                    non_repeating_query_horizon_seconds,
                ),
            }
        }
        // Only ingestion-time candidates run in an ingestion pipeline.
        ExecutionContext::IngestionPipeline(_) => Ok(1.0),
    };
    rate.map_err(|error| error.to_string())
}
