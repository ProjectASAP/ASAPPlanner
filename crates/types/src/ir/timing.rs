//! Execution timing: written into every node from a materialization
//! assignment, then validated against each operator's kind and its consuming
//! edges.
//!
//! The logical DAG carries no timing. Materialization (#509
//! §Materialization) gives each sub-DAG one of three options:
//! `NotMaterialized`, `MatAtIngestion(medium)` or `MatAtQuery(medium)`.
//! [`MaterializationAssignment`] records that option per sub-DAG, named by
//! the node whose output would be stored (any node: a summary state, a
//! population, or e.g. the tumbling-window panes of #509 Example 4), and
//! [`apply_materialization_timings`] expands it into a timing on every node:
//!
//! - `SummaryEstimate` and `EvaluatePopulation` run at query time;
//! - a node assigned `MatAtIngestion` runs at ingestion time, and one
//!   assigned `MatAtQuery` at query time. A `SummaryAgg` or
//!   `MaintainPopulation` without an assignment is `NotMaterialized` and runs
//!   at query time. Every node upstream of an ingestion-time node also runs
//!   at ingestion time, so `MatAtIngestion` (or an ingestion timing fixed by
//!   a candidate builder) over something that can only exist at query time
//!   is an error ([`ExecutionDataStateError::UpstreamNotAtIngestion`]);
//! - a PromQL `BinaryOp` over per-series inputs that are not the same series
//!   rows runs at query time, since maintenance cannot align them;
//! - every other node is `NotMaterialized` and runs when its consumer runs:
//!   everything that feeds an ingestion-time node runs at ingestion time,
//!   everything above an evaluation at query time.
//!
//! A node whose timing a candidate builder fixed (e.g. a query-time
//! `FinalizeExactAccumulator`) keeps that timing. An explicit assignment on it that agrees is kept; one that needs
//! the other timing is an error
//! ([`ExecutionDataStateError::MaterializationConflictsWithFixedTiming`]).
//! A root may be `MatAtIngestion`: the query reads its stored output. A root
//! of rows that runs at ingestion time without being stored there is an
//! error ([`ExecutionDataStateError::UnstoredIngestionRoot`]).
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
//! | `EvaluatePopulation.child` | A timed `MaintainPopulation` |
//! | `MaintainPopulation.child` | Rows at its own timing matching the population's input |
//! | any `NonASAP` consumer | Rows (or exact-accumulator state for a projection-like operator) at the consumer's own timing, or a `MatAtIngestion` output read at query time; ingestion work never reads a query-time value |

use std::collections::HashMap;
use std::rc::Rc;

use super::asap::ASAPOp;
use super::node::{Operator, OperatorNode};
use super::non_asap::NonASAPOp;
use crate::ir::operator_properties::BinaryOpKind;
use crate::post_asap::execution_data_state::{
    DataPrimitive, ExecutionDataState, ExecutionDataStateError, ExecutionTiming, Materialization,
    Medium,
};
use crate::pre_asap::schema::{DataType, FieldDataType, Schema};

/// The materialization option of each sub-DAG (#509 §Materialization:
/// "Materialization decides, for each sub-DAG, whether its output is
/// stored"). A sub-DAG is named by the node whose output would be stored (by
/// identity); any node may be assigned. A node without an assignment is not
/// stored: a `SummaryAgg` or `MaintainPopulation` takes the default, every
/// other node runs when its consumer runs. `Default` assigns
/// `NotMaterialized` to every sub-DAG.
///
/// The assignment keeps every assigned node alive, so a later allocation
/// cannot reuse its address and match a stale entry.
#[derive(Debug, Clone, Default)]
pub struct MaterializationAssignment {
    assigned: HashMap<*const OperatorNode, (Rc<OperatorNode>, Materialization)>,
    unassigned: Materialization,
}

impl MaterializationAssignment {
    /// Every `SummaryAgg` or `MaintainPopulation` not [`set`](Self::set)
    /// explicitly takes `materialization` where the DAG allows it: a
    /// `MatAtIngestion` default over a sub-DAG that can only run at query
    /// time leaves that sub-DAG `NotMaterialized`.
    pub fn with_default(materialization: Materialization) -> Self {
        Self {
            assigned: HashMap::new(),
            unassigned: materialization,
        }
    }

    /// Assign `materialization` to the sub-DAG whose output `node` produces.
    pub fn set(&mut self, node: &Rc<OperatorNode>, materialization: Materialization) {
        self.assigned
            .insert(Rc::as_ptr(node), (Rc::clone(node), materialization));
    }

    /// The explicit assignment of `node`, if any.
    fn get(&self, node: &OperatorNode) -> Option<Materialization> {
        self.assigned
            .get(&(node as *const OperatorNode))
            .map(|(_, m)| *m)
    }

    /// The option `node` gets when `upstream_at_query_time` says whether
    /// something below it can only run at query time.
    fn resolve(
        &self,
        node: &OperatorNode,
        upstream_at_query_time: bool,
    ) -> Result<Materialization, ExecutionDataStateError> {
        let at_ingestion = |m: Materialization| m.timing() == ExecutionTiming::IngestionTime;
        match self.get(node) {
            Some(m) if at_ingestion(m) && upstream_at_query_time => {
                Err(ExecutionDataStateError::UpstreamNotAtIngestion {
                    operator: node.operator.kind_name(),
                })
            }
            Some(m) => Ok(m),
            None if at_ingestion(self.unassigned) && upstream_at_query_time => {
                Ok(Materialization::NotMaterialized)
            }
            None => Ok(self.unassigned),
        }
    }
}

/// Whether `operator`'s output is a sub-DAG output that materialization may
/// store: a `SummaryAgg` or a `MaintainPopulation`.
pub(crate) fn is_materialization_point<C>(operator: &Operator<C>) -> bool {
    matches!(
        operator,
        Operator::ASAP(ASAPOp::SummaryAgg { .. } | ASAPOp::MaintainPopulation { .. })
    )
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
/// query time (an evaluation, or a sub-DAG `assignment` does not put at
/// ingestion time), which forces every consumer above it to query time as well.
/// A PromQL `BinaryOp` whose inputs are not the same per-series rows also runs
/// at query time.
fn forces_query_time(
    node: &OperatorNode,
    assignment: &MaterializationAssignment,
    seen: &mut HashMap<*const OperatorNode, bool>,
) -> bool {
    let key = node as *const OperatorNode;
    if let Some(&cached) = seen.get(&key) {
        return cached;
    }
    let upstream = |seen: &mut HashMap<*const OperatorNode, bool>| {
        node.children()
            .iter()
            .any(|child| forces_query_time(child, assignment, seen))
    };
    let forced = match &node.operator {
        _ if node.timing == Some(ExecutionTiming::QueryTime) => true,
        Operator::ASAP(ASAPOp::SummaryEstimate { .. })
        | Operator::ASAP(ASAPOp::EvaluatePopulation { .. }) => true,
        _ if node.timing.is_none()
            && (is_materialization_point(&node.operator) || assignment.get(node).is_some()) =>
        {
            let upstream = upstream(seen);
            match assignment.resolve(node, upstream) {
                Ok(Materialization::NotMaterialized)
                    if !is_materialization_point(&node.operator) =>
                {
                    upstream
                }
                Ok(m) => m.timing() == ExecutionTiming::QueryTime,
                Err(_) => true,
            }
        }
        Operator::NonASAP(NonASAPOp::BinaryOp { lhs, rhs, .. })
            if node
                .schema
                .fields
                .iter()
                .any(|f| f.name == crate::pre_asap::schema::PROMQL_SERIES_IDENTITY)
                && per_series_rows(lhs).is_none_or(|rows| per_series_rows(rhs) != Some(rows)) =>
        {
            true
        }
        _ => upstream(seen),
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
    // A root stored at ingestion time is read by the query; one that runs at
    // ingestion time without being stored there produces rows nothing reads.
    if timed.timing == Some(ExecutionTiming::IngestionTime)
        && !matches!(timed.materialization, Materialization::MatAtIngestion(_))
        && data_state(&timed).map(|s| s.primitive) == Some(DataPrimitive::Raw)
    {
        return Err(ExecutionDataStateError::UnstoredIngestionRoot {
            operator: timed.operator.kind_name(),
        });
    }
    validate(&timed, &mut HashMap::new())?;
    Ok(timed)
}

/// Validate the sub-DAG below `root` with every sub-DAG `MatAtIngestion`
/// where the DAG allows it ([`MaterializationAssignment::with_default`]) and
/// `root` consumed at `root_timing`. For planning-time legality checks of a
/// candidate before it is assembled into a workload DAG: a candidate must stay
/// executable if materialization later puts its states at ingestion time.
/// Nothing is kept.
pub fn validate_at_ingestion(
    root: &Rc<OperatorNode>,
    root_timing: ExecutionTiming,
) -> Result<(), ExecutionDataStateError> {
    let assignment = all_at_ingestion();
    let mut memo = TimingMemo::new();
    let mut forced = HashMap::new();
    let timed = write(root, root_timing, &assignment, &mut memo, &mut forced)?;
    validate(&timed, &mut HashMap::new())
}

/// The data state `node` produces with every sub-DAG `MatAtIngestion` where
/// the DAG allows it, when its consumer runs at `consumer` — the planning-time answer to
/// "what does this candidate's output look like", consistent with
/// [`validate_at_ingestion`].
pub fn planned_data_state(
    node: &Rc<OperatorNode>,
    consumer: ExecutionTiming,
) -> ExecutionDataState {
    let mut forced = HashMap::new();
    let (timing, _) = placement(node, consumer, &all_at_ingestion(), &mut forced)
        .expect("a default assignment never fails");
    ExecutionDataState {
        timing,
        primitive: match &node.operator {
            Operator::ASAP(op) if op.produced_state().is_some() => DataPrimitive::SummaryState,
            Operator::ASAP(ASAPOp::MaintainPopulation { .. }) => DataPrimitive::SummaryState,
            _ => DataPrimitive::Raw,
        },
    }
}

/// The assignment of [`validate_at_ingestion`] and [`planned_data_state`].
fn all_at_ingestion() -> MaterializationAssignment {
    MaterializationAssignment::with_default(Materialization::MatAtIngestion(Medium::Memory))
}

/// The timing and materialization `node` takes when its consumer runs at
/// `consumer`.
fn placement(
    node: &Rc<OperatorNode>,
    consumer: ExecutionTiming,
    assignment: &MaterializationAssignment,
    forced: &mut HashMap<*const OperatorNode, bool>,
) -> Result<(ExecutionTiming, Materialization), ExecutionDataStateError> {
    // A placement fixed when the candidate was built (an exact-state read
    // boundary that must run at query time, or one that feeds maintenance)
    // is honored; a conflicting consumer is rejected by validation.
    let upstream_at_query_time = |forced: &mut HashMap<*const OperatorNode, bool>| {
        node.children()
            .iter()
            .any(|child| forces_query_time(child, assignment, forced))
    };
    if let Some(placed) = node.timing {
        if placed == ExecutionTiming::IngestionTime && upstream_at_query_time(forced) {
            return Err(ExecutionDataStateError::UpstreamNotAtIngestion {
                operator: node.operator.kind_name(),
            });
        }
        // An explicit assignment is never silently dropped: one that agrees
        // with the fixed timing is kept, one that needs the other timing is
        // an error.
        return match assignment.get(node) {
            Some(m) if m != Materialization::NotMaterialized && m.timing() != placed => Err(
                ExecutionDataStateError::MaterializationConflictsWithFixedTiming {
                    operator: node.operator.kind_name(),
                    fixed: placed,
                    materialization: m,
                },
            ),
            Some(m) => Ok((placed, m)),
            None => Ok((placed, node.materialization)),
        };
    }
    Ok(match &node.operator {
        Operator::ASAP(ASAPOp::SummaryEstimate { .. })
        | Operator::ASAP(ASAPOp::EvaluatePopulation { .. }) => match assignment.get(node) {
            Some(m) if m.timing() == ExecutionTiming::IngestionTime => {
                return Err(ExecutionDataStateError::UpstreamNotAtIngestion {
                    operator: node.operator.kind_name(),
                })
            }
            Some(m) => (ExecutionTiming::QueryTime, m),
            None => (ExecutionTiming::QueryTime, Materialization::NotMaterialized),
        },
        _ if is_materialization_point(&node.operator) || assignment.get(node).is_some() => {
            let upstream = upstream_at_query_time(forced);
            match assignment.resolve(node, upstream)? {
                Materialization::NotMaterialized if !is_materialization_point(&node.operator) => {
                    (consumer, Materialization::NotMaterialized)
                }
                materialization => (materialization.timing(), materialization),
            }
        }
        _ => (consumer, Materialization::NotMaterialized),
    })
}

/// The timing `node` takes when its consumer runs at `consumer`, for
/// [`split_shared_by_phase`]: an assignment that cannot be honored is
/// reported later by [`apply_materialization_timings`].
fn own_timing(
    node: &Rc<OperatorNode>,
    consumer: ExecutionTiming,
    assignment: &MaterializationAssignment,
    forced: &mut HashMap<*const OperatorNode, bool>,
) -> ExecutionTiming {
    placement(node, consumer, assignment, forced)
        .map_or(ExecutionTiming::QueryTime, |(timing, _)| timing)
}

fn write(
    node: &Rc<OperatorNode>,
    consumer: ExecutionTiming,
    assignment: &MaterializationAssignment,
    memo: &mut TimingMemo,
    forced: &mut HashMap<*const OperatorNode, bool>,
) -> Result<Rc<OperatorNode>, ExecutionDataStateError> {
    let (timing, materialization) = placement(node, consumer, assignment, forced)?;
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
    let timed = Rc::new(
        OperatorNode::with_schema(operator, node.schema.clone())
            .with_guarantee(node.guarantee.clone())
            .with_timing(Some(timing))
            .with_materialization(materialization),
    );
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
        // A query-time consumer may read the stored output of a sub-DAG
        // materialized at ingestion time.
        let reads_stored = timing == ExecutionTiming::QueryTime
            && child.materialization.timing() == ExecutionTiming::IngestionTime;
        if (s.timing != timing && !reads_stored)
            || (s.primitive != DataPrimitive::Raw && !passes_state)
        {
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
                BinaryOpKind::Arithmetic(crate::pre_asap::ArithmeticOpKind::Div)
            );
            if (operator.checked_relative_division && operator.checked_finite_division)
                || ((operator.checked_relative_division || operator.checked_finite_division)
                    && (timing != ExecutionTiming::QueryTime || !is_div))
            {
                return Err(ExecutionDataStateError::InvalidCheckedDivision);
            }
            if timing == ExecutionTiming::IngestionTime {
                // The opaque series identity is a non-null Utf8 key; every
                // other column is a non-null Float64 value or timestamp.
                let plain_float_or_ts = |schema: &Schema| {
                    schema.fields.iter().all(|field| {
                        !field.nullable
                            && if field.name == crate::pre_asap::schema::PROMQL_SERIES_IDENTITY {
                                field.dtype == FieldDataType::Plain(DataType::Utf8)
                            } else {
                                matches!(
                                    field.dtype,
                                    FieldDataType::Plain(DataType::Float64 | DataType::Timestamp)
                                )
                            }
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
                        .filter(|f| f.name == crate::pre_asap::schema::PROMQL_SERIES_IDENTITY)
                        .count()
                        > 1
                    || (node
                        .schema
                        .fields
                        .iter()
                        .any(|f| f.name == crate::pre_asap::schema::PROMQL_SERIES_IDENTITY)
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
/// one timing stays one `Rc`. Returns the (possibly rewritten) root and
/// `assignment` remapped onto it: a rebuilt node keeps its original's
/// explicit materialization.
pub fn split_shared_by_phase(
    root: &Rc<OperatorNode>,
    assignment: &MaterializationAssignment,
) -> (Rc<OperatorNode>, MaterializationAssignment) {
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
        return (Rc::clone(root), assignment.clone());
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
            Rc::new(
                OperatorNode::with_schema(operator, node.schema.clone())
                    .with_guarantee(node.guarantee.clone())
                    .with_timing(node.timing)
                    .with_materialization(node.materialization),
            )
        } else {
            Rc::clone(node)
        };
        copies.insert(key, Rc::clone(&out));
        out
    }
    let root = rebuild(
        root,
        ExecutionTiming::QueryTime,
        assignment,
        &reached,
        &mut copies,
        &mut forced,
    );
    let mut remapped = assignment.clone();
    for ((original, _), copy) in &copies {
        if let Some((_, materialization)) = assignment.assigned.get(original) {
            remapped.set(copy, *materialization);
        }
    }
    (root, remapped)
}

/// Maintenance arithmetic needs the same per-series population on both sides.
fn per_series_rows(node: &OperatorNode) -> Option<&OperatorNode> {
    use crate::post_asap::ExactKind;
    match &node.operator {
        Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child }) => match &child.operator {
            Operator::ASAP(ASAPOp::SummaryAgg {
                child,
                family: FieldDataType::ExactAggregate(ExactKind::Sum | ExactKind::Count, _),
                reduction: crate::pre_asap::Reduction::PerEntity,
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
    use crate::ir::non_asap::NonASAPOp;
    use crate::ir::operator_properties::{Reduction, Source};
    use crate::post_asap::sketch::{
        ExactKind, ExactParams, GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams,
        SketchStatistic, SummaryUpdate,
    };
    use crate::pre_asap::agg_intent::AggIntent;
    use crate::pre_asap::expr_ir::ColumnRef;
    use crate::pre_asap::schema::Field;

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

    /// Apply with every sub-DAG `MatAtIngestion` where the DAG allows it, the
    /// placement whose edge rules these tests exercise.
    fn apply(root: &Rc<OperatorNode>) -> Result<Rc<OperatorNode>, ExecutionDataStateError> {
        apply_materialization_timings(root, &all_at_ingestion(), &mut TimingMemo::new())
    }

    const AT_INGESTION: Materialization = Materialization::MatAtIngestion(Medium::Memory);

    fn child(node: &Rc<OperatorNode>) -> Rc<OperatorNode> {
        Rc::clone(node.children()[0])
    }

    /// Without a materialization decision, a summary is `NotMaterialized` and
    /// it and its input run at query time; an explicit choice overrides the
    /// default and is recorded on the timed node.
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
        assert_eq!(root.materialization, Materialization::NotMaterialized);
        assert_eq!(
            data_state(&child(&root)),
            Some(ExecutionDataState::QUERY_ROWS)
        );
        for (materialization, timing) in [
            (AT_INGESTION, ExecutionTiming::IngestionTime),
            (
                Materialization::MatAtQuery(Medium::Disk),
                ExecutionTiming::QueryTime,
            ),
        ] {
            let mut assignment = MaterializationAssignment::default();
            assignment.set(&summary, materialization);
            let root = apply_materialization_timings(&summary, &assignment, &mut TimingMemo::new())
                .unwrap();
            assert_eq!(root.timing, Some(timing));
            assert_eq!(root.materialization, materialization);
            assert_eq!(child(&root).timing, Some(timing));
            assert_eq!(
                child(&root).materialization,
                Materialization::NotMaterialized
            );
        }
    }

    /// #509 Algorithm 3, line 13: an explicit `MatAtIngestion` over a state
    /// that only exists at query time is an error, not a silent move to
    /// query time.
    #[test]
    fn ingestion_time_node_over_query_time_upstream_is_rejected() {
        let inner = agg(scan(), exact_sum());
        let outer = agg(Rc::clone(&inner), kll());
        let mut assignment = MaterializationAssignment::default();
        assignment.set(&outer, AT_INGESTION);
        assert_eq!(
            apply_materialization_timings(&estimate(outer), &assignment, &mut TimingMemo::new())
                .err(),
            Some(ExecutionDataStateError::UpstreamNotAtIngestion {
                operator: "SummaryAgg"
            })
        );
        assignment.set(&inner, AT_INGESTION);
        let outer = agg(Rc::clone(&inner), kll());
        assignment.set(&outer, AT_INGESTION);
        let root =
            apply_materialization_timings(&estimate(outer), &assignment, &mut TimingMemo::new())
                .unwrap();
        assert_eq!(child(&root).materialization, AT_INGESTION);
        assert_eq!(child(&child(&root)).materialization, AT_INGESTION);
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

    /// A state not at ingestion time leaves the summary that reads it, when
    /// that summary has no explicit assignment, `NotMaterialized` at query
    /// time, as an evaluation below it would.
    #[test]
    fn query_time_state_moves_its_consuming_summary_to_query_time() {
        let inner = agg(scan(), exact_sum());
        let outer = agg(Rc::clone(&inner), kll());
        let mut assignment = all_at_ingestion();
        assignment.set(&inner, Materialization::NotMaterialized);
        let root =
            apply_materialization_timings(&estimate(outer), &assignment, &mut TimingMemo::new())
                .unwrap();
        let outer = child(&root);
        assert_eq!(outer.timing, Some(ExecutionTiming::QueryTime));
        assert_eq!(outer.materialization, Materialization::NotMaterialized);
        assert_eq!(child(&outer).timing, Some(ExecutionTiming::QueryTime));
    }

    #[test]
    fn evaluation_can_feed_summary_construction_at_query_time() {
        let inner = estimate(agg(scan(), kll()));
        let root = apply(&estimate(agg(inner, kll()))).unwrap();
        assert_eq!(child(&root).timing, Some(ExecutionTiming::QueryTime));
    }

    /// Any non-ASAP operator over an evaluation runs at query time.
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
            Some(ExecutionDataStateError::UnstoredIngestionRoot {
                operator: "Aggregate"
            })
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
        assert_eq!(
            apply(&estimate(agg(operation, kll()))).err(),
            Some(ExecutionDataStateError::UpstreamNotAtIngestion {
                operator: "Aggregate"
            })
        );
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
        let (split, assignment) = split_shared_by_phase(&root, &all_at_ingestion());
        assert!(apply_materialization_timings(&split, &assignment, &mut TimingMemo::new()).is_ok());
    }

    /// A finalize pinned to ingestion time (as fixed-window rate candidates
    /// build it) over a state that is not `MatAtIngestion` names the missing
    /// upstream, not an illegal edge.
    #[test]
    fn ingestion_pinned_finalize_over_query_time_state_is_upstream_not_at_ingestion() {
        let finalize = placed_at_ingestion(
            OperatorNode::new_shared(crate::ir::Operator::ASAP(
                ASAPOp::FinalizeExactAccumulator {
                    child: agg(scan(), exact_sum()),
                },
            ))
            .unwrap(),
        );
        assert_eq!(
            apply_materialization_timings(
                &estimate(agg(finalize, kll())),
                &MaterializationAssignment::default(),
                &mut TimingMemo::new()
            )
            .err(),
            Some(ExecutionDataStateError::UpstreamNotAtIngestion {
                operator: "FinalizeExactAccumulator"
            })
        );
    }

    /// Splitting rebuilds the ancestors of a copied sub-DAG; an explicit
    /// assignment on one of them follows it onto the rebuilt node.
    #[test]
    fn split_keeps_explicit_assignments() {
        let shared = scan();
        let summary = agg(Rc::clone(&shared), kll());
        let root = OperatorNode::new_shared(crate::ir::Operator::NonASAP(NonASAPOp::Concat {
            children: vec![max(estimate(Rc::clone(&summary))), max(Rc::clone(&shared))],
            discriminator_unique_key: None,
        }))
        .unwrap();
        let mut assignment = MaterializationAssignment::default();
        assignment.set(&summary, AT_INGESTION);
        let (split, assignment) = split_shared_by_phase(&root, &assignment);
        let timed =
            apply_materialization_timings(&split, &assignment, &mut TimingMemo::new()).unwrap();
        let summary = child(&child(&child(&timed)));
        assert!(matches!(
            summary.operator,
            Operator::ASAP(ASAPOp::SummaryAgg { .. })
        ));
        assert_eq!(summary.materialization, AT_INGESTION);
        assert_eq!(summary.timing, Some(ExecutionTiming::IngestionTime));
    }

    /// Any node may be materialized, not only a summary state: stored rows
    /// at ingestion time are read by a query-time consumer, and a node that
    /// can only run at query time cannot be stored at ingestion time.
    #[test]
    fn any_node_may_be_materialized() {
        let panes = max(scan());
        let root = max(Rc::clone(&panes));
        let mut assignment = MaterializationAssignment::default();
        assignment.set(&panes, AT_INGESTION);
        let timed =
            apply_materialization_timings(&root, &assignment, &mut TimingMemo::new()).unwrap();
        assert_eq!(timed.timing, Some(ExecutionTiming::QueryTime));
        assert_eq!(child(&timed).materialization, AT_INGESTION);
        assert_eq!(child(&timed).timing, Some(ExecutionTiming::IngestionTime));
        assert_eq!(
            child(&child(&timed)).timing,
            Some(ExecutionTiming::IngestionTime)
        );

        let over_evaluation = max(estimate(agg(scan(), kll())));
        let mut assignment = MaterializationAssignment::default();
        assignment.set(&over_evaluation, AT_INGESTION);
        assert_eq!(
            apply_materialization_timings(&over_evaluation, &assignment, &mut TimingMemo::new())
                .err(),
            Some(ExecutionDataStateError::UpstreamNotAtIngestion {
                operator: "Aggregate"
            })
        );
    }

    fn finalize(child: Rc<OperatorNode>) -> Rc<OperatorNode> {
        OperatorNode::new_shared(crate::ir::Operator::ASAP(
            ASAPOp::FinalizeExactAccumulator { child },
        ))
        .unwrap()
    }

    /// The materialization the exported DAG records for `node`'s operator kind.
    fn exported(root: &Rc<OperatorNode>, kind: &str) -> Materialization {
        let dag = crate::ir::physical_export::compile_physical_asap_dag(root).unwrap();
        dag.validate().unwrap();
        dag.nodes
            .iter()
            .find(|n| n.payload.kind_name() == kind)
            .unwrap()
            .materialization
    }

    /// A node whose timing its candidate fixed (a query-time read boundary)
    /// keeps an explicit assignment that agrees
    /// with that timing and rejects one that needs the other timing.
    #[test]
    fn fixed_timing_keeps_an_agreeing_assignment_and_rejects_a_conflict() {
        let state = agg(scan(), exact_sum());
        let read = Rc::new(
            (*finalize(Rc::clone(&state)))
                .clone()
                .with_timing(Some(ExecutionTiming::QueryTime)),
        );
        let stored = Materialization::MatAtQuery(Medium::Disk);
        let mut assignment = MaterializationAssignment::default();
        assignment.set(&read, stored);
        let timed =
            apply_materialization_timings(&read, &assignment, &mut TimingMemo::new()).unwrap();
        assert_eq!(timed.timing, Some(ExecutionTiming::QueryTime));
        assert_eq!(timed.materialization, stored);
        assert_eq!(exported(&timed, "FinalizeExactAccumulator"), stored);

        let mut assignment = MaterializationAssignment::default();
        assignment.set(&state, AT_INGESTION);
        assignment.set(&read, AT_INGESTION);
        assert_eq!(
            apply_materialization_timings(&read, &assignment, &mut TimingMemo::new()).err(),
            Some(
                ExecutionDataStateError::MaterializationConflictsWithFixedTiming {
                    operator: "FinalizeExactAccumulator",
                    fixed: ExecutionTiming::QueryTime,
                    materialization: AT_INGESTION,
                }
            )
        );
    }

    /// A root stored at ingestion time is valid: the query reads the stored
    /// output. A root that runs at ingestion time without being stored there
    /// is not.
    #[test]
    fn a_root_may_be_stored_at_ingestion_time() {
        let root = max(scan());
        let mut assignment = MaterializationAssignment::default();
        assignment.set(&root, AT_INGESTION);
        let timed =
            apply_materialization_timings(&root, &assignment, &mut TimingMemo::new()).unwrap();
        assert_eq!(timed.timing, Some(ExecutionTiming::IngestionTime));
        assert_eq!(exported(&timed, "Aggregate"), AT_INGESTION);

        let state = agg(scan(), exact_sum());
        let root = finalize(Rc::clone(&state));
        let mut assignment = MaterializationAssignment::default();
        assignment.set(&state, AT_INGESTION);
        assignment.set(&root, AT_INGESTION);
        let timed =
            apply_materialization_timings(&root, &assignment, &mut TimingMemo::new()).unwrap();
        assert_eq!(timed.timing, Some(ExecutionTiming::IngestionTime));
        assert_eq!(exported(&timed, "FinalizeExactAccumulator"), AT_INGESTION);

        let mut assignment = MaterializationAssignment::default();
        let root = placed_at_ingestion(max(scan()));
        assignment.set(&root, Materialization::NotMaterialized);
        assert_eq!(
            apply_materialization_timings(&root, &assignment, &mut TimingMemo::new()).err(),
            Some(ExecutionDataStateError::UnstoredIngestionRoot {
                operator: "Aggregate"
            })
        );
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
                validate_at_ingestion(&corr_over(operand), ExecutionTiming::QueryTime),
                Err(ExecutionDataStateError::NonPlainOperand { .. })
            ));
        }
        validate_at_ingestion(&corr_over(3), ExecutionTiming::QueryTime).unwrap();
    }
    // Typed derived updates retain one opaque series identity only when both
    // operands cover the same series; arbitrary labels are never admitted.
    #[test]
    fn maintenance_binary_accepts_only_well_typed_series_identity() {
        use crate::ir::non_asap::BinaryOperator;
        use crate::pre_asap::{ArithmeticOpKind, BinaryOpKind};
        let identity = crate::pre_asap::schema::PROMQL_SERIES_IDENTITY;
        // A finalized per-series Sum of `metric`'s rows.
        let operand = |metric: &str, schema: &Schema| {
            let rows = OperatorNode::new_shared(crate::ir::Operator::NonASAP(NonASAPOp::Scan {
                source: Source::TimeSeries {
                    metric: metric.into(),
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
            let mut state = schema.clone();
            state.fields[0].dtype = exact_sum();
            let sum = Rc::new(OperatorNode::with_schema(
                crate::ir::Operator::ASAP(ASAPOp::SummaryAgg {
                    child: rows,
                    family: exact_sum(),
                    input: SummaryUpdate::column(ColumnRef::SampleValue),
                    reduction: Reduction::PerEntity,
                    grouping: GroupingStrategy::default(),
                    filter: None,
                }),
                state,
            ));
            Rc::new(OperatorNode::with_schema(
                crate::ir::Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: sum }),
                schema.clone(),
            ))
        };
        let validate = |schema: Schema, rhs: &str| {
            let binary = Rc::new(OperatorNode::with_schema(
                crate::ir::Operator::NonASAP(NonASAPOp::BinaryOp {
                    operator: BinaryOperator {
                        kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Add),
                        vector_match: None,
                        checked_relative_division: false,
                        checked_finite_division: false,
                    },
                    return_bool: false,
                    lhs: operand("m", &schema),
                    rhs: operand(rhs, &schema),
                }),
                schema,
            ));
            validate_at_ingestion(&placed_at_ingestion(binary), ExecutionTiming::IngestionTime)
        };
        let mut schema = Schema::lifted(
            vec![
                Field::plain("value", DataType::Float64, false),
                Field::plain("ts", DataType::Timestamp, false),
            ],
            None,
        );
        schema.time_index = Some(1);
        assert!(validate(schema.clone(), "m").is_ok());
        assert!(
            validate(schema.clone(), "n").is_ok(),
            "no identity to align"
        );
        schema
            .fields
            .push(Field::plain(identity, DataType::Utf8, false));
        assert!(validate(schema.clone(), "m").is_ok());
        assert_eq!(
            validate(schema.clone(), "n"),
            Err(ExecutionDataStateError::InvalidMaintenanceBinary),
            "different selectors may cover different series"
        );
        for mutation in 0..4 {
            let mut invalid = schema.clone();
            match mutation {
                0 => invalid.fields[2].nullable = true,
                1 => invalid.fields[2].dtype = FieldDataType::Plain(DataType::Timestamp),
                2 => invalid.fields.push(invalid.fields[2].clone()),
                _ => invalid.fields[2].name = "label".into(),
            }
            assert_eq!(
                validate(invalid, "m"),
                Err(ExecutionDataStateError::InvalidMaintenanceBinary)
            );
        }
    }
}
