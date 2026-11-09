//! Stage 2's output: each logical candidate's physical alternatives
//! $\mathcal C$, one per valid $p^{\mathrm{phys}} = (\tau, \ell)$, and
//! the physical edges $\mathcal E$ with $\alpha^{\log}$ and
//! $\alpha^{\mathrm{phys}}$ (#1200, #1201). Stage 3 selects among them;
//! Stage 2 prices nothing.

use std::collections::HashMap;

use super::execution_context::ExecutionContext;
use crate::ir::QueryRoot;
use crate::ir::{ASAPOp, Operator};
use crate::physical::{ExecutionTiming, Lifetime, PhysicalProperties};
use crate::stages::{CandidateForMemoGroupId, LogicalASAPMemoCandidates, PrunedCandidate};

/// Index into [`PhysicalASAPMemoCandidates::physical_candidates`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
pub struct PhysicalCandidateId(pub usize);

/// Index into [`PhysicalASAPMemoCandidates::edges`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PhysicalEdgeId(pub usize);

/// One physical alternative of a logical candidate: what Stage 3 selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PhysicalCandidate {
    /// $e_v$ and $p^{\log}_v$.
    pub logical_candidate_id: CandidateForMemoGroupId,
    /// $p^{\mathrm{phys}}_v$.
    pub physical_properties: PhysicalProperties,
}

impl PhysicalCandidate {
    /// Whether a consumer running in `context` can read this producer (the
    /// concepts doc's alignment table, section 2): true for $\ell$ = None,
    /// which runs in the consumer's own context; for `OneEvaluation` only in
    /// an `Evaluation` context, the one evaluation its result belongs to.
    /// With $\ell$ = `AcrossEvaluations`, every execution time of the
    /// context must be a time the producer has stored: for a chain
    /// (`SummaryTumblingWindowChain`) of width $w$, $w$ divides an
    /// evaluation grid's period and phase, or a refresh window's width and
    /// phase; any other result is stored on its group's evaluation grid
    /// (`evaluation_grid_of_group`), which must hold every time of the
    /// context, unless the group does not depend on the evaluation time
    /// (then it delivers on every context). The times of a
    /// `Materialized(v)` context are $v$'s own: every window end of a chain
    /// $v$ (its width at phase 0), else its group's evaluation grid; with
    /// neither they are not known, and no time-dependent producer delivers
    /// on it.
    pub fn delivers_on(
        &self,
        candidates: &PhysicalASAPMemoCandidates,
        context: &ExecutionContext,
    ) -> bool {
        match self.physical_properties.lifetime() {
            Lifetime::None => true,
            Lifetime::OneEvaluation(_) => matches!(context, ExecutionContext::Evaluation { .. }),
            Lifetime::AcrossEvaluations(_) => {
                let logical = &candidates.logical_candidates;
                let group = self.logical_candidate_id.group;
                let chain_width_ms = self.chain_width_ms(candidates);
                let depends_on_evaluation_time = logical.memo.groups()[group.0 as usize]
                    .group_properties
                    .depends_on_evaluation_time;
                if chain_width_ms.is_none() && !depends_on_evaluation_time {
                    return true;
                }
                let (period_ms, phase_ms) = match context {
                    ExecutionContext::Evaluation { grid } => (grid.period_ms, grid.phase_ms),
                    ExecutionContext::IngestionPipeline(window) => {
                        (window.width_ms, window.phase_ms)
                    }
                    ExecutionContext::Materialized(v) => {
                        let materialized = &candidates.physical_candidates[v.0];
                        match materialized.chain_width_ms(candidates) {
                            Some(width_ms) => (width_ms, 0),
                            None => match logical.evaluation_grid_of_group
                                [materialized.logical_candidate_id.group.0 as usize]
                            {
                                Some(grid) => (grid.period_ms, grid.phase_ms),
                                None => return false,
                            },
                        }
                    }
                };
                match chain_width_ms {
                    Some(width_ms) => {
                        period_ms.is_multiple_of(width_ms) && phase_ms.is_multiple_of(width_ms)
                    }
                    None => {
                        logical.evaluation_grid_of_group[group.0 as usize].is_some_and(|grid| {
                            period_ms.is_multiple_of(grid.period_ms)
                                && phase_ms % grid.period_ms == grid.phase_ms % grid.period_ms
                        })
                    }
                }
            }
        }
    }

    /// Its tumbling-window width when it is a chain
    /// (`SummaryTumblingWindowChain`).
    pub fn chain_width_ms(&self, candidates: &PhysicalASAPMemoCandidates) -> Option<u64> {
        match candidates
            .logical_candidates
            .memo
            .candidate(self.logical_candidate_id)
            .expression
            .operator
        {
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                tumbling_window_width_ms,
                ..
            }) => Some(tumbling_window_width_ms),
            _ => None,
        }
    }
}

/// $\alpha^{\mathrm{phys}}_{u,c,i}$: timing and lifetime only (the concepts
/// doc's table, section 2). Alignment is checked per context in Stage 3 with
/// [`PhysicalCandidate::delivers_on`].
pub fn producer_satisfies_physical_requirements(
    producer: &PhysicalCandidate,
    consumer_timing: ExecutionTiming,
) -> bool {
    use ExecutionTiming::{IngestionTime, QueryTime};
    match (
        producer.physical_properties.timing(),
        producer.physical_properties.lifetime(),
    ) {
        (QueryTime, _) => consumer_timing == QueryTime,
        (IngestionTime, Lifetime::None) => consumer_timing == IngestionTime,
        // Materialized at ingestion: read at ingestion time and at query time.
        (IngestionTime, Lifetime::AcrossEvaluations(_)) => true,
        (IngestionTime, Lifetime::OneEvaluation(_)) => {
            unreachable!("rejected by `PhysicalProperties::new`")
        }
    }
}

/// $(u, c, i)$ over physical candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PhysicalEdge {
    pub producer_candidate_id: PhysicalCandidateId,
    pub consumer_candidate_id: PhysicalCandidateId,
    pub input_index_of_consumer: usize,
    /// $\alpha^{\log}_{u,c,i}$.
    pub satisfies_logical_requirement: bool,
    /// $\alpha^{\mathrm{phys}}_{u,c,i}$.
    pub satisfies_physical_requirement: bool,
}

impl PhysicalEdge {
    /// Whether a plan may use the edge: $\alpha^{\log}_{u,c,i} =
    /// \alpha^{\mathrm{phys}}_{u,c,i} = 1$.
    pub fn is_admissible(&self) -> bool {
        self.satisfies_logical_requirement && self.satisfies_physical_requirement
    }
}

/// Stage 2's output, which Stage 3 selects from.
#[derive(Debug, Clone)]
pub struct PhysicalASAPMemoCandidates {
    /// Pass 2's output: the memo, its candidates' $e_v$, $p^{\log}_v$,
    /// $r^{\log}_{c,i}$, and the logical edges.
    pub logical_candidates: LogicalASAPMemoCandidates,
    /// $\mathcal C$, indexed by [`PhysicalCandidateId`].
    pub physical_candidates: Vec<PhysicalCandidate>,
    /// The alternatives Stage 2 pruned, in pruning order, each as the
    /// [`PhysicalCandidate`] it would have been: candidates are numbered
    /// after pruning, so a pruned one has no [`PhysicalCandidateId`].
    pub pruned_candidates: Vec<PrunedCandidate<PhysicalCandidate>>,
    /// $\mathcal E$, grouped by (consumer, input); indexed by
    /// [`PhysicalEdgeId`].
    edges: Vec<PhysicalEdge>,
    /// $P_{c,i}$ after $\alpha$: the admissible edges into each (consumer, input).
    admissible_edges_into_input: HashMap<(PhysicalCandidateId, usize), Vec<PhysicalEdgeId>>,
    /// The admissible edges out of each candidate, by [`PhysicalCandidateId`].
    admissible_edges_out_of_candidate: Vec<Vec<PhysicalEdgeId>>,
}

impl PhysicalASAPMemoCandidates {
    /// Stage 2's output over `physical_candidates` and `edges` (grouped by
    /// (consumer, input)), with the admissible-edge indexes built once.
    /// Query roots are read from `logical_candidates`, not copied.
    pub fn new(
        logical_candidates: LogicalASAPMemoCandidates,
        physical_candidates: Vec<PhysicalCandidate>,
        edges: Vec<PhysicalEdge>,
        pruned_candidates: Vec<PrunedCandidate<PhysicalCandidate>>,
    ) -> Self {
        let mut admissible_edges_into_input: HashMap<_, Vec<PhysicalEdgeId>> = HashMap::new();
        let mut admissible_edges_out_of_candidate = vec![Vec::new(); physical_candidates.len()];
        for (id, edge) in edges.iter().enumerate() {
            if edge.is_admissible() {
                admissible_edges_into_input
                    .entry((edge.consumer_candidate_id, edge.input_index_of_consumer))
                    .or_default()
                    .push(PhysicalEdgeId(id));
                admissible_edges_out_of_candidate[edge.producer_candidate_id.0]
                    .push(PhysicalEdgeId(id));
            }
        }
        Self {
            logical_candidates,
            physical_candidates,
            pruned_candidates,
            edges,
            admissible_edges_into_input,
            admissible_edges_out_of_candidate,
        }
    }

    /// $\mathcal E$: every edge, in (consumer, input) order.
    pub fn edges(&self) -> impl Iterator<Item = (PhysicalEdgeId, PhysicalEdge)> + '_ {
        self.edges
            .iter()
            .enumerate()
            .map(|(id, edge)| (PhysicalEdgeId(id), *edge))
    }

    /// Edge `id` of $\mathcal E$.
    pub fn edge(&self, id: PhysicalEdgeId) -> &PhysicalEdge {
        &self.edges[id.0]
    }

    /// $P^{\mathrm{adm}}_{c,i}$: the admissible edges into input
    /// `input_index_of_consumer` of `consumer`
    /// ($\alpha^{\log} = \alpha^{\mathrm{phys}} = 1$).
    pub fn admissible_edges_into(
        &self,
        consumer: PhysicalCandidateId,
        input_index_of_consumer: usize,
    ) -> &[PhysicalEdgeId] {
        self.admissible_edges_into_input
            .get(&(consumer, input_index_of_consumer))
            .map_or(&[], Vec::as_slice)
    }

    /// The admissible edges out of `producer`: every consumer that may share it.
    pub fn admissible_edges_out_of(&self, producer: PhysicalCandidateId) -> &[PhysicalEdgeId] {
        &self.admissible_edges_out_of_candidate[producer.0]
    }
}

/// The output references of `query_roots`: each operator reference, in
/// order (a scalar root's in `map_operator_refs` order), with the index of
/// the query whose root it is.
pub fn output_references<C: Clone>(query_roots: &[QueryRoot<C>]) -> Vec<(usize, C)> {
    let mut references = Vec::new();
    for (query, root) in query_roots.iter().enumerate() {
        match root {
            QueryRoot::Operator(reference) => references.push((query, reference.clone())),
            QueryRoot::Scalar(expression) => {
                expression.map_operator_refs(&mut |reference| {
                    references.push((query, reference.clone()))
                });
            }
        }
    }
    references
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::materialized_view::RefreshWindow;
    use crate::ir::operator::operator_properties::{Reduction, Source};
    use crate::ir::scalar::expr_ir::ColumnRef;
    use crate::ir::schema::{
        DataType, Field, FieldDataType, Schema, SketchAlgorithm, SketchParams, SummaryUpdate,
    };
    use crate::ir::{NonASAPOp, TimeRangeKind};
    use crate::physical::StorageMedium;
    use crate::stages::physical::execution_context::EvaluationGrid;
    use crate::stages::{
        CandidateExpression, CandidateForMemoGroup, LogicalProperties, LogicalRequirements, Memo,
        MemoGroupId, OriginOfCandidateForMemoGroup,
    };
    use ExecutionTiming::{IngestionTime, QueryTime};

    /// The KLL build over the scan: not a chain.
    const BUILD: CandidateForMemoGroupId = CandidateForMemoGroupId {
        group: MemoGroupId(1),
        index_in_group: 0,
    };
    /// The chain of tumbling windows 5 s wide over the scan.
    const WINDOWS_5S: CandidateForMemoGroupId = CandidateForMemoGroupId {
        group: MemoGroupId(2),
        index_in_group: 0,
    };
    /// The chain of tumbling windows 10 s wide over the scan.
    const WINDOWS_10S: CandidateForMemoGroupId = CandidateForMemoGroupId {
        group: MemoGroupId(3),
        index_in_group: 0,
    };
    /// X = the scan's last 5 minutes (`m[5m]`): a result that depends on
    /// the evaluation time and is not a chain.
    const LAST_5M: CandidateForMemoGroupId = CandidateForMemoGroupId {
        group: MemoGroupId(4),
        index_in_group: 0,
    };

    /// Group 1, KLL states over the scan group 0, groups 2 and 3, chains
    /// of KLL states over it 5 s and 10 s wide, and group 4, the scan's last
    /// 5 minutes, each with its own expression as its one candidate. Built
    /// through `find_or_add_group` and `add_candidate`.
    fn memo() -> Memo {
        let mut memo = Memo::default();
        let scan = memo
            .find_or_add_group(Operator::NonASAP(NonASAPOp::Scan {
                source: Source::TimeSeries {
                    metric: "latency".into(),
                },
                predicates: vec![],
                schema: Schema::with_time_index(
                    vec![
                        Field::plain("ts", DataType::Timestamp, false),
                        Field::plain("value", DataType::Float64, false),
                    ],
                    0,
                    vec![],
                ),
            }))
            .unwrap();
        let kll = FieldDataType::Sketch(SketchAlgorithm::Kll);
        let build = Operator::ASAP(ASAPOp::SummaryAgg {
            child: scan,
            summary_type: kll.clone(),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            filter: None,
        });
        let chain = |tumbling_window_width_ms| {
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                child: scan,
                summary_type: kll.clone(),
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::PerEntity,
                filter: None,
                tumbling_window_width_ms,
            })
        };
        for operator in [build, chain(5_000), chain(10_000)] {
            let group = memo.find_or_add_group(operator.clone()).unwrap();
            let expression = CandidateExpression { operator };
            let candidate = CandidateForMemoGroup {
                logical_properties: LogicalProperties::new(
                    &expression,
                    Some(SketchParams::Kll { k: 269 }),
                )
                .unwrap(),
                expression,
                input_requirements: vec![LogicalRequirements::default()],
                fused_groups: vec![],
                origin: OriginOfCandidateForMemoGroup::Pass1,
            };
            memo.add_candidate(group, candidate).unwrap();
        }
        let operator = Operator::NonASAP(NonASAPOp::TimeRange {
            range: std::time::Duration::from_secs(300),
            kind: TimeRangeKind::Range,
            child: scan,
        });
        let group = memo.find_or_add_group(operator.clone()).unwrap();
        let expression = CandidateExpression { operator };
        let candidate = CandidateForMemoGroup {
            logical_properties: LogicalProperties::new(&expression, None).unwrap(),
            expression,
            input_requirements: vec![LogicalRequirements::default()],
            fused_groups: vec![],
            origin: OriginOfCandidateForMemoGroup::Pass1,
        };
        memo.add_candidate(group, candidate).unwrap();
        memo
    }

    /// Stage 2's candidates over `memo`, with `evaluation_grid_of_group`;
    /// `delivers_on` reads nothing else.
    fn candidates(
        memo: Memo,
        evaluation_grid_of_group: Vec<Option<EvaluationGrid>>,
    ) -> PhysicalASAPMemoCandidates {
        PhysicalASAPMemoCandidates {
            logical_candidates: LogicalASAPMemoCandidates {
                memo,
                edges: vec![],
                pruned: vec![],
                evaluation_grid_of_group,
            },
            physical_candidates: vec![],
            pruned_candidates: vec![],
            edges: vec![],
            admissible_edges_into_input: HashMap::new(),
            admissible_edges_out_of_candidate: vec![],
        }
    }

    fn candidate(
        logical_candidate_id: CandidateForMemoGroupId,
        timing: ExecutionTiming,
        lifetime: Lifetime,
    ) -> PhysicalCandidate {
        PhysicalCandidate {
            logical_candidate_id,
            physical_properties: PhysicalProperties::new(timing, lifetime).unwrap(),
        }
    }

    fn evaluation(period_ms: u64, phase_ms: u64) -> ExecutionContext {
        ExecutionContext::Evaluation {
            grid: EvaluationGrid {
                period_ms,
                phase_ms,
            },
        }
    }

    fn ingestion(width_ms: u64, phase_ms: u64) -> ExecutionContext {
        ExecutionContext::IngestionPipeline(RefreshWindow { width_ms, phase_ms })
    }

    const MEMORY: StorageMedium = StorageMedium::Memory;

    /// A `None` producer runs in the consumer's context and delivers on
    /// every one; a `OneEvaluation` producer only on an evaluation context,
    /// the one evaluation its result belongs to. Neither reads the memo.
    #[test]
    fn delivers_on_by_lifetime_without_windows() {
        let candidates = candidates(Memo::default(), vec![]);
        let unmaterialized = candidate(BUILD, QueryTime, Lifetime::None);
        let one_evaluation = candidate(BUILD, QueryTime, Lifetime::OneEvaluation(MEMORY));
        let materialized = ExecutionContext::Materialized(PhysicalCandidateId(0));
        for (producer, context, delivers) in [
            (&unmaterialized, evaluation(7_000, 3_000), true),
            (&unmaterialized, ingestion(5_000, 0), true),
            (&unmaterialized, materialized, true),
            (&one_evaluation, evaluation(7_000, 3_000), true),
            (&one_evaluation, ingestion(5_000, 0), false),
            (&one_evaluation, materialized, false),
        ] {
            assert_eq!(
                producer.delivers_on(&candidates, &context),
                delivers,
                "{:?} in {context:?}",
                producer.physical_properties
            );
        }
    }

    /// A chain producer with `AcrossEvaluations` of width $w$ (grid at phase
    /// 0) delivers when $w$ divides the context's period (or refresh-window
    /// width) and phase. With phases 0 and 5 s every 10 s, only w = 5 s
    /// serves both.
    #[test]
    fn delivers_on_a_chain_when_its_width_divides_the_context_times() {
        let candidates = candidates(memo(), vec![None; 5]);
        let across_5s = candidate(WINDOWS_5S, QueryTime, Lifetime::AcrossEvaluations(MEMORY));
        let ingestion_10s = candidate(
            WINDOWS_10S,
            IngestionTime,
            Lifetime::AcrossEvaluations(MEMORY),
        );
        for (producer, context, delivers) in [
            // Evaluation grids: w divides period and phase.
            (&across_5s, evaluation(10_000, 0), true),
            (&across_5s, evaluation(10_000, 5_000), true),
            (&across_5s, evaluation(10_000, 2_000), false),
            (&across_5s, evaluation(7_000, 0), false),
            (&ingestion_10s, evaluation(10_000, 0), true),
            (&ingestion_10s, evaluation(10_000, 5_000), false),
            // Refresh windows: w divides width and phase.
            (&across_5s, ingestion(5_000, 0), true),
            (&across_5s, ingestion(10_000, 5_000), true),
            (&across_5s, ingestion(10_000, 2_500), false),
            (&ingestion_10s, ingestion(5_000, 0), false),
        ] {
            assert_eq!(
                producer.delivers_on(&candidates, &context),
                delivers,
                "{:?} {:?} in {context:?}",
                producer.logical_candidate_id,
                producer.physical_properties
            );
        }
    }

    /// A stored result that is not a chain delivers when its group's
    /// evaluation grid holds every time of the context: X =
    /// `sum_over_time(m[5m])`'s range, read every 60 s and every 120 s, is
    /// evaluated on 60 s @ 0, so it serves both but not a 30 s grid or
    /// another phase. Without a known grid it serves none.
    #[test]
    fn delivers_on_a_stored_result_on_its_group_grid() {
        let grid_60s = Some(EvaluationGrid {
            period_ms: 60_000,
            phase_ms: 0,
        });
        let on_grid = candidates(memo(), vec![None, None, None, None, grid_60s]);
        let without_grid = candidates(memo(), vec![None; 5]);
        let x = candidate(LAST_5M, QueryTime, Lifetime::AcrossEvaluations(MEMORY));
        for (candidates, context, delivers) in [
            (&on_grid, evaluation(60_000, 0), true),
            (&on_grid, evaluation(120_000, 0), true),
            (&on_grid, evaluation(120_000, 60_000), true),
            (&on_grid, evaluation(30_000, 0), false),
            (&on_grid, evaluation(60_000, 15_000), false),
            (&on_grid, ingestion(60_000, 0), true),
            (&without_grid, evaluation(60_000, 0), false),
        ] {
            assert_eq!(x.delivers_on(candidates, &context), delivers, "{context:?}");
        }
    }

    /// In `Materialized(v)` a producer delivers on $v$'s own times: a
    /// 5 s chain inside the computation of a 10 s chain (every 10 s) and of
    /// X = the scan's last 5 minutes stored on its 60 s grid, but not a
    /// 10 s chain inside X's computation every 25 s, nor anything
    /// time-dependent inside a build with neither a width nor a grid.
    #[test]
    fn delivers_on_the_own_times_of_a_materialized_computation() {
        let grid = |period_ms| {
            Some(EvaluationGrid {
                period_ms,
                phase_ms: 0,
            })
        };
        let materialized = |grid_of_x| {
            let mut candidates = candidates(memo(), vec![None, None, None, None, grid_of_x]);
            candidates.physical_candidates = vec![
                candidate(WINDOWS_10S, QueryTime, Lifetime::AcrossEvaluations(MEMORY)),
                candidate(LAST_5M, QueryTime, Lifetime::AcrossEvaluations(MEMORY)),
                candidate(BUILD, QueryTime, Lifetime::AcrossEvaluations(MEMORY)),
            ];
            candidates
        };
        let across_5s = candidate(WINDOWS_5S, QueryTime, Lifetime::AcrossEvaluations(MEMORY));
        let across_10s = candidate(
            WINDOWS_10S,
            IngestionTime,
            Lifetime::AcrossEvaluations(MEMORY),
        );
        let in_computation_of = |v| ExecutionContext::Materialized(PhysicalCandidateId(v));
        for (candidates, producer, context, delivers) in [
            (
                materialized(grid(60_000)),
                &across_5s,
                in_computation_of(0),
                true,
            ),
            (
                materialized(grid(60_000)),
                &across_5s,
                in_computation_of(1),
                true,
            ),
            (
                materialized(grid(25_000)),
                &across_10s,
                in_computation_of(1),
                false,
            ),
            (
                materialized(grid(60_000)),
                &across_5s,
                in_computation_of(2),
                false,
            ),
        ] {
            assert_eq!(
                producer.delivers_on(&candidates, &context),
                delivers,
                "{:?} in {context:?}",
                producer.logical_candidate_id
            );
        }
    }

    /// `producer_satisfies_physical_requirements` over every valid producer state and every valid
    /// consumer state: a query-time producer serves only query-time
    /// consumers, an ingestion-time producer that is not materialized only
    /// ingestion-time consumers, and one materialized across evaluations
    /// serves both. Chain widths do not matter: alignment is Stage 3's
    /// per-context check.
    #[test]
    fn producer_satisfies_physical_requirements_over_every_valid_timing_and_lifetime() {
        let states = |medium| {
            [
                candidate(BUILD, QueryTime, Lifetime::None),
                candidate(BUILD, QueryTime, Lifetime::OneEvaluation(medium)),
                candidate(WINDOWS_10S, QueryTime, Lifetime::AcrossEvaluations(medium)),
                candidate(BUILD, IngestionTime, Lifetime::None),
                candidate(
                    WINDOWS_5S,
                    IngestionTime,
                    Lifetime::AcrossEvaluations(medium),
                ),
            ]
        };
        // Rows: producer state; columns: consumer state, in `states` order.
        let expected = [
            [true, true, true, false, false],
            [true, true, true, false, false],
            [true, true, true, false, false],
            [false, false, false, true, true],
            [true, true, true, true, true],
        ];
        for producer_medium in [StorageMedium::Memory, StorageMedium::Disk] {
            for consumer_medium in [StorageMedium::Memory, StorageMedium::Disk] {
                for (producer, row) in states(producer_medium).iter().zip(expected) {
                    for (consumer, satisfies) in states(consumer_medium).iter().zip(row) {
                        assert_eq!(
                            producer_satisfies_physical_requirements(
                                producer,
                                consumer.physical_properties.timing()
                            ),
                            satisfies,
                            "{:?} -> {:?}",
                            producer.physical_properties,
                            consumer.physical_properties
                        );
                    }
                }
            }
        }
    }
}
