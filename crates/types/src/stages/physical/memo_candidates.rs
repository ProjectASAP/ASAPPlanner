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
use crate::stages::{CandidateForMemoGroupId, LogicalASAPMemoCandidates, Memo, PrunedCandidate};

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
    /// an `Evaluation` context, the one evaluation its result belongs to. A
    /// chain producer (`SummaryTumblingWindowChain`) with $\ell$ =
    /// `AcrossEvaluations`, $w$ the chain operator's width in its expression
    /// in `memo`, needs every execution time of the context on its window
    /// grid: $w$ divides an evaluation grid's period and phase, or a refresh
    /// window's width and phase.
    pub fn delivers_on(&self, memo: &Memo, context: &ExecutionContext) -> bool {
        match self.physical_properties.lifetime() {
            Lifetime::None => true,
            Lifetime::OneEvaluation(_) => matches!(context, ExecutionContext::Evaluation { .. }),
            Lifetime::AcrossEvaluations(_) => {
                let Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                    tumbling_window_width_ms,
                    ..
                }) = memo
                    .candidate(self.logical_candidate_id)
                    .expression
                    .operator
                else {
                    todo!("#1207 PR 5: a materialized result of a grid-evaluated group delivers on a context whose times lie on its grid (pending: where the grid is read)")
                };
                let (period_ms, phase_ms) = match context {
                    ExecutionContext::Evaluation { grid } => (grid.period_ms, grid.phase_ms),
                    ExecutionContext::IngestionPipeline(window) => {
                        (window.width_ms, window.phase_ms)
                    }
                    ExecutionContext::Materialized(_) => {
                        todo!("#1207 PR 7: the schedule of a Materialized(v) context (pending: v's candidate is not in the memo)")
                    }
                };
                period_ms.is_multiple_of(tumbling_window_width_ms)
                    && phase_ms.is_multiple_of(tumbling_window_width_ms)
            }
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
        let _ = (
            logical_candidates,
            physical_candidates,
            edges,
            pruned_candidates,
        );
        todo!("#1207 PR 5: index the admissible edges")
    }

    /// $\mathcal E$: every edge, in (consumer, input) order.
    pub fn edges(&self) -> impl Iterator<Item = (PhysicalEdgeId, PhysicalEdge)> + '_ {
        self.edges
            .iter()
            .enumerate()
            .map(|(id, edge)| (PhysicalEdgeId(id), *edge))
    }

    /// $P^{\mathrm{adm}}_{c,i}$: the admissible edges into input
    /// `input_index_of_consumer` of `consumer`
    /// ($\alpha^{\log} = \alpha^{\mathrm{phys}} = 1$).
    pub fn admissible_edges_into(
        &self,
        consumer: PhysicalCandidateId,
        input_index_of_consumer: usize,
    ) -> &[PhysicalEdgeId] {
        let _ = (
            consumer,
            input_index_of_consumer,
            &self.admissible_edges_into_input,
        );
        todo!("#1207 PR 5")
    }

    /// The admissible edges out of `producer`: every consumer that may share it.
    pub fn admissible_edges_out_of(&self, producer: PhysicalCandidateId) -> &[PhysicalEdgeId] {
        let _ = (producer, &self.admissible_edges_out_of_candidate);
        todo!("#1207 PR 5")
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
    use crate::ir::NonASAPOp;
    use crate::physical::StorageMedium;
    use crate::stages::physical::execution_context::EvaluationGrid;
    use crate::stages::{
        CandidateExpression, CandidateForMemoGroup, LogicalProperties, LogicalRequirements,
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

    /// Group 1, KLL states over the scan group 0, and groups 2 and 3, chains
    /// of KLL states over it 5 s and 10 s wide, each with its own expression
    /// as its one candidate. Built through `find_or_add_group` and
    /// `add_candidate` (#1207 PR 2).
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
        memo
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
        let memo = Memo::default();
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
                producer.delivers_on(&memo, &context),
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
        let memo = memo();
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
                producer.delivers_on(&memo, &context),
                delivers,
                "{:?} {:?} in {context:?}",
                producer.logical_candidate_id,
                producer.physical_properties
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
