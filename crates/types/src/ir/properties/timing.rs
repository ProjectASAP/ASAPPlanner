//! Execution timing: written into every node from a materialization
//! assignment, then validated against each operator's kind and its consuming
//! edges.
//!
//! The logical DAG carries no timing. Materialization decides per summary
//! state whether it is maintained at ingestion time or computed at query
//! time; [`MaterializationAssignment`] records that choice per `SummaryAgg`
//! and [`apply_materialization_timings`] expands it into a timing on every node:
//!
//! - a node of fixed kind takes its kind's timing (`SummaryEstimate` and
//!   `EvaluatePopulation` run at query time, `MaintainPopulation` at ingestion
//!   time);
//! - a `SummaryAgg` takes the assignment's timing (default: query time, until
//!   Stage 2 materialization (#509) chooses otherwise), unless something below
//!   it can only exist at query time;
//! - every other node runs when its consumer runs: everything that feeds a
//!   maintained state runs at ingestion time, everything above a evaluation at
//!   query time.
//!
//! A node reached from two consumers that need different timings cannot be
//! executed once for both; [`split_shared_by_phase`] copies such a sub-DAG
//! for one side before the assignment is applied, and the pass itself
//! rejects a conflict it still finds.
//!
//! ## Edge rules (checked after the write)
//!
//! | Consumer | Accepts from an input |
//! |---|---|
//! | `SummaryAgg.child` | Rows, or exact-accumulator state, never a query-time value when the state is maintained |
//! | `SummaryEstimate.summary_input` | Summary state at either phase |
//! | `FinalizeExactAccumulator.child` | Exact-accumulator state |
//! | `EvaluatePopulation.child` | A `MaintainPopulation` at ingestion time |
//! | `MaintainPopulation.child` | Ingestion-time rows matching the population's input |
//! | any `NonASAP` consumer | Rows (or exact-accumulator state for a projection-like operator) at the consumer's own timing; ingestion work never reads a query-time value |

use std::collections::HashMap;
use std::rc::Rc;

use crate::ir::operator::asap::ASAPOp;
use crate::ir::operator::node::{Operator, OperatorNode};
use crate::ir::operator::non_asap::NonASAPOp;
use crate::ir::operator::operator_properties::BinaryOpKind;
use crate::ir::properties::execution::{
    DataPrimitive, ExecutionDataState, ExecutionDataStateError, ExecutionTiming,
};
use crate::ir::schema::{DataType, FieldDataType, Schema};

/// The per-state materialization choice: for each `SummaryAgg` node (by
/// identity), whether its state is maintained at ingestion time or computed
/// at query time. A state absent from the map takes the assignment's default.
/// `Default` is [`Self::all_query_time`]: nothing is materialized until Stage 2
/// materialization (#509) decides otherwise.
#[derive(Debug, Clone, Default)]
pub struct MaterializationAssignment {
    summary_timings: HashMap<*const OperatorNode, ExecutionTiming>,
    default_timing: ExecutionTiming,
}

impl MaterializationAssignment {
    /// Every summary state computed at query time.
    pub fn all_query_time() -> Self {
        Self::default()
    }

    /// Every summary state maintained at ingestion time.
    pub fn all_ingestion_time() -> Self {
        Self {
            summary_timings: HashMap::new(),
            default_timing: ExecutionTiming::IngestionTime,
        }
    }

    pub fn set(&mut self, summary: &Rc<OperatorNode>, timing: ExecutionTiming) {
        self.summary_timings.insert(Rc::as_ptr(summary), timing);
    }

    pub fn summary_timing(&self, summary: &Rc<OperatorNode>) -> ExecutionTiming {
        self.summary_timings
            .get(&Rc::as_ptr(summary))
            .copied()
            .unwrap_or(self.default_timing)
    }
}

/// Memo of one [`apply_materialization_timings`] pass: `input node → timed node`,
/// shared by every root of a workload so a node shared by two roots stays
/// one `Rc`. Re-reaching a node with a different timing is a conflict.
#[derive(Default)]
pub struct TimingMemo {
    done: HashMap<*const OperatorNode, Rc<OperatorNode>>,
}

impl TimingMemo {
    pub fn new() -> Self {
        Self::default()
    }

    /// The timed node produced for `input`, if the pass has reached it.
    pub fn timed(&self, input: &Rc<OperatorNode>) -> Option<&Rc<OperatorNode>> {
        self.done.get(&Rc::as_ptr(input))
    }
}

/// The data state a timed node's output carries.
pub fn data_state(node: &OperatorNode) -> Option<ExecutionDataState> {
    Some(ExecutionDataState {
        timing: node.timing?,
        primitive: match &node.operator {
            Operator::ASAP(op) if op.produced_state().is_some() => DataPrimitive::SummaryState,
            Operator::ASAP(ASAPOp::MaintainPopulation { .. }) => DataPrimitive::SummaryState,
            _ => DataPrimitive::Raw,
        },
    })
}

/// Whether the sub-DAG below `node` contains a node that can only run at
/// query time (a evaluation), which forces every consumer above it to query
/// time as well.
fn forces_query_time(node: &OperatorNode, seen: &mut HashMap<*const OperatorNode, bool>) -> bool {
    let key = node as *const OperatorNode;
    if let Some(&cached) = seen.get(&key) {
        return cached;
    }
    let forced = match &node.operator {
        _ if node.timing == Some(ExecutionTiming::QueryTime) => true,
        Operator::ASAP(ASAPOp::SummaryEstimate { .. })
        | Operator::ASAP(ASAPOp::EvaluatePopulation { .. }) => true,
        Operator::NonASAP(NonASAPOp::BinaryOp { lhs, rhs, .. })
            if node
                .schema
                .fields
                .iter()
                .any(|f| f.name == crate::ir::schema::PROMQL_SERIES_IDENTITY)
                && per_series_rows(lhs).is_none_or(|rows| per_series_rows(rhs) != Some(rows)) =>
        {
            true
        }
        _ => node
            .children()
            .iter()
            .any(|child| forces_query_time(child, seen)),
    };
    seen.insert(key, forced);
    forced
}

/// Write the timings of `assignment` into every node reachable from `root`,
/// top-down, then validate every edge. Returns the timed copy of `root`;
/// `memo` carries the sharing across the roots of one workload.
pub fn apply_materialization_timings(
    root: &Rc<OperatorNode>,
    assignment: &MaterializationAssignment,
    memo: &mut TimingMemo,
) -> Result<Rc<OperatorNode>, ExecutionDataStateError> {
    let mut forced = HashMap::new();
    let timed = write(
        root,
        ExecutionTiming::QueryTime,
        assignment,
        memo,
        &mut forced,
    )?;
    if timed.timing == Some(ExecutionTiming::IngestionTime)
        && data_state(&timed).map(|s| s.primitive) == Some(DataPrimitive::Raw)
    {
        return Err(ExecutionDataStateError::MaintenanceRowsAtRoot);
    }
    validate(&timed, &mut HashMap::new())?;
    Ok(timed)
}

/// Validate the sub-DAG below `root` with every summary maintained at
/// ingestion time ([`MaterializationAssignment::all_ingestion_time`]) and
/// `root` consumed at `root_timing`. For planning-time legality checks of a
/// candidate before it is assembled into a workload DAG: a candidate must stay
/// executable if materialization later maintains its states. Nothing is kept.
pub fn validate_maintained(
    root: &Rc<OperatorNode>,
    root_timing: ExecutionTiming,
) -> Result<(), ExecutionDataStateError> {
    let assignment = MaterializationAssignment::all_ingestion_time();
    let mut memo = TimingMemo::new();
    let mut forced = HashMap::new();
    let timed = write(root, root_timing, &assignment, &mut memo, &mut forced)?;
    validate(&timed, &mut HashMap::new())
}

/// The data state `node` produces with every summary maintained at ingestion
/// time when its consumer runs at `consumer` — the planning-time answer to
/// "what does this candidate's output look like", consistent with
/// [`validate_maintained`].
pub fn planned_data_state(
    node: &Rc<OperatorNode>,
    consumer: ExecutionTiming,
) -> ExecutionDataState {
    let mut forced = HashMap::new();
    let timing = own_timing(
        node,
        consumer,
        &MaterializationAssignment::all_ingestion_time(),
        &mut forced,
    );
    ExecutionDataState {
        timing,
        primitive: match &node.operator {
            Operator::ASAP(op) if op.produced_state().is_some() => DataPrimitive::SummaryState,
            Operator::ASAP(ASAPOp::MaintainPopulation { .. }) => DataPrimitive::SummaryState,
            _ => DataPrimitive::Raw,
        },
    }
}

/// The timing `node` takes when its consumer runs at `consumer`.
fn own_timing(
    node: &Rc<OperatorNode>,
    consumer: ExecutionTiming,
    assignment: &MaterializationAssignment,
    forced: &mut HashMap<*const OperatorNode, bool>,
) -> ExecutionTiming {
    // A placement fixed when the candidate was built (an exact-state read
    // boundary that must run at query time, or one that feeds maintenance)
    // is honored; a conflicting consumer is rejected by validation.
    if let Some(placed) = node.timing {
        return placed;
    }
    match &node.operator {
        Operator::ASAP(ASAPOp::SummaryEstimate { .. })
        | Operator::ASAP(ASAPOp::EvaluatePopulation { .. }) => ExecutionTiming::QueryTime,
        Operator::ASAP(ASAPOp::MaintainPopulation { .. }) => ExecutionTiming::IngestionTime,
        Operator::ASAP(ASAPOp::SummaryAgg { child, .. }) => {
            if forces_query_time(child, forced) {
                ExecutionTiming::QueryTime
            } else {
                assignment.summary_timing(node)
            }
        }
        _ => consumer,
    }
}

fn write(
    node: &Rc<OperatorNode>,
    consumer: ExecutionTiming,
    assignment: &MaterializationAssignment,
    memo: &mut TimingMemo,
    forced: &mut HashMap<*const OperatorNode, bool>,
) -> Result<Rc<OperatorNode>, ExecutionDataStateError> {
    let timing = own_timing(node, consumer, assignment, forced);
    if let Some(done) = memo.done.get(&Rc::as_ptr(node)) {
        let previous = done.timing.expect("memoized node is timed");
        if previous != timing {
            return Err(ExecutionDataStateError::ConflictingTiming {
                first: ExecutionDataState {
                    timing: previous,
                    primitive: data_state(done).map_or(DataPrimitive::Raw, |s| s.primitive),
                },
                second: ExecutionDataState {
                    timing,
                    primitive: data_state(done).map_or(DataPrimitive::Raw, |s| s.primitive),
                },
            });
        }
        return Ok(Rc::clone(done));
    }
    let mut error = None;
    let operator =
        node.operator.map_children(
            |child| match write(child, timing, assignment, memo, forced) {
                Ok(timed) => timed,
                Err(e) => {
                    error.get_or_insert(e);
                    Rc::clone(child)
                }
            },
        );
    if let Some(e) = error {
        return Err(e);
    }
    let timed = Rc::new(OperatorNode {
        operator,
        result_kind: node.result_kind,
        schema: node.schema.clone(),
        guarantee: node.guarantee.clone(),
        timing: Some(timing),
        // Timing copies the same logical sub-DAG; its observations are unchanged.
        coverage: node.coverage.clone(),
    });
    memo.done.insert(Rc::as_ptr(node), Rc::clone(&timed));
    Ok(timed)
}

fn state_of(node: &OperatorNode) -> ExecutionDataState {
    data_state(node).expect("timed node")
}

/// Check every edge below `node` against the module-level rules.
fn validate(
    node: &Rc<OperatorNode>,
    seen: &mut HashMap<*const OperatorNode, ()>,
) -> Result<(), ExecutionDataStateError> {
    if seen.insert(Rc::as_ptr(node), ()).is_some() {
        return Ok(());
    }
    let timing = node.timing.expect("timed node");
    match &node.operator {
        Operator::ASAP(op) => validate_asap(node, op, timing)?,
        Operator::NonASAP(op) => validate_non_asap(node, op, timing)?,
    }
    for child in node.children() {
        validate(child, seen)?;
    }
    Ok(())
}

fn is_exact_accumulator_state(schema: &Schema) -> Result<(), ExecutionDataStateError> {
    for field in &schema.fields {
        match &field.dtype {
            FieldDataType::Plain(_) | FieldDataType::ExactAggregate(..) => {}
            other => {
                return Err(ExecutionDataStateError::UnsupportedStateComposition {
                    family: format!("{other:?}"),
                })
            }
        }
    }
    Ok(())
}

fn validate_asap(
    node: &OperatorNode,
    op: &ASAPOp,
    timing: ExecutionTiming,
) -> Result<(), ExecutionDataStateError> {
    match op {
        ASAPOp::SummaryAgg { child, .. } => {
            let avail = state_of(child);
            match avail {
                ExecutionDataState::INGESTION_ROWS | ExecutionDataState::QUERY_ROWS => {}
                s if s.primitive == DataPrimitive::SummaryState => {
                    is_exact_accumulator_state(&child.schema)?
                }
                other => {
                    return Err(ExecutionDataStateError::EvaluationUnderMaintenance {
                        edge: "SummaryAgg.child",
                        child: other,
                    })
                }
            }
            if timing == ExecutionTiming::IngestionTime
                && avail.timing == ExecutionTiming::QueryTime
            {
                return Err(ExecutionDataStateError::EvaluationUnderMaintenance {
                    edge: "SummaryAgg.child",
                    child: avail,
                });
            }
            Ok(())
        }
        ASAPOp::SummaryEstimate { summary_input, .. } => {
            let s = state_of(summary_input);
            if s.primitive != DataPrimitive::SummaryState {
                return Err(ExecutionDataStateError::IllegalChildDataState {
                    edge: "SummaryEstimate.summary_input",
                    child: s,
                });
            }
            if timing != ExecutionTiming::QueryTime {
                return Err(ExecutionDataStateError::IllegalChildDataState {
                    edge: "SummaryEstimate",
                    child: state_of(node),
                });
            }
            Ok(())
        }
        ASAPOp::FinalizeExactAccumulator { child } => {
            let s = state_of(child);
            if s.primitive != DataPrimitive::SummaryState
                || is_exact_accumulator_state(&child.schema).is_err()
                || (timing == ExecutionTiming::IngestionTime && s.timing != timing)
            {
                return Err(ExecutionDataStateError::IllegalChildDataState {
                    edge: "FinalizeExactAccumulator.child",
                    child: s,
                });
            }
            Ok(())
        }
        ASAPOp::MaintainPopulation { child, population } => {
            let valid = population.matches_node(child)
                && state_of(child)
                    == ExecutionDataState {
                        timing,
                        primitive: DataPrimitive::Raw,
                    };
            if !valid {
                return Err(ExecutionDataStateError::InvalidMaintainedPopulation);
            }
            Ok(())
        }
        ASAPOp::EvaluatePopulation { child, evaluation } => {
            let valid = timing == ExecutionTiming::QueryTime
                && matches!(
                    &child.operator,
                    Operator::ASAP(ASAPOp::MaintainPopulation { population, .. })
                        if population.supports(evaluation)
                            && child.timing.is_some()
                );
            if !valid {
                return Err(ExecutionDataStateError::InvalidMaintainedPopulation);
            }
            Ok(())
        }
        ASAPOp::SummaryMerge { .. }
        | ASAPOp::SummarySubtract { .. }
        | ASAPOp::SummaryDelete { .. }
        | ASAPOp::SummaryJoin { .. }
        | ASAPOp::Extension { .. } => Err(ExecutionDataStateError::UnimplementedOperator {
            operator: op.kind_name(),
        }),
    }
}

fn check_plain_or_exact_values(input: &Schema) -> Result<(), ExecutionDataStateError> {
    for field in &input.fields {
        if !matches!(
            field.dtype,
            FieldDataType::Plain(_) | FieldDataType::ExactAggregate(..)
        ) {
            return Err(ExecutionDataStateError::NonPlainOperand {
                column: field.name.clone(),
                dtype: format!("{:?}", field.dtype),
            });
        }
    }
    Ok(())
}

fn check_all_plain(input: &Schema) -> Result<(), ExecutionDataStateError> {
    for field in &input.fields {
        if !field.is_plain() {
            return Err(ExecutionDataStateError::NonPlainOperand {
                column: field.name.clone(),
                dtype: format!("{:?}", field.dtype),
            });
        }
    }
    Ok(())
}

fn validate_non_asap(
    node: &OperatorNode,
    op: &NonASAPOp,
    timing: ExecutionTiming,
) -> Result<(), ExecutionDataStateError> {
    // Every input is rows at this node's own timing. Ingestion work never
    // reads a query-time value; exact-accumulator state may pass through
    // the projection-like operators unchanged.
    for child in op.children() {
        let s = state_of(child);
        let passes_state = matches!(
            op,
            NonASAPOp::Project { .. }
                | NonASAPOp::Filter { .. }
                | NonASAPOp::Sort { .. }
                | NonASAPOp::Limit { .. }
        ) && s.primitive == DataPrimitive::SummaryState
            && is_exact_accumulator_state(&child.schema).is_ok();
        if s.timing != timing || (s.primitive != DataPrimitive::Raw && !passes_state) {
            return Err(ExecutionDataStateError::IllegalChildDataState {
                edge: op.kind_name(),
                child: s,
            });
        }
    }
    match op {
        NonASAPOp::Project { child, .. }
        | NonASAPOp::Filter { child, .. }
        | NonASAPOp::Sort { child, .. }
        | NonASAPOp::Limit { child, .. } => check_plain_or_exact_values(&child.schema)?,
        NonASAPOp::Aggregate {
            reduction,
            measures,
            child,
            ..
        } => {
            let mut referenced: Vec<usize> = reduction
                .group_keys()
                .map(|keys| keys.keys().to_vec())
                .unwrap_or_default();
            for m in measures {
                referenced.extend(m.input_cols());
            }
            let implicit = measures.iter().any(|m| m.input_cols().is_empty());
            for (i, field) in child.schema.fields.iter().enumerate() {
                if (implicit || referenced.contains(&i)) && !field.is_plain() {
                    return Err(ExecutionDataStateError::NonPlainOperand {
                        column: field.name.clone(),
                        dtype: format!("{:?}", field.dtype),
                    });
                }
            }
        }
        NonASAPOp::BinaryOp {
            operator, lhs, rhs, ..
        } => {
            let is_div = matches!(
                operator.kind,
                BinaryOpKind::Arithmetic(crate::ir::scalar::ArithmeticOpKind::Div)
            );
            if (operator.checked_relative_division && operator.checked_finite_division)
                || ((operator.checked_relative_division || operator.checked_finite_division)
                    && (timing != ExecutionTiming::QueryTime || !is_div))
            {
                return Err(ExecutionDataStateError::InvalidCheckedDivision);
            }
            if timing == ExecutionTiming::IngestionTime {
                let plain_float_or_ts = |schema: &Schema| {
                    schema.fields.iter().all(|field| {
                        !field.nullable
                            && (matches!(
                                field.dtype,
                                FieldDataType::Plain(DataType::Float64 | DataType::Timestamp)
                            ) || (field.name == crate::ir::schema::PROMQL_SERIES_IDENTITY
                                && field.dtype == FieldDataType::Plain(DataType::Utf8)))
                    })
                };
                let float_count = node
                    .schema
                    .fields
                    .iter()
                    .filter(|f| matches!(f.dtype, FieldDataType::Plain(DataType::Float64)))
                    .count();
                if operator.vector_match.is_some()
                    || !matches!(operator.kind, BinaryOpKind::Arithmetic(_))
                    || lhs.schema != rhs.schema
                    || lhs.schema != node.schema
                    || node
                        .schema
                        .fields
                        .iter()
                        .filter(|f| f.name == crate::ir::schema::PROMQL_SERIES_IDENTITY)
                        .count()
                        > 1
                    || (node
                        .schema
                        .fields
                        .iter()
                        .any(|f| f.name == crate::ir::schema::PROMQL_SERIES_IDENTITY)
                        && per_series_rows(lhs)
                            .is_none_or(|rows| per_series_rows(rhs) != Some(rows)))
                    || !plain_float_or_ts(&node.schema)
                    || float_count != 1
                {
                    return Err(ExecutionDataStateError::InvalidMaintenanceBinary);
                }
            }
        }
        _ => {
            for child in op.children() {
                check_all_plain(&child.schema)?;
            }
        }
    }
    Ok(())
}

/// Copy, for one consumer, every sub-DAG that `assignment` would reach with
/// two different timings, so that a workload whose CSE shared a `Scan`
/// between an ingestion-time summary and a query-time computation can still
/// be timed. Only the conflicting sub-DAGs are copied; a sub-DAG reached with
/// one timing stays one `Rc`. Returns the (possibly rewritten) root.
pub fn split_shared_by_phase(
    root: &Rc<OperatorNode>,
    assignment: &MaterializationAssignment,
) -> Rc<OperatorNode> {
    // First pass: the set of timings each node is reached with.
    let mut reached: HashMap<*const OperatorNode, Vec<ExecutionTiming>> = HashMap::new();
    let mut forced = HashMap::new();
    fn collect(
        node: &Rc<OperatorNode>,
        consumer: ExecutionTiming,
        assignment: &MaterializationAssignment,
        reached: &mut HashMap<*const OperatorNode, Vec<ExecutionTiming>>,
        forced: &mut HashMap<*const OperatorNode, bool>,
    ) {
        let timing = own_timing(node, consumer, assignment, forced);
        let entry = reached.entry(Rc::as_ptr(node)).or_default();
        if entry.contains(&timing) {
            return;
        }
        entry.push(timing);
        for child in node.children() {
            collect(child, timing, assignment, reached, forced);
        }
    }
    collect(
        root,
        ExecutionTiming::QueryTime,
        assignment,
        &mut reached,
        &mut forced,
    );
    if reached.values().all(|timings| timings.len() <= 1) {
        return Rc::clone(root);
    }
    // Second pass: rebuild, giving each (node, timing) pair its own copy.
    let mut copies: HashMap<(*const OperatorNode, ExecutionTiming), Rc<OperatorNode>> =
        HashMap::new();
    fn rebuild(
        node: &Rc<OperatorNode>,
        consumer: ExecutionTiming,
        assignment: &MaterializationAssignment,
        reached: &HashMap<*const OperatorNode, Vec<ExecutionTiming>>,
        copies: &mut HashMap<(*const OperatorNode, ExecutionTiming), Rc<OperatorNode>>,
        forced: &mut HashMap<*const OperatorNode, bool>,
    ) -> Rc<OperatorNode> {
        let timing = own_timing(node, consumer, assignment, forced);
        let key = (Rc::as_ptr(node), timing);
        if let Some(done) = copies.get(&key) {
            return Rc::clone(done);
        }
        let conflicted = reached
            .get(&Rc::as_ptr(node))
            .is_some_and(|timings| timings.len() > 1);
        let mut changed = conflicted;
        let operator = node.operator.map_children(|child| {
            let rebuilt = rebuild(child, timing, assignment, reached, copies, forced);
            changed |= !Rc::ptr_eq(&rebuilt, child);
            rebuilt
        });
        let out = if changed {
            Rc::new(OperatorNode {
                operator,
                result_kind: node.result_kind,
                schema: node.schema.clone(),
                guarantee: node.guarantee.clone(),
                timing: node.timing,
                coverage: node.coverage.clone(),
            })
        } else {
            Rc::clone(node)
        };
        copies.insert(key, Rc::clone(&out));
        out
    }
    rebuild(
        root,
        ExecutionTiming::QueryTime,
        assignment,
        &reached,
        &mut copies,
        &mut forced,
    )
}

/// Maintenance arithmetic needs the same per-series population on both sides.
fn per_series_rows(node: &OperatorNode) -> Option<&OperatorNode> {
    use crate::ir::schema::ExactKind;
    match &node.operator {
        Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child }) => match &child.operator {
            Operator::ASAP(ASAPOp::SummaryAgg {
                child,
                family: FieldDataType::ExactAggregate(ExactKind::Sum | ExactKind::Count, _),
                reduction: crate::ir::operator::Reduction::PerEntity,
                filter: None,
                ..
            }) => Some(child),
            _ => None,
        },
        Operator::NonASAP(NonASAPOp::BinaryOp { lhs, rhs, .. }) => {
            let rows = per_series_rows(lhs)?;
            (per_series_rows(rhs) == Some(rows)).then_some(rows)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::operator::agg_intent::AggIntent;
    use crate::ir::operator::non_asap::NonASAPOp;
    use crate::ir::operator::operator_properties::{Reduction, Source};
    use crate::ir::scalar::ColumnRef;
    use crate::ir::schema::state_type::{
        ExactKind, ExactParams, GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams,
        SketchStatistic, SummaryUpdate,
    };
    use crate::ir::schema::Field;

    fn scan_with(fields: Vec<Field>) -> Rc<OperatorNode> {
        OperatorNode::new_shared(crate::ir::Operator::NonASAP(NonASAPOp::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(fields, 0, vec![]),
        }))
        .unwrap()
    }

    fn scan() -> Rc<OperatorNode> {
        scan_with(vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("value", DataType::Float64, false),
            Field::plain("zone", DataType::Utf8, true),
        ])
    }

    fn kll() -> FieldDataType {
        FieldDataType::Sketch(
            SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 200 }),
            GroupingStrategy::default(),
        )
    }

    fn exact_sum() -> FieldDataType {
        FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum)
    }

    fn agg(child: Rc<OperatorNode>, family: FieldDataType) -> Rc<OperatorNode> {
        std::rc::Rc::new(
            OperatorNode::with_schema(
                crate::ir::Operator::ASAP(ASAPOp::SummaryAgg {
                    child,
                    family: family.clone(),
                    input: SummaryUpdate::column(ColumnRef::SampleValue),
                    reduction: Reduction::by(vec![]),
                    grouping: GroupingStrategy::default(),
                    filter: None,
                }),
                Schema::lifted(vec![Field::new("state", family, false)], None),
            )
            .with_guarantee(None),
        )
    }

    fn estimate(child: Rc<OperatorNode>) -> Rc<OperatorNode> {
        std::rc::Rc::new(
            OperatorNode::with_schema(
                crate::ir::Operator::ASAP(ASAPOp::SummaryEstimate {
                    summary_input: child,
                    query: SketchStatistic::Quantile { q: 0.99 },
                }),
                Schema::lifted(
                    vec![Field::plain("quantile_0_99", DataType::Float64, false)],
                    None,
                ),
            )
            .with_guarantee(None),
        )
    }

    fn aggregate(measure: AggIntent, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
        OperatorNode::new_shared(crate::ir::Operator::NonASAP(NonASAPOp::Aggregate {
            reduction: Reduction::by(vec![]),
            measures: vec![measure],
            output_names: vec![],
            filters: vec![],
            having: None,
            child,
        }))
        .unwrap()
    }

    fn max(child: Rc<OperatorNode>) -> Rc<OperatorNode> {
        aggregate(AggIntent::Max { col: None }, child)
    }

    /// `node` with its timing fixed in advance, as a candidate builder does
    /// for an operator that must feed maintenance.
    fn placed_at_ingestion(node: Rc<OperatorNode>) -> Rc<OperatorNode> {
        Rc::new(
            (*node)
                .clone()
                .with_timing(Some(ExecutionTiming::IngestionTime)),
        )
    }

    /// Apply with every summary maintained, the placement whose edge rules
    /// these tests exercise.
    fn apply(root: &Rc<OperatorNode>) -> Result<Rc<OperatorNode>, ExecutionDataStateError> {
        apply_materialization_timings(
            root,
            &MaterializationAssignment::all_ingestion_time(),
            &mut TimingMemo::new(),
        )
    }

    fn child(node: &Rc<OperatorNode>) -> Rc<OperatorNode> {
        Rc::clone(node.children()[0])
    }

    /// Without a materialization decision, a summary and its input run at
    /// query time; an explicit per-state choice overrides the default.
    #[test]
    fn default_assignment_materializes_nothing() {
        let summary = agg(scan(), kll());
        let root = apply_materialization_timings(
            &summary,
            &MaterializationAssignment::default(),
            &mut TimingMemo::new(),
        )
        .unwrap();
        assert_eq!(
            data_state(&root),
            Some(ExecutionDataState {
                timing: ExecutionTiming::QueryTime,
                primitive: DataPrimitive::SummaryState,
            })
        );
        assert_eq!(
            data_state(&child(&root)),
            Some(ExecutionDataState::QUERY_ROWS)
        );
        let mut assignment = MaterializationAssignment::all_query_time();
        assignment.set(&summary, ExecutionTiming::IngestionTime);
        let root =
            apply_materialization_timings(&summary, &assignment, &mut TimingMemo::new()).unwrap();
        assert_eq!(
            data_state(&root),
            Some(ExecutionDataState::INGESTION_SUMMARY)
        );
    }

    #[test]
    fn summary_agg_input_runs_at_ingestion_time() {
        let root = apply(&agg(scan(), kll())).unwrap();
        assert_eq!(
            data_state(&root),
            Some(ExecutionDataState::INGESTION_SUMMARY)
        );
        assert_eq!(
            data_state(&child(&root)),
            Some(ExecutionDataState::INGESTION_ROWS)
        );
    }

    #[test]
    fn exact_accumulator_state_may_feed_another_summary_agg() {
        let inner = agg(scan(), exact_sum());
        assert!(apply(&estimate(agg(inner, kll()))).is_ok());
    }

    #[test]
    fn evaluation_can_feed_summary_construction_at_query_time() {
        let inner = estimate(agg(scan(), kll()));
        let root = apply(&estimate(agg(inner, kll()))).unwrap();
        assert_eq!(child(&root).timing, Some(ExecutionTiming::QueryTime));
    }

    /// Any non-ASAP operator over a evaluation runs at query time.
    #[test]
    fn query_time_operation_over_evaluation_is_legal_and_root_is_evaluation() {
        let evaluation = || estimate(agg(scan(), kll()));
        let sorted = OperatorNode::new_shared(crate::ir::Operator::NonASAP(NonASAPOp::Sort {
            keys: vec![],
            partition_by: Default::default(),
            child: evaluation(),
        }))
        .unwrap();
        for root in [max(evaluation()), sorted] {
            let root = apply(&root).unwrap();
            assert_eq!(data_state(&root), Some(ExecutionDataState::QUERY_ROWS));
        }
    }

    #[test]
    fn query_time_values_can_feed_query_time_summary_construction() {
        let post = max(estimate(agg(scan(), kll())));
        let root = apply(&estimate(agg(post, kll()))).unwrap();
        assert_eq!(child(&root).timing, Some(ExecutionTiming::QueryTime));
    }

    #[test]
    fn function_under_summary_agg_is_legal_but_not_at_root() {
        let operation = placed_at_ingestion(max(scan()));
        assert_eq!(
            apply(&operation).err(),
            Some(ExecutionDataStateError::MaintenanceRowsAtRoot)
        );
        let root = apply(&estimate(agg(operation, kll()))).unwrap();
        let timed_operation = child(&child(&root));
        assert_eq!(
            data_state(&timed_operation),
            Some(ExecutionDataState::INGESTION_ROWS)
        );
    }

    #[test]
    fn function_over_evaluation_is_rejected() {
        let operation = placed_at_ingestion(max(estimate(agg(scan(), kll()))));
        assert!(matches!(
            apply(&estimate(agg(operation, kll()))),
            Err(ExecutionDataStateError::IllegalChildDataState {
                child: ExecutionDataState::QUERY_ROWS,
                ..
            })
        ));
    }

    /// One shared sub-DAG reached as maintenance input and as query-time
    /// input cannot be executed once for both; splitting it by phase first
    /// makes the plan timeable.
    #[test]
    fn a_shared_subtree_reached_at_two_timings_conflicts() {
        let shared = scan();
        let root = OperatorNode::new_shared(crate::ir::Operator::NonASAP(NonASAPOp::Concat {
            children: vec![
                max(estimate(agg(Rc::clone(&shared), kll()))),
                max(Rc::clone(&shared)),
            ],
            discriminator_unique_key: None,
        }))
        .unwrap();
        assert_eq!(
            apply(&root).err(),
            Some(ExecutionDataStateError::ConflictingTiming {
                first: ExecutionDataState::INGESTION_ROWS,
                second: ExecutionDataState::QUERY_ROWS,
            })
        );
        let split = split_shared_by_phase(&root, &MaterializationAssignment::all_ingestion_time());
        assert!(apply(&split).is_ok());
    }

    /// Both paired operands must be plain; an unrelated state column is not
    /// an input.
    #[test]
    fn pearson_corr_checks_both_operand_states() {
        let corr_over = |state_column: usize| {
            let mut fields = vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("x", DataType::Float64, false),
                Field::plain("y", DataType::Float64, false),
                Field::plain("unused", DataType::Float64, false),
            ];
            fields[state_column].dtype = kll();
            aggregate(
                AggIntent::PearsonCorr { left: 1, right: 2 },
                scan_with(fields),
            )
        };
        for operand in [1, 2] {
            assert!(matches!(
                validate_maintained(&corr_over(operand), ExecutionTiming::QueryTime),
                Err(ExecutionDataStateError::NonPlainOperand { .. })
            ));
        }
        validate_maintained(&corr_over(3), ExecutionTiming::QueryTime).unwrap();
    }
}
