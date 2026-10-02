//! Execution timing: written into every node from a lifecycle assignment,
//! then validated against each operator's kind and its consuming edges.
//!
//! The logical DAG carries no timing. Summary materialization chooses a
//! lifecycle per summary state; [`LifecycleAssignment`] records that choice
//! (ingestion-time maintenance or query-time recomputation per `SummaryAgg`)
//! and [`apply_lifecycle_timings`] expands it into a timing on every node:
//!
//! - a node of fixed kind takes its kind's timing (`SummaryEstimate` and
//!   `ReadPopulation` run at query time, `MaintainPopulation` at ingestion
//!   time);
//! - a `SummaryAgg` takes the assignment's timing (default: ingestion time),
//!   unless something below it can only exist at query time;
//! - every other node runs when its consumer runs: everything that feeds a
//!   maintained state runs at ingestion time, everything above a readout at
//!   query time.
//!
//! A node reached from two consumers that need different timings cannot be
//! executed once for both; [`split_shared_by_phase`] copies such a subtree
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
//! | `ReadPopulation.child` | A `MaintainPopulation` at ingestion time |
//! | `MaintainPopulation.child` | Ingestion-time rows matching the population's input |
//! | any `NonASAP` consumer | Rows (or exact-accumulator state for a projection-like operator) at the consumer's own timing; ingestion work never reads a query-time value |

use std::collections::HashMap;
use std::rc::Rc;

use super::asap::ASAPOp;
use super::node::{Operator, OperatorNode};
use super::non_asap::NonASAPOp;
use crate::post_asap::execution_data_state::{
    DataPrimitive, ExecutionDataState, ExecutionDataStateError, ExecutionTiming,
};
use crate::pre_asap::query_expr::BinaryOpKind;
use crate::pre_asap::schema::{DataType, FieldDataType, Schema};

/// The per-state lifecycle choice summary materialization made: for each
/// `SummaryAgg` node (by identity), whether its state is maintained at
/// ingestion time or recomputed at query time. A state absent from the map
/// takes the default, ingestion-time maintenance.
#[derive(Debug, Clone, Default)]
pub struct LifecycleAssignment {
    summary_timings: HashMap<*const OperatorNode, ExecutionTiming>,
}

impl LifecycleAssignment {
    /// The assignment under which every summary state is maintained at
    /// ingestion time — the timings every plan carried before lifecycles
    /// became a planning choice.
    pub fn default_maintained() -> Self {
        Self::default()
    }

    pub fn set(&mut self, summary: &Rc<OperatorNode>, timing: ExecutionTiming) {
        self.summary_timings.insert(Rc::as_ptr(summary), timing);
    }

    pub fn summary_timing(&self, summary: &Rc<OperatorNode>) -> ExecutionTiming {
        self.summary_timings
            .get(&Rc::as_ptr(summary))
            .copied()
            .unwrap_or(ExecutionTiming::IngestionTime)
    }
}

/// Memo of one [`apply_lifecycle_timings`] pass: `input node → timed node`,
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

/// Whether the subtree below `node` contains a node that can only run at
/// query time (a readout), which forces every consumer above it to query
/// time as well.
fn forces_query_time(node: &OperatorNode, seen: &mut HashMap<*const OperatorNode, bool>) -> bool {
    let key = node as *const OperatorNode;
    if let Some(&cached) = seen.get(&key) {
        return cached;
    }
    let forced = match &node.operator {
        Operator::ASAP(ASAPOp::SummaryEstimate { .. })
        | Operator::ASAP(ASAPOp::ReadPopulation { .. }) => true,
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
pub fn apply_lifecycle_timings(
    root: &Rc<OperatorNode>,
    assignment: &LifecycleAssignment,
    memo: &mut TimingMemo,
) -> Result<Rc<OperatorNode>, ExecutionDataStateError> {
    let mut forced = HashMap::new();
    let timed = write(root, ExecutionTiming::QueryTime, assignment, memo, &mut forced)?;
    if timed.timing == Some(ExecutionTiming::IngestionTime)
        && data_state(&timed).map(|s| s.primitive) == Some(DataPrimitive::Raw)
    {
        return Err(ExecutionDataStateError::MaintenanceRowsAtRoot);
    }
    validate(&timed, &mut HashMap::new())?;
    Ok(timed)
}

/// Validate the subtree below `root` under the default (every summary
/// maintained) assignment, with `root` consumed at `root_timing`. For
/// planning-time legality checks of a candidate before it is assembled into
/// a workload DAG; nothing is kept.
pub fn validate_default(
    root: &Rc<OperatorNode>,
    root_timing: ExecutionTiming,
) -> Result<(), ExecutionDataStateError> {
    let assignment = LifecycleAssignment::default_maintained();
    let mut memo = TimingMemo::new();
    let mut forced = HashMap::new();
    let timed = write(root, root_timing, &assignment, &mut memo, &mut forced)?;
    validate(&timed, &mut HashMap::new())
}

/// The data state `node` produces under the default assignment when its
/// consumer runs at `consumer` — the planning-time answer to "what does this
/// candidate's output look like" before any assignment is applied.
pub fn planned_data_state(node: &Rc<OperatorNode>, consumer: ExecutionTiming) -> ExecutionDataState {
    let mut forced = HashMap::new();
    let timing = own_timing(node, consumer, &LifecycleAssignment::default_maintained(), &mut forced);
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
    assignment: &LifecycleAssignment,
    forced: &mut HashMap<*const OperatorNode, bool>,
) -> ExecutionTiming {
    match &node.operator {
        Operator::ASAP(ASAPOp::SummaryEstimate { .. })
        | Operator::ASAP(ASAPOp::ReadPopulation { .. }) => ExecutionTiming::QueryTime,
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
    assignment: &LifecycleAssignment,
    memo: &mut TimingMemo,
    forced: &mut HashMap<*const OperatorNode, bool>,
) -> Result<Rc<OperatorNode>, ExecutionDataStateError> {
    let timing = own_timing(node, consumer, assignment, forced);
    if let Some(done) = memo.done.get(&Rc::as_ptr(node)) {
        let previous = done.timing.expect("memoized node is timed");
        if previous != timing {
            return Err(ExecutionDataStateError::AmbiguousKeepPreAsap {
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
    let operator = node.operator.map_children(|child| {
        match write(child, timing, assignment, memo, forced) {
            Ok(timed) => timed,
            Err(e) => {
                error.get_or_insert(e);
                Rc::clone(child)
            }
        }
    });
    if let Some(e) = error {
        return Err(e);
    }
    let timed = Rc::new(OperatorNode {
        operator,
        result_kind: node.result_kind,
        schema: node.schema.clone(),
        guarantee: node.guarantee.clone(),
        timing: Some(timing),
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
                    return Err(ExecutionDataStateError::ReadoutUnderMaintenance {
                        edge: "SummaryAgg.child",
                        child: other,
                    })
                }
            }
            if timing == ExecutionTiming::IngestionTime
                && avail.timing == ExecutionTiming::QueryTime
            {
                return Err(ExecutionDataStateError::ReadoutUnderMaintenance {
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
            let valid = timing == ExecutionTiming::IngestionTime
                && population.matches_node(child)
                && state_of(child) == ExecutionDataState::INGESTION_ROWS;
            if !valid {
                return Err(ExecutionDataStateError::InvalidMaintainedPopulation);
            }
            Ok(())
        }
        ASAPOp::ReadPopulation { child, readout } => {
            let valid = timing == ExecutionTiming::QueryTime
                && matches!(
                    &child.operator,
                    Operator::ASAP(ASAPOp::MaintainPopulation { population, .. })
                        if population.supports(readout)
                            && child.timing == Some(ExecutionTiming::IngestionTime)
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
                BinaryOpKind::Arithmetic(crate::pre_asap::ArithmeticOpKind::Div)
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
                            && matches!(
                                field.dtype,
                                FieldDataType::Plain(DataType::Float64 | DataType::Timestamp)
                            )
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

/// Copy, for one consumer, every subtree that `assignment` would reach with
/// two different timings, so that a workload whose CSE shared a `Scan`
/// between an ingestion-time summary and a query-time computation can still
/// be timed. Only the conflicting subtrees are copied; a subtree reached with
/// one timing stays one `Rc`. Returns the (possibly rewritten) root.
pub fn split_shared_by_phase(
    root: &Rc<OperatorNode>,
    assignment: &LifecycleAssignment,
) -> Rc<OperatorNode> {
    // First pass: the set of timings each node is reached with.
    let mut reached: HashMap<*const OperatorNode, Vec<ExecutionTiming>> = HashMap::new();
    let mut forced = HashMap::new();
    fn collect(
        node: &Rc<OperatorNode>,
        consumer: ExecutionTiming,
        assignment: &LifecycleAssignment,
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
    collect(root, ExecutionTiming::QueryTime, assignment, &mut reached, &mut forced);
    if reached.values().all(|timings| timings.len() <= 1) {
        return Rc::clone(root);
    }
    // Second pass: rebuild, giving each (node, timing) pair its own copy.
    let mut copies: HashMap<(*const OperatorNode, ExecutionTiming), Rc<OperatorNode>> =
        HashMap::new();
    fn rebuild(
        node: &Rc<OperatorNode>,
        consumer: ExecutionTiming,
        assignment: &LifecycleAssignment,
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
