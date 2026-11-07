//! Stage 2's output: each logical candidate's physical alternatives
//! $\mathcal C$, one per valid $p^{\mathrm{phys}} = (\tau, \ell)$, and
//! the physical edges $\mathcal E$ with $\alpha^{\log}$ and
//! $\alpha^{\mathrm{phys}}$ (#1200, #1201). Stage 3 selects among them;
//! Stage 2 prices nothing.

use std::collections::HashMap;

use super::execution_context::ExecutionContext;
use super::graph::PhysicalCandidateId;
use crate::ir::QueryRoot;
use crate::ir::{ASAPOp, Operator};
use crate::physical::{ExecutionTiming, Lifetime, PhysicalProperties};
use crate::stages::{CandidateForMemoGroupId, LogicalASAPMemoCandidates, Memo, PrunedCandidate};

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
    /// concepts doc's alignment table, section 2). True for $\ell$ = None,
    /// and for `OneEvaluation` in the same context: such a producer runs in
    /// the consumer's own context ($x_{\kappa,v}$, Stage 3). A chain producer
    /// (`SummaryTumblingWindowChain`) with $\ell$ = `AcrossEvaluations`, $w$
    /// its chain operator's width in `memo`, needs every evaluation time of
    /// the context on its grid: $w$ divides the context's
    /// [`evaluation_grid_ms`](crate::workload::QueryRecurrence::evaluation_grid_ms); false when that is `None`
    /// (the times are not known). `AcrossEvaluations` is offered only to
    /// chains, so this is false for any other `AcrossEvaluations` producer.
    pub fn delivers_on(&self, memo: &Memo, context: &ExecutionContext) -> bool {
        match self.physical_properties.lifetime() {
            Lifetime::None | Lifetime::OneEvaluation(_) => true,
            Lifetime::AcrossEvaluations(_) => {
                let Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                    tumbling_window_width_ms,
                    ..
                }) = memo
                    .candidate(self.logical_candidate_id)
                    .expression
                    .operator
                else {
                    return false;
                };
                match context {
                    ExecutionContext::Evaluation {
                        recurrence,
                        range_evaluation,
                    } => recurrence
                        .evaluation_grid_ms(*range_evaluation)
                        .is_some_and(|grid_ms| grid_ms.is_multiple_of(tumbling_window_width_ms)),
                    ExecutionContext::IngestionPipeline(_) | ExecutionContext::Materialized(_) => {
                        todo!("#1207 PR 7: delivery into an ingestion or materialized context")
                    }
                }
            }
        }
    }
}

/// $\alpha^{\mathrm{phys}}_{u,c,i}$: timing and lifetime only (the concepts
/// doc's table, section 2). Alignment is checked per context in Stage 3 with
/// [`PhysicalCandidate::delivers_on`].
pub fn producer_satisfies_physical_requirements(
    producer: &PhysicalCandidate,
    consumer: &PhysicalCandidate,
) -> bool {
    use ExecutionTiming::{IngestionTime, QueryTime};
    let consumer_timing = consumer.physical_properties.timing();
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
    use crate::ir::operator::operator_properties::{Reduction, Source};
    use crate::ir::scalar::expr_ir::ColumnRef;
    use crate::ir::schema::{
        DataType, Field, FieldDataType, GroupingStrategy, Schema, SketchAlgorithm, SketchParams,
        SummaryUpdate,
    };
    use crate::ir::NonASAPOp;
    use crate::physical::StorageMedium;
    use crate::stages::{
        CandidateExpression, CandidateForMemoGroup, LogicalProperties, LogicalRequirements,
        MemoGroupId, OriginOfCandidateForMemoGroup,
    };
    use crate::workload::{
        DemandEstimate, QueryRecurrence, RangeEvaluation, Rate, RepeatedDemand, RepetitionInterval,
        TimestampMs,
    };
    use ExecutionTiming::{IngestionTime, QueryTime};

    /// The KLL build over the scan: not a chain.
    const UNWINDOWED: CandidateForMemoGroupId = CandidateForMemoGroupId {
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
        let kll = FieldDataType::Sketch(SketchAlgorithm::Kll, GroupingStrategy::default());
        let build = Operator::ASAP(ASAPOp::SummaryAgg {
            child: scan,
            family: kll.clone(),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            grouping: GroupingStrategy::default(),
            filter: None,
        });
        let chain = |tumbling_window_width_ms| {
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                child: scan,
                family: kll.clone(),
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::PerEntity,
                grouping: GroupingStrategy::default(),
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
                    Some(SketchParams::Kll {
                        k: 269,
                        failure_probability: None,
                    }),
                )
                .unwrap(),
                expression,
                input_requirements: vec![LogicalRequirements::new(None)],
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

    fn evaluation(
        recurrence: QueryRecurrence,
        range_evaluation: Option<RangeEvaluation>,
    ) -> ExecutionContext {
        ExecutionContext::Evaluation {
            recurrence,
            range_evaluation,
        }
    }

    fn every(interval_ms: u32, phase_ms: u64) -> QueryRecurrence {
        QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
            interval: RepetitionInterval(interval_ms),
            evaluation_phase: TimestampMs(phase_ms),
        })
    }

    const MEMORY: StorageMedium = StorageMedium::Memory;

    /// `delivers_on` follows the alignment table, one case per recurrence: a
    /// chain producer with `AcrossEvaluations` of width $w$ (the chain
    /// operator's, grid at phase 0) needs $w$ to divide the context's
    /// evaluation grid (interval, range step and phase), each scheduled time,
    /// or a one-off query's time, and is refused when the times are not
    /// known (a fixed interval without a phase included); a `None` or
    /// `OneEvaluation` producer runs in the consumer's context and delivers
    /// on any; an `AcrossEvaluations` producer that is not a chain is not
    /// offered.
    #[test]
    fn delivers_on_follows_the_alignment_table() {
        let memo = memo();
        let across_5s = candidate(WINDOWS_5S, QueryTime, Lifetime::AcrossEvaluations(MEMORY));
        let ingestion_10s = candidate(
            WINDOWS_10S,
            IngestionTime,
            Lifetime::AcrossEvaluations(MEMORY),
        );
        let across_unwindowed =
            candidate(UNWINDOWED, QueryTime, Lifetime::AcrossEvaluations(MEMORY));
        let unmaterialized = candidate(UNWINDOWED, QueryTime, Lifetime::None);
        let one_evaluation = candidate(WINDOWS_5S, QueryTime, Lifetime::OneEvaluation(MEMORY));
        let hour_by_minute = Some(RangeEvaluation {
            step_ms: 60_000,
            span_ms: 3_600_000,
        });
        let hour_by_7s = Some(RangeEvaluation {
            step_ms: 7_000,
            span_ms: 3_600_000,
        });
        let scheduled = |times: &[u64]| {
            QueryRecurrence::Repeated(RepeatedDemand::Scheduled(
                times.iter().copied().map(TimestampMs).collect(),
            ))
        };
        let one_off = |execute_at| QueryRecurrence::OneTime {
            invocations: 3,
            execute_at,
        };
        let estimated = QueryRecurrence::Repeated(RepeatedDemand::EstimatedRate(DemandEstimate {
            expected_rate: Rate(0.1),
            observed_at: None,
            valid_for: None,
        }));
        let without_phase =
            QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(10_000)));
        for (producer, context, delivers) in [
            // FixedIntervalAt: w divides gcd(interval, step, phase).
            (across_5s, evaluation(every(10_000, 0), None), true),
            (across_5s, evaluation(every(10_000, 5_000), None), true),
            (across_5s, evaluation(every(10_000, 2_000), None), false),
            (across_5s, evaluation(every(7_000, 0), None), false),
            (
                across_5s,
                evaluation(every(10_000, 0), hour_by_minute),
                true,
            ),
            (across_5s, evaluation(every(10_000, 0), hour_by_7s), false),
            // Phases 0 and 5 s with w = 10 s: only w = 5 s serves both.
            (ingestion_10s, evaluation(every(10_000, 0), None), true),
            (ingestion_10s, evaluation(every(10_000, 5_000), None), false),
            // FixedInterval without a phase: the times are not known.
            (across_5s, evaluation(without_phase.clone(), None), false),
            // Scheduled: w divides every time.
            (
                across_5s,
                evaluation(scheduled(&[10_000, 25_000]), None),
                true,
            ),
            (
                across_5s,
                evaluation(scheduled(&[10_000, 27_000]), None),
                false,
            ),
            // OneTime: w divides its time; without a time, not known.
            (
                across_5s,
                evaluation(one_off(Some(TimestampMs(60_000))), None),
                true,
            ),
            (
                across_5s,
                evaluation(one_off(Some(TimestampMs(61_000))), None),
                false,
            ),
            (across_5s, evaluation(one_off(None), None), false),
            // EstimatedRate and Unknown: not known.
            (across_5s, evaluation(estimated.clone(), None), false),
            (across_5s, evaluation(QueryRecurrence::Unknown, None), false),
            (across_unwindowed, evaluation(every(10_000, 0), None), false),
            (
                unmaterialized,
                evaluation(without_phase.clone(), None),
                true,
            ),
            (one_evaluation, evaluation(estimated.clone(), None), true),
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
                candidate(UNWINDOWED, QueryTime, Lifetime::None),
                candidate(UNWINDOWED, QueryTime, Lifetime::OneEvaluation(medium)),
                candidate(WINDOWS_10S, QueryTime, Lifetime::AcrossEvaluations(medium)),
                candidate(UNWINDOWED, IngestionTime, Lifetime::None),
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
                            producer_satisfies_physical_requirements(producer, consumer),
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
