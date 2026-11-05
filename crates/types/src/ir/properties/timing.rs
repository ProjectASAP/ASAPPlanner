//! Execution timing: the physical properties of every node, derived from a
//! physical properties assignment, then validated against each operator's
//! kind and its consuming edges.
//!
//! The logical DAG carries no physical properties (#1200: a logical
//! candidate has only its logical properties). Materialization (#509
//! §Materialization) decides, for each sub-DAG, whether its output is
//! materialized, and if so with which [`PhysicalProperties`]: its timing and
//! its [`Lifetime`] (#1200). [`PhysicalPropertiesAssignment`] records that
//! per sub-DAG, named by the node whose output would be materialized (any
//! node: a summary state, a population, or e.g. the tumbling windows of #509
//! Example 4), and [`apply_physical_properties`] completes it into the
//! physical properties of every node:
//!
//! - `SummaryEstimate` and `EvaluatePopulation` run at query time;
//! - a node the assignment lists runs at its entry's timing with its entry's
//!   lifetime. A `SummaryAgg` or `MaintainPopulation` the assignment does
//!   not list is not materialized and runs at query time. Every node
//!   upstream of an ingestion-time node also runs at ingestion time, so an
//!   ingestion-time entry over something that can only exist at query time
//!   is an error ([`ExecutionDataStateError::UpstreamNotAtIngestion`]);
//! - a PromQL `BinaryOp` over per-series inputs that are not the same series
//!   rows runs at query time, since maintenance cannot align them;
//! - every other node is not materialized (lifetime `None`) and runs when its
//!   consumer runs: everything that feeds an ingestion-time node runs at
//!   ingestion time, everything above an evaluation at query time.
//!
//! An entry whose lifetime is `None` fixes only the node's timing: e.g. the
//! completed assignment of a plan Stage 3 selected, applied again for
//! export, gives every node the properties Stage 3 selected. A root may be
//! materialized at ingestion time across evaluations: the query reads its
//! stored output. A root of rows that runs at ingestion time without being
//! materialized there is an error
//! ([`ExecutionDataStateError::UnstoredIngestionRoot`]).
//!
//! A node reached from two consumers that need different timings cannot be
//! executed once for both; Stage 2's `split_shared_by_timing` copies such a
//! sub-DAG for one side (by [`timing_under_consumer`]) before the assignment
//! is applied, and the pass itself
//! rejects a conflict it still finds.
//!
//! ## Edge rules (checked after the assignment is completed)
//!
//! | Consumer | Accepts from an input |
//! |---|---|
//! | `SummaryAgg.child`, `SummaryTumblingWindowChain.child` | Rows, or exact-accumulator state, never a query-time value when the state is maintained; a query-time build reads an ingestion-time input (rows or exact-accumulator state) only as a materialized at ingestion time input's stored output |
//! | `SummaryEstimate.summary_input` | Summary state at either timing |
//! | `SummaryMerge.children` | Summary state at either timing; at ingestion time only for a merge at ingestion time |
//! | `FinalizeExactAccumulator.child` | Exact-accumulator state |
//! | `EvaluatePopulation.child` | A `MaintainPopulation` |
//! | `MaintainPopulation.child` | Rows at its own timing matching the population's input; unlike a `NonASAP` consumer, a query-time population does not read a materialized at ingestion time input's stored output |
//! | any `NonASAP` consumer | Rows (or exact-accumulator state for a projection-like operator) at the consumer's own timing, or an output materialized at ingestion time read at query time; ingestion work never reads a query-time value |

use std::collections::HashMap;
use std::rc::Rc;

use crate::ir::operator::asap::ASAPOp;
use crate::ir::operator::node::{Operator, OperatorNode};
use crate::ir::operator::non_asap::NonASAPOp;
use crate::ir::operator::operator_properties::BinaryOpKind;
use crate::ir::operator::query::QueryRoot;
use crate::ir::properties::execution_data_state::{
    DataPrimitive, ExecutionDataState, ExecutionDataStateError, ExecutionTiming,
};
use crate::ir::schema::{DataType, FieldDataType, Schema};
use crate::physical::{satisfies_physical, Evaluation, PhysicalProperties, PhysicalRequirement};
#[cfg(test)]
use crate::physical::{Lifetime, Medium};

/// The physical properties of operator nodes, by node identity (#509
/// §Materialization: "Materialization decides, for each sub-DAG, whether its
/// output is stored"). Stage 2 lists each materialized sub-DAG output, named
/// by the node whose output is materialized; an entry whose lifetime is
/// `None` fixes only the node's timing. A node the assignment does not list
/// is placed by [`apply_physical_properties`]' rules: not materialized, a
/// summary build or `MaintainPopulation` at query time, every other node
/// when its consumer runs. [`apply_physical_properties`] returns the
/// completed assignment, which lists every node it reached; physical stages
/// read a node's timing and lifetime from it. `Default` lists nothing.
///
/// The assignment keeps every listed node alive, so a later allocation
/// cannot reuse its address and match a stale entry.
#[derive(Clone, Default)]
pub struct PhysicalPropertiesAssignment {
    properties_of: HashMap<*const OperatorNode, (Rc<OperatorNode>, PhysicalProperties)>,
}

/// Each listed node as its operator kind and address, not its whole sub-DAG.
impl std::fmt::Debug for PhysicalPropertiesAssignment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let properties: Vec<_> = self
            .properties_of
            .iter()
            .map(|(ptr, (node, properties))| {
                (format!("{}@{ptr:p}", node.operator.kind_name()), properties)
            })
            .collect();
        f.debug_struct("PhysicalPropertiesAssignment")
            .field("properties", &properties)
            .finish()
    }
}

/// Equal when they list the same nodes (by identity) with the same
/// properties.
impl PartialEq for PhysicalPropertiesAssignment {
    fn eq(&self, other: &Self) -> bool {
        self.properties_of.len() == other.properties_of.len()
            && self.properties_of.iter().all(|(node, (_, properties))| {
                other
                    .properties_of
                    .get(node)
                    .is_some_and(|(_, other)| other == properties)
            })
    }
}

impl PhysicalPropertiesAssignment {
    /// Give the node `node` the physical properties `properties`: a
    /// materialized lifetime materializes the output of the sub-DAG it produces,
    /// lifetime `None` fixes only its timing.
    pub fn set(&mut self, node: &Rc<OperatorNode>, properties: PhysicalProperties) {
        self.properties_of
            .insert(Rc::as_ptr(node), (Rc::clone(node), properties));
    }

    /// Give `copy` the entry of `original`, if it has one: for a node
    /// rebuilt as a copy of another.
    pub fn copy_assignment(&mut self, original: &OperatorNode, copy: &Rc<OperatorNode>) {
        if let Some(properties) = self.get(original) {
            self.set(copy, properties);
        }
    }

    /// The physical properties of `node`, if the assignment lists it.
    pub fn get(&self, node: &OperatorNode) -> Option<PhysicalProperties> {
        self.properties_of
            .get(&(node as *const OperatorNode))
            .map(|(_, properties)| *properties)
    }

    /// Whether the assignment lists every node reachable from `roots`, as a
    /// completed assignment ([`apply_physical_properties_to_roots`]) does.
    pub fn lists_every_node_of(&self, roots: &[QueryRoot]) -> bool {
        fn visit(
            node: &OperatorNode,
            assignment: &PhysicalPropertiesAssignment,
            visited: &mut std::collections::HashSet<*const OperatorNode>,
        ) -> bool {
            if !visited.insert(node as *const OperatorNode) {
                return true;
            }
            assignment.get(node).is_some()
                && node
                    .children()
                    .iter()
                    .all(|child| visit(child, assignment, visited))
        }
        let mut visited = std::collections::HashSet::new();
        roots
            .iter()
            .flat_map(QueryRoot::operator_refs)
            .all(|node| visit(node, self, &mut visited))
    }

    /// Every summary build and `MaintainPopulation` reachable from `roots`
    /// materialized in memory at ingestion time where the DAG allows it: not under a
    /// filter, and with nothing below it that can only run at query time.
    #[cfg(any(test, feature = "test-support"))]
    pub fn materialized_at_ingestion_where_allowed(roots: &[Rc<OperatorNode>]) -> Self {
        fn visit(
            node: &Rc<OperatorNode>,
            assignment: &mut PhysicalPropertiesAssignment,
            visited: &mut std::collections::HashSet<*const OperatorNode>,
        ) {
            if !visited.insert(Rc::as_ptr(node)) {
                return;
            }
            for child in node.children() {
                visit(child, assignment, visited);
            }
            if is_materializable_state_output(&node.operator)
                && !is_filtered_build(node)
                && !node
                    .children()
                    .iter()
                    .any(|child| forces_query_time(child, assignment, &mut HashMap::new()))
            {
                assignment.set(
                    node,
                    PhysicalProperties::MATERIALIZED_IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION,
                );
            }
        }
        let mut assignment = Self::default();
        let mut visited = std::collections::HashSet::new();
        for root in roots {
            visit(root, &mut assignment, &mut visited);
        }
        assignment
    }
}

/// Whether `operator`'s output is a sub-DAG output that materialization may
/// store: a `SummaryAgg`, a `SummaryTumblingWindowChain` or a `MaintainPopulation`.
pub(crate) fn is_materializable_state_output<C>(operator: &Operator<C>) -> bool {
    matches!(
        operator,
        Operator::ASAP(
            ASAPOp::SummaryAgg { .. }
                | ASAPOp::SummaryTumblingWindowChain { .. }
                | ASAPOp::MaintainPopulation { .. }
        )
    )
}

/// Whether `node` only ever runs at query time: a `SummaryEstimate` or an
/// `EvaluatePopulation`.
fn runs_at_query_time_only(node: &OperatorNode) -> bool {
    matches!(
        node.asap(),
        Some(ASAPOp::SummaryEstimate { .. } | ASAPOp::EvaluatePopulation { .. })
    )
}

/// Whether `node` is a summary build (`SummaryAgg` or `SummaryTumblingWindowChain`) with a
/// filter, which never runs at ingestion time
/// ([`ExecutionDataStateError::FilteredSummaryAtIngestion`]).
pub fn is_filtered_build(node: &OperatorNode) -> bool {
    matches!(
        node.asap(),
        Some(
            ASAPOp::SummaryAgg {
                filter: Some(_),
                ..
            } | ASAPOp::SummaryTumblingWindowChain {
                filter: Some(_),
                ..
            }
        )
    )
}

/// What `node`'s output carries: summary state from a build, a merge or a
/// node that keeps whole states (a key filter, a projection of the state
/// column), raw rows otherwise.
fn primitive(node: &OperatorNode) -> DataPrimitive {
    match &node.operator {
        Operator::ASAP(op) if op.produced_state().is_some() => DataPrimitive::SummaryState,
        Operator::ASAP(ASAPOp::MaintainPopulation { .. }) => DataPrimitive::SummaryState,
        _ if node.result_kind == crate::ir::OperatorResultKind::State => {
            DataPrimitive::SummaryState
        }
        _ => DataPrimitive::Raw,
    }
}

/// The data state `node`'s output carries, if `timed` lists it.
pub fn data_state(
    node: &OperatorNode,
    physical_properties: &PhysicalPropertiesAssignment,
) -> Option<ExecutionDataState> {
    Some(ExecutionDataState {
        timing: physical_properties.get(node)?.timing(),
        primitive: primitive(node),
    })
}

/// Whether `node` is a PromQL `BinaryOp` over per-series inputs that are not
/// the same series rows, which maintenance cannot align.
fn is_unaligned_series_binary(node: &OperatorNode) -> bool {
    matches!(
        &node.operator,
        Operator::NonASAP(NonASAPOp::BinaryOp { lhs, rhs, .. })
            if node
                .schema
                .fields
                .iter()
                .any(|f| f.name == crate::ir::schema::PROMQL_SERIES_IDENTITY)
                && per_series_rows(lhs).is_none_or(|rows| per_series_rows(rhs) != Some(rows))
    )
}

/// Whether the sub-DAG below `node` contains a node that can only run at
/// query time (an evaluation, or a sub-DAG `assignment` does not put at
/// ingestion time), which forces every consumer above it to query time as well.
/// A PromQL `BinaryOp` whose inputs are not the same per-series rows also runs
/// at query time.
fn forces_query_time(
    node: &OperatorNode,
    assignment: &PhysicalPropertiesAssignment,
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
    let forced = if runs_at_query_time_only(node) {
        true
    } else {
        match assignment.get(node) {
            Some(listed) if listed.timing() == ExecutionTiming::QueryTime => true,
            // An ingestion-time entry the DAG cannot honor runs at query time.
            Some(_) => is_filtered_build(node) || upstream(seen),
            // A summary build that is not materialized runs at query time.
            None if is_materializable_state_output(&node.operator) => true,
            None if is_unaligned_series_binary(node) => true,
            None => upstream(seen),
        }
    };
    seen.insert(key, forced);
    forced
}

/// Complete `assignment` over every node reachable from `root`, top-down,
/// then validate every edge: the physical properties of every node, as the
/// completed assignment.
pub fn apply_physical_properties(
    root: &Rc<OperatorNode>,
    assignment: &PhysicalPropertiesAssignment,
) -> Result<PhysicalPropertiesAssignment, ExecutionDataStateError> {
    apply_physical_properties_to_roots(&[QueryRoot::Operator(Rc::clone(root))], assignment)
}

/// [`apply_physical_properties`] over every root of one workload, so a node
/// several roots share is timed once. A standalone scalar root has each
/// operator it reads timed.
pub fn apply_physical_properties_to_roots(
    roots: &[QueryRoot],
    assignment: &PhysicalPropertiesAssignment,
) -> Result<PhysicalPropertiesAssignment, ExecutionDataStateError> {
    let mut physical_properties = PhysicalPropertiesAssignment::default();
    let mut forced = HashMap::new();
    let mut validated = HashMap::new();
    for root in roots {
        for node in root.operator_refs() {
            write(
                node,
                ExecutionTiming::QueryTime,
                assignment,
                &mut physical_properties,
                &mut forced,
            )?;
            // A root stored at ingestion time is read by the query; one that
            // runs at ingestion time without being stored there produces rows
            // nothing reads.
            let properties = physical_properties
                .get(node)
                .expect("written node is timed");
            if properties.timing() == ExecutionTiming::IngestionTime
                && !satisfies_physical(
                    &properties,
                    &read_in_the_same_evaluation(ExecutionTiming::QueryTime),
                )
                && primitive(node) == DataPrimitive::Raw
            {
                return Err(ExecutionDataStateError::UnstoredIngestionRoot {
                    operator: node.operator.kind_name(),
                });
            }
            validate(node, &physical_properties, &mut validated)?;
        }
    }
    Ok(physical_properties)
}

/// Validate the sub-DAG below `root` with every sub-DAG materialized at ingestion time
/// where the DAG allows it
/// ([`PhysicalPropertiesAssignment::materialized_at_ingestion_where_allowed`]) and
/// `root` consumed at `root_timing`. For planning-time legality checks of a
/// candidate before it is assembled into a workload DAG: a candidate must stay
/// executable if materialization later puts its states at ingestion time.
#[cfg(any(test, feature = "test-support"))]
pub fn validate_at_ingestion(
    root: &Rc<OperatorNode>,
    root_timing: ExecutionTiming,
) -> Result<(), ExecutionDataStateError> {
    let assignment = PhysicalPropertiesAssignment::materialized_at_ingestion_where_allowed(
        std::slice::from_ref(root),
    );
    let mut physical_properties = PhysicalPropertiesAssignment::default();
    let mut forced = HashMap::new();
    write(
        root,
        root_timing,
        &assignment,
        &mut physical_properties,
        &mut forced,
    )?;
    validate(root, &physical_properties, &mut HashMap::new())
}

/// The data state `node` produces with every sub-DAG materialized at ingestion time where
/// the DAG allows it, when its consumer runs at `consumer` — the planning-time answer to
/// "what does this candidate's output look like", consistent with
/// [`validate_at_ingestion`].
#[cfg(any(test, feature = "test-support"))]
pub fn planned_data_state(
    node: &Rc<OperatorNode>,
    consumer: ExecutionTiming,
) -> ExecutionDataState {
    let mut forced = HashMap::new();
    let assignment = PhysicalPropertiesAssignment::materialized_at_ingestion_where_allowed(
        std::slice::from_ref(node),
    );
    let properties = placement(node, consumer, &assignment, &mut forced)
        .expect("an assignment materialized where allowed never fails");
    ExecutionDataState {
        timing: properties.timing(),
        primitive: primitive(node),
    }
}

/// The physical properties `node` takes when its consumer runs at
/// `consumer`.
fn placement(
    node: &Rc<OperatorNode>,
    consumer: ExecutionTiming,
    assignment: &PhysicalPropertiesAssignment,
    forced: &mut HashMap<*const OperatorNode, bool>,
) -> Result<PhysicalProperties, ExecutionDataStateError> {
    if let Some(listed) = assignment.get(node) {
        if listed.timing() == ExecutionTiming::IngestionTime {
            if is_filtered_build(node) {
                return Err(ExecutionDataStateError::FilteredSummaryAtIngestion {
                    operator: node.operator.kind_name(),
                });
            }
            if runs_at_query_time_only(node)
                || node
                    .children()
                    .iter()
                    .any(|child| forces_query_time(child, assignment, forced))
            {
                return Err(ExecutionDataStateError::UpstreamNotAtIngestion {
                    operator: node.operator.kind_name(),
                });
            }
        }
        return Ok(listed);
    }
    // A summary build that is not materialized runs at query time.
    Ok(
        if runs_at_query_time_only(node) || is_materializable_state_output(&node.operator) {
            PhysicalProperties::NOT_MATERIALIZED_AT_QUERY_TIME
        } else {
            PhysicalProperties::not_materialized_at(consumer)
        },
    )
}

/// The timing `node` takes when its consumer runs at `consumer`, by the
/// placement rules [`apply_physical_properties`] applies, for Stage 2's
/// `split_shared_by_timing`: an assignment that cannot be honored is
/// reported later by [`apply_physical_properties`].
/// `query_time_forced` caches, per node, whether something at or below it
/// can only run at query time: pass an empty map and reuse it across the
/// calls of one assignment.
pub fn timing_under_consumer(
    node: &Rc<OperatorNode>,
    consumer: ExecutionTiming,
    assignment: &PhysicalPropertiesAssignment,
    query_time_forced: &mut HashMap<*const OperatorNode, bool>,
) -> ExecutionTiming {
    placement(node, consumer, assignment, query_time_forced)
        .map_or(ExecutionTiming::QueryTime, |properties| properties.timing())
}

/// List `node` and every node below it in `timed`, `node` consumed at
/// `consumer`.
fn write(
    node: &Rc<OperatorNode>,
    consumer: ExecutionTiming,
    assignment: &PhysicalPropertiesAssignment,
    physical_properties: &mut PhysicalPropertiesAssignment,
    forced: &mut HashMap<*const OperatorNode, bool>,
) -> Result<(), ExecutionDataStateError> {
    let properties = placement(node, consumer, assignment, forced)?;
    if let Some(previous) = physical_properties.get(node) {
        if previous.timing() != properties.timing() {
            return Err(ExecutionDataStateError::ConflictingTiming {
                first: ExecutionDataState {
                    timing: previous.timing(),
                    primitive: primitive(node),
                },
                second: ExecutionDataState {
                    timing: properties.timing(),
                    primitive: primitive(node),
                },
            });
        }
        return Ok(());
    }
    for child in node.children() {
        write(
            child,
            properties.timing(),
            assignment,
            physical_properties,
            forced,
        )?;
    }
    physical_properties.set(node, properties);
    Ok(())
}

fn state_of(
    node: &OperatorNode,
    physical_properties: &PhysicalPropertiesAssignment,
) -> ExecutionDataState {
    data_state(node, physical_properties).expect("timed node")
}

/// Check every edge below `node` against the module-level rules.
fn validate(
    node: &Rc<OperatorNode>,
    physical_properties: &PhysicalPropertiesAssignment,
    seen: &mut HashMap<*const OperatorNode, ()>,
) -> Result<(), ExecutionDataStateError> {
    if seen.insert(Rc::as_ptr(node), ()).is_some() {
        return Ok(());
    }
    let timing = physical_properties.get(node).expect("timed node").timing();
    match &node.operator {
        Operator::ASAP(op) => validate_asap(node, op, timing, physical_properties)?,
        Operator::NonASAP(op) => validate_non_asap(node, op, timing, physical_properties)?,
    }
    for child in node.children() {
        validate(child, physical_properties, seen)?;
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
    physical_properties: &PhysicalPropertiesAssignment,
) -> Result<(), ExecutionDataStateError> {
    match op {
        ASAPOp::SummaryAgg { child, .. } | ASAPOp::SummaryTumblingWindowChain { child, .. } => {
            let edge = match op {
                ASAPOp::SummaryTumblingWindowChain { .. } => "SummaryTumblingWindowChain.child",
                _ => "SummaryAgg.child",
            };
            let avail = state_of(child, physical_properties);
            match avail {
                ExecutionDataState::INGESTION_ROWS | ExecutionDataState::QUERY_ROWS => {}
                s if s.primitive == DataPrimitive::SummaryState => {
                    is_exact_accumulator_state(&child.schema)?
                }
                other => {
                    return Err(
                        ExecutionDataStateError::QueryTimeValueUnderIngestionSummary {
                            edge,
                            child: other,
                        },
                    )
                }
            }
            // A query-time build reads an ingestion-time input (rows or
            // exact-accumulator state) only from where it is stored, as a
            // `NonASAP` consumer does.
            if timing == ExecutionTiming::QueryTime
                && avail.timing == ExecutionTiming::IngestionTime
                && !physical_properties
                    .get(child)
                    .is_some_and(|p| satisfies_physical(&p, &read_in_the_same_evaluation(timing)))
            {
                return Err(
                    ExecutionDataStateError::UnstoredIngestionInputUnderQueryTimeSummary {
                        edge,
                        operator: child.operator.kind_name(),
                    },
                );
            }
            if timing == ExecutionTiming::IngestionTime
                && avail.timing == ExecutionTiming::QueryTime
            {
                return Err(
                    ExecutionDataStateError::QueryTimeValueUnderIngestionSummary {
                        edge,
                        child: avail,
                    },
                );
            }
            Ok(())
        }
        ASAPOp::SummaryEstimate { summary_input, .. } => {
            let s = state_of(summary_input, physical_properties);
            if s.primitive != DataPrimitive::SummaryState {
                return Err(ExecutionDataStateError::IllegalChildDataState {
                    edge: "SummaryEstimate.summary_input",
                    child: s,
                });
            }
            if timing != ExecutionTiming::QueryTime {
                return Err(ExecutionDataStateError::IllegalChildDataState {
                    edge: "SummaryEstimate",
                    child: state_of(node, physical_properties),
                });
            }
            Ok(())
        }
        ASAPOp::FinalizeExactAccumulator { child } => {
            let s = state_of(child, physical_properties);
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
                && state_of(child, physical_properties)
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
                );
            if !valid {
                return Err(ExecutionDataStateError::InvalidMaintainedPopulation);
            }
            Ok(())
        }
        ASAPOp::SummaryMerge { children, .. } => {
            for child in children {
                let s = state_of(child, physical_properties);
                if s.primitive != DataPrimitive::SummaryState
                    || (timing == ExecutionTiming::IngestionTime && s.timing != timing)
                {
                    return Err(ExecutionDataStateError::IllegalChildDataState {
                        edge: "SummaryMerge.children",
                        child: s,
                    });
                }
            }
            Ok(())
        }
        ASAPOp::SummarySubtract { .. }
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

/// Whether `node` is a filter that keeps whole summary states by their key
/// columns ([`selected_from`](crate::ir::operator::asap::selected_from)).
fn selects_states(node: &OperatorNode) -> bool {
    crate::ir::operator::asap::selected_from(node).is_some()
}

fn validate_non_asap(
    node: &OperatorNode,
    op: &NonASAPOp,
    timing: ExecutionTiming,
    physical_properties: &PhysicalPropertiesAssignment,
) -> Result<(), ExecutionDataStateError> {
    // Every input is rows at this node's own timing. Ingestion work never
    // reads a query-time value; exact-accumulator state may pass through
    // the projection-like operators unchanged.
    for child in op.children() {
        let s = state_of(child, physical_properties);
        let passes_state = matches!(
            op,
            NonASAPOp::Project { .. }
                | NonASAPOp::Filter { .. }
                | NonASAPOp::Sort { .. }
                | NonASAPOp::Limit { .. }
        ) && s.primitive == DataPrimitive::SummaryState
            && (is_exact_accumulator_state(&child.schema).is_ok() || selects_states(node));
        // A query-time consumer may read the stored output of a sub-DAG
        // materialized at ingestion time.
        let reads_stored = physical_properties
            .get(child)
            .is_some_and(|p| satisfies_physical(&p, &read_in_the_same_evaluation(timing)));
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
        // A key filter keeps whole summary states of any family.
        NonASAPOp::Filter { .. } if selects_states(node) => {}
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
                // The opaque series identity is a non-null Utf8 key; every
                // other column is a non-null Float64 value or timestamp.
                let plain_float_or_ts = |schema: &Schema| {
                    schema.fields.iter().all(|field| {
                        !field.nullable
                            && if field.name == crate::ir::schema::PROMQL_SERIES_IDENTITY {
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
                    return Err(ExecutionDataStateError::InvalidIngestionTimeBinary);
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

/// Ingestion-time arithmetic needs the same per-series population on both sides.
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

/// r^phys of a reader at `timing` that reads its input's output in the same
/// evaluation (or ingestion work), as every edge of the timing pass does.
fn read_in_the_same_evaluation(timing: ExecutionTiming) -> PhysicalRequirement {
    PhysicalRequirement::new(timing, Evaluation::Same)
        .expect("a same-evaluation read is valid at either timing")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::operator::agg_intent::AggIntent;
    use crate::ir::operator::non_asap::NonASAPOp;
    use crate::ir::operator::operator_properties::{Reduction, Source};
    use crate::ir::scalar::expr_ir::ColumnRef;
    use crate::ir::schema::sketch::{
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
            SketchKind::new(
                SketchAlgorithm::Kll,
                SketchParams::Kll {
                    k: 200,
                    failure_probability: None,
                },
            ),
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

    /// An assignment that fixes each of `nodes` at ingestion time, not
    /// materialized, as for an operator that must feed maintenance.
    fn fixed_at_ingestion(nodes: &[&Rc<OperatorNode>]) -> PhysicalPropertiesAssignment {
        let mut assignment = PhysicalPropertiesAssignment::default();
        for node in nodes {
            assignment.set(node, PhysicalProperties::NOT_MATERIALIZED_AT_INGESTION);
        }
        assignment
    }

    /// Apply with every sub-DAG materialized at ingestion time where the DAG
    /// allows it, the placement whose edge rules these tests exercise, and
    /// each of `fixed` fixed at ingestion time, not materialized.
    fn apply_fixing(
        root: &Rc<OperatorNode>,
        fixed: &[&Rc<OperatorNode>],
    ) -> Result<PhysicalPropertiesAssignment, ExecutionDataStateError> {
        let mut assignment = PhysicalPropertiesAssignment::materialized_at_ingestion_where_allowed(
            std::slice::from_ref(root),
        );
        for node in fixed {
            assignment.set(node, PhysicalProperties::NOT_MATERIALIZED_AT_INGESTION);
        }
        apply_physical_properties(root, &assignment)
    }

    fn apply(
        root: &Rc<OperatorNode>,
    ) -> Result<PhysicalPropertiesAssignment, ExecutionDataStateError> {
        apply_fixing(root, &[])
    }

    fn timing_of(
        physical_properties: &PhysicalPropertiesAssignment,
        node: &OperatorNode,
    ) -> ExecutionTiming {
        physical_properties.get(node).expect("timed node").timing()
    }

    fn lifetime_of(
        physical_properties: &PhysicalPropertiesAssignment,
        node: &OperatorNode,
    ) -> Lifetime {
        physical_properties
            .get(node)
            .expect("timed node")
            .lifetime()
    }

    const MATERIALIZED_ACROSS: Lifetime = Lifetime::AcrossEvaluations(Medium::Memory);

    fn properties(timing: ExecutionTiming, lifetime: Lifetime) -> PhysicalProperties {
        PhysicalProperties::new(timing, lifetime).unwrap()
    }

    fn child(node: &Rc<OperatorNode>) -> Rc<OperatorNode> {
        Rc::clone(node.children()[0])
    }

    /// Without a materialization decision, a summary is not materialized and
    /// it and its input run at query time; an explicit choice overrides the
    /// default and is recorded in the completed assignment.
    #[test]
    fn default_assignment_materializes_nothing() {
        let summary = agg(scan(), kll());
        let physical_properties =
            apply_physical_properties(&summary, &PhysicalPropertiesAssignment::default()).unwrap();
        assert_eq!(
            data_state(&summary, &physical_properties),
            Some(ExecutionDataState {
                timing: ExecutionTiming::QueryTime,
                primitive: DataPrimitive::SummaryState,
            })
        );
        assert_eq!(lifetime_of(&physical_properties, &summary), Lifetime::None);
        assert_eq!(
            data_state(&child(&summary), &physical_properties),
            Some(ExecutionDataState::QUERY_ROWS)
        );
        for (lifetime, timing) in [
            (MATERIALIZED_ACROSS, ExecutionTiming::IngestionTime),
            (
                Lifetime::OneEvaluation(Medium::Disk),
                ExecutionTiming::QueryTime,
            ),
        ] {
            let mut assignment = PhysicalPropertiesAssignment::default();
            assignment.set(&summary, properties(timing, lifetime));
            let physical_properties = apply_physical_properties(&summary, &assignment).unwrap();
            assert_eq!(timing_of(&physical_properties, &summary), timing);
            assert_eq!(lifetime_of(&physical_properties, &summary), lifetime);
            assert_eq!(timing_of(&physical_properties, &child(&summary)), timing);
            assert_eq!(
                lifetime_of(&physical_properties, &child(&summary)),
                Lifetime::None
            );
        }
    }

    /// #509 Algorithm 3, line 13: an explicit assignment materialized at ingestion time over a state
    /// that only exists at query time is an error, not a silent move to
    /// query time.
    #[test]
    fn ingestion_time_node_over_query_time_upstream_is_rejected() {
        let inner = agg(scan(), exact_sum());
        let outer = agg(Rc::clone(&inner), kll());
        let mut assignment = PhysicalPropertiesAssignment::default();
        assignment.set(
            &outer,
            PhysicalProperties::MATERIALIZED_IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION,
        );
        assert_eq!(
            apply_physical_properties(&estimate(outer), &assignment).err(),
            Some(ExecutionDataStateError::UpstreamNotAtIngestion {
                operator: "SummaryAgg"
            })
        );
        assignment.set(
            &inner,
            PhysicalProperties::MATERIALIZED_IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION,
        );
        let outer = agg(Rc::clone(&inner), kll());
        assignment.set(
            &outer,
            PhysicalProperties::MATERIALIZED_IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION,
        );
        let physical_properties =
            apply_physical_properties(&estimate(Rc::clone(&outer)), &assignment).unwrap();
        assert_eq!(
            lifetime_of(&physical_properties, &outer),
            MATERIALIZED_ACROSS
        );
        assert_eq!(
            lifetime_of(&physical_properties, &inner),
            MATERIALIZED_ACROSS
        );
    }

    #[test]
    fn summary_agg_input_runs_at_ingestion_time() {
        let root = agg(scan(), kll());
        let physical_properties = apply(&root).unwrap();
        assert_eq!(
            data_state(&root, &physical_properties),
            Some(ExecutionDataState::INGESTION_SUMMARY)
        );
        assert_eq!(
            data_state(&child(&root), &physical_properties),
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
        let root = estimate(agg(inner, kll()));
        let physical_properties = apply(&root).unwrap();
        assert_eq!(
            timing_of(&physical_properties, &child(&root)),
            ExecutionTiming::QueryTime
        );
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
            let physical_properties = apply(&root).unwrap();
            assert_eq!(
                data_state(&root, &physical_properties),
                Some(ExecutionDataState::QUERY_ROWS)
            );
        }
    }

    #[test]
    fn query_time_values_can_feed_query_time_summary_construction() {
        let post = max(estimate(agg(scan(), kll())));
        let root = estimate(agg(post, kll()));
        let physical_properties = apply(&root).unwrap();
        assert_eq!(
            timing_of(&physical_properties, &child(&root)),
            ExecutionTiming::QueryTime
        );
    }

    #[test]
    fn function_under_summary_agg_is_legal_but_not_at_root() {
        let operation = max(scan());
        assert_eq!(
            apply_fixing(&operation, &[&operation]).err(),
            Some(ExecutionDataStateError::UnstoredIngestionRoot {
                operator: "Aggregate"
            })
        );
        let root = estimate(agg(Rc::clone(&operation), kll()));
        let physical_properties = apply_fixing(&root, &[&operation]).unwrap();
        assert_eq!(
            data_state(&operation, &physical_properties),
            Some(ExecutionDataState::INGESTION_ROWS)
        );
    }

    #[test]
    fn function_over_evaluation_is_rejected() {
        let operation = max(estimate(agg(scan(), kll())));
        assert_eq!(
            apply_fixing(&estimate(agg(Rc::clone(&operation), kll())), &[&operation]).err(),
            Some(ExecutionDataStateError::UpstreamNotAtIngestion {
                operator: "Aggregate"
            })
        );
    }

    /// One shared sub-DAG reached as ingestion-time input and as query-time
    /// input cannot be executed once for both (Stage 2's
    /// `split_shared_by_timing` copies it first).
    #[test]
    fn a_shared_subtree_reached_at_two_timings_conflicts() {
        let shared = scan();
        let root = OperatorNode::new_shared(crate::ir::Operator::NonASAP(NonASAPOp::Concat {
            children: vec![
                max(estimate(agg(Rc::clone(&shared), kll()))),
                max(Rc::clone(&shared)),
            ],
        }))
        .unwrap();
        assert_eq!(
            apply(&root).err(),
            Some(ExecutionDataStateError::ConflictingTiming {
                first: ExecutionDataState::INGESTION_ROWS,
                second: ExecutionDataState::QUERY_ROWS,
            })
        );
    }

    /// A finalize fixed at ingestion time over a state that is not
    /// materialized at ingestion time names the missing upstream, not an
    /// illegal edge.
    #[test]
    fn ingestion_fixed_finalize_over_query_time_state_is_upstream_not_at_ingestion() {
        let finalize = OperatorNode::new_shared(crate::ir::Operator::ASAP(
            ASAPOp::FinalizeExactAccumulator {
                child: agg(scan(), exact_sum()),
            },
        ))
        .unwrap();
        assert_eq!(
            apply_physical_properties(
                &estimate(agg(Rc::clone(&finalize), kll())),
                &fixed_at_ingestion(&[&finalize]),
            )
            .err(),
            Some(ExecutionDataStateError::UpstreamNotAtIngestion {
                operator: "FinalizeExactAccumulator"
            })
        );
    }

    /// An entry whose lifetime is `None` replaces a materialized one and
    /// records only the node's timing.
    #[test]
    fn setting_a_lifetime_none_records_only_the_timing() {
        let node = max(scan());
        let mut assignment = PhysicalPropertiesAssignment::default();
        assignment.set(
            &node,
            PhysicalProperties::MATERIALIZED_IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION,
        );
        assignment.set(&node, PhysicalProperties::NOT_MATERIALIZED_AT_QUERY_TIME);
        assert_eq!(
            assignment.get(&node),
            Some(PhysicalProperties::NOT_MATERIALIZED_AT_QUERY_TIME)
        );
    }

    /// Any node may be materialized, not only a summary state: stored rows
    /// at ingestion time are read by a query-time consumer, and a node that
    /// can only run at query time cannot be stored at ingestion time.
    #[test]
    fn any_node_may_be_materialized() {
        let tumbling_windows = max(scan());
        let root = max(Rc::clone(&tumbling_windows));
        let mut assignment = PhysicalPropertiesAssignment::default();
        assignment.set(
            &tumbling_windows,
            PhysicalProperties::MATERIALIZED_IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION,
        );
        let physical_properties = apply_physical_properties(&root, &assignment).unwrap();
        assert_eq!(
            timing_of(&physical_properties, &root),
            ExecutionTiming::QueryTime
        );
        assert_eq!(
            lifetime_of(&physical_properties, &tumbling_windows),
            MATERIALIZED_ACROSS
        );
        assert_eq!(
            timing_of(&physical_properties, &tumbling_windows),
            ExecutionTiming::IngestionTime
        );
        assert_eq!(
            timing_of(&physical_properties, &child(&tumbling_windows)),
            ExecutionTiming::IngestionTime
        );

        let over_evaluation = max(estimate(agg(scan(), kll())));
        let mut assignment = PhysicalPropertiesAssignment::default();
        assignment.set(
            &over_evaluation,
            PhysicalProperties::MATERIALIZED_IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION,
        );
        assert_eq!(
            apply_physical_properties(&over_evaluation, &assignment).err(),
            Some(ExecutionDataStateError::UpstreamNotAtIngestion {
                operator: "Aggregate"
            })
        );
    }

    /// A summary build with a filter never runs at ingestion time: an
    /// ingestion-time build reads raw data samples, which carry none of the
    /// query's columns the filter reads. An explicit assignment materialized at ingestion time is an
    /// error; the all-at-ingestion default leaves it at query time.
    #[test]
    fn a_filtered_summary_build_never_runs_at_ingestion_time() {
        use crate::ir::scalar::expr_ir::ScalarValue;
        use crate::ir::{Predicate, ScalarExpr};
        let plain = agg(scan(), kll());
        let Operator::ASAP(ASAPOp::SummaryAgg {
            child: rows,
            family,
            input,
            reduction,
            grouping,
            ..
        }) = plain.operator.clone()
        else {
            unreachable!()
        };
        let filtered = Rc::new(
            OperatorNode::with_schema(
                Operator::ASAP(ASAPOp::SummaryAgg {
                    child: rows,
                    family,
                    input,
                    reduction,
                    grouping,
                    filter: Some(Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true)))),
                }),
                plain.schema.clone(),
            )
            .with_guarantee(None),
        );
        let mut assignment = PhysicalPropertiesAssignment::default();
        assignment.set(
            &filtered,
            PhysicalProperties::MATERIALIZED_IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION,
        );
        assert_eq!(
            apply_physical_properties(&estimate(Rc::clone(&filtered)), &assignment).err(),
            Some(ExecutionDataStateError::FilteredSummaryAtIngestion {
                operator: "SummaryAgg"
            })
        );
        let physical_properties = apply(&estimate(Rc::clone(&filtered))).unwrap();
        assert_eq!(
            timing_of(&physical_properties, &filtered),
            ExecutionTiming::QueryTime
        );
        assert_eq!(lifetime_of(&physical_properties, &filtered), Lifetime::None);
        assert!(validate_at_ingestion(&filtered, ExecutionTiming::QueryTime).is_ok());
    }

    fn finalize(child: Rc<OperatorNode>) -> Rc<OperatorNode> {
        OperatorNode::new_shared(crate::ir::Operator::ASAP(
            ASAPOp::FinalizeExactAccumulator { child },
        ))
        .unwrap()
    }

    /// The lifetime the exported DAG of `root`, timed as `timed`, records
    /// for `node`'s operator kind.
    fn exported(
        root: &Rc<OperatorNode>,
        physical_properties: &PhysicalPropertiesAssignment,
        kind: &str,
    ) -> Lifetime {
        let dag = crate::ir::physical_export::compile_physical_asap_dag(root, physical_properties)
            .unwrap();
        dag.validate().unwrap();
        dag.nodes
            .iter()
            .find(|n| n.payload.kind_name() == kind)
            .unwrap()
            .lifetime
    }

    /// An assignment entry gives a node its timing and lifetime, and the
    /// exported DAG records them.
    #[test]
    fn an_entry_gives_a_node_its_timing_and_lifetime() {
        let read = finalize(agg(scan(), exact_sum()));
        let stored = Lifetime::OneEvaluation(Medium::Disk);
        let mut assignment = PhysicalPropertiesAssignment::default();
        assignment.set(&read, properties(ExecutionTiming::QueryTime, stored));
        let physical_properties = apply_physical_properties(&read, &assignment).unwrap();
        assert_eq!(
            timing_of(&physical_properties, &read),
            ExecutionTiming::QueryTime
        );
        assert_eq!(lifetime_of(&physical_properties, &read), stored);
        assert_eq!(
            exported(&read, &physical_properties, "FinalizeExactAccumulator"),
            stored
        );
    }

    /// The Debug output names each assigned node by its operator kind, not
    /// its whole sub-DAG.
    #[test]
    fn debug_names_each_assigned_node_by_its_kind() {
        let mut assignment = PhysicalPropertiesAssignment::default();
        assignment.set(
            &agg(scan(), kll()),
            PhysicalProperties::MATERIALIZED_IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION,
        );
        let text = format!("{assignment:?}");
        assert!(text.contains("SummaryAgg@"), "{text}");
        assert!(!text.contains("fields"), "{text}");
    }

    /// A query-time summary build cannot read ingestion-time rows that are
    /// not stored: at query time they no longer exist. Storing the rows node
    /// at ingestion time makes the same build valid (#738).
    #[test]
    fn query_time_summary_over_unstored_ingestion_rows_is_rejected() {
        let rows = max(scan());
        let summary = agg(Rc::clone(&rows), kll());
        assert_eq!(
            apply_physical_properties(&summary, &fixed_at_ingestion(&[&rows])).err(),
            Some(
                ExecutionDataStateError::UnstoredIngestionInputUnderQueryTimeSummary {
                    edge: "SummaryAgg.child",
                    operator: "Aggregate",
                }
            )
        );

        let mut assignment = PhysicalPropertiesAssignment::default();
        assignment.set(
            &rows,
            PhysicalProperties::MATERIALIZED_IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION,
        );
        let physical_properties = apply_physical_properties(&summary, &assignment).unwrap();
        assert_eq!(
            timing_of(&physical_properties, &summary),
            ExecutionTiming::QueryTime
        );
        assert_eq!(
            lifetime_of(&physical_properties, &child(&summary)),
            MATERIALIZED_ACROSS
        );
    }

    /// The same holds for exact-accumulator state: a query-time build cannot
    /// read an ingestion-time exact state that is not stored.
    #[test]
    fn query_time_summary_over_unstored_ingestion_state_is_rejected() {
        let state = agg(scan(), exact_sum());
        let summary = agg(Rc::clone(&state), kll());
        assert_eq!(
            apply_physical_properties(&summary, &fixed_at_ingestion(&[&state])).err(),
            Some(
                ExecutionDataStateError::UnstoredIngestionInputUnderQueryTimeSummary {
                    edge: "SummaryAgg.child",
                    operator: "SummaryAgg",
                }
            )
        );

        let mut assignment = PhysicalPropertiesAssignment::default();
        assignment.set(
            &state,
            PhysicalProperties::MATERIALIZED_IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION,
        );
        let physical_properties = apply_physical_properties(&summary, &assignment).unwrap();
        assert_eq!(
            timing_of(&physical_properties, &summary),
            ExecutionTiming::QueryTime
        );
        assert_eq!(
            lifetime_of(&physical_properties, &child(&summary)),
            MATERIALIZED_ACROSS
        );
    }

    /// A root stored at ingestion time is valid: the query reads the stored
    /// output. A root that runs at ingestion time without being stored there
    /// is not.
    #[test]
    fn a_root_may_be_stored_at_ingestion_time() {
        let root = max(scan());
        let mut assignment = PhysicalPropertiesAssignment::default();
        assignment.set(
            &root,
            PhysicalProperties::MATERIALIZED_IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION,
        );
        let physical_properties = apply_physical_properties(&root, &assignment).unwrap();
        assert_eq!(
            timing_of(&physical_properties, &root),
            ExecutionTiming::IngestionTime
        );
        assert_eq!(
            exported(&root, &physical_properties, "Aggregate"),
            MATERIALIZED_ACROSS
        );

        let state = agg(scan(), exact_sum());
        let root = finalize(Rc::clone(&state));
        let mut assignment = PhysicalPropertiesAssignment::default();
        assignment.set(
            &state,
            PhysicalProperties::MATERIALIZED_IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION,
        );
        assignment.set(
            &root,
            PhysicalProperties::MATERIALIZED_IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION,
        );
        let physical_properties = apply_physical_properties(&root, &assignment).unwrap();
        assert_eq!(
            timing_of(&physical_properties, &root),
            ExecutionTiming::IngestionTime
        );
        assert_eq!(
            exported(&root, &physical_properties, "FinalizeExactAccumulator"),
            MATERIALIZED_ACROSS
        );

        let root = max(scan());
        assert_eq!(
            apply_physical_properties(&root, &fixed_at_ingestion(&[&root])).err(),
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
    fn ingestion_time_binary_accepts_only_well_typed_series_identity() {
        use crate::ir::operator::non_asap::BinaryOperator;
        use crate::ir::operator::BinaryOpKind;
        use crate::ir::scalar::ArithmeticOpKind;
        let identity = crate::ir::schema::PROMQL_SERIES_IDENTITY;
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
            validate_at_ingestion(&binary, ExecutionTiming::IngestionTime)
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
            Err(ExecutionDataStateError::InvalidIngestionTimeBinary),
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
                Err(ExecutionDataStateError::InvalidIngestionTimeBinary)
            );
        }
    }
}
