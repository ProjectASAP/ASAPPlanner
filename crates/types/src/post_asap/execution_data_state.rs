//! Execution-data-state contract for mixed exact/summary plans (issue #171).
//!
//! A post-ASAP DAG mixes two very different moments of execution: the
//! **update/ingest path** (rows arrive, maintained summary state is updated)
//! and **query evaluation** (maintained state is read out and a final result
//! is produced). A plan that places a query-time residual *underneath* a
//! maintained summary is not merely expensive — it is unexecutable, because
//! the maintenance loop has no readout values to feed into that summary.
//! [`SummaryExpr::ValueOperation`] represents such work without inventing a
//! node per function or use case. Its [`ExecutionTiming`] makes placement
//! explicit and independent of the semantic [`ValueOperation`].
//!
//! [`ExecutionDataState`] is what a node's output *is*, at which data_state;
//! [`validate_execution_data_states`] checks every edge of a DAG against the
//! rules below at plan construction, returning a typed [`ExecutionDataStateError`] rather
//! than deferring to a runtime failure.
//!
//! ## Edge rules
//!
//! | Parent | Accepts from `child` |
//! |---|---|
//! | `SummaryAgg.child` | `MAINTENANCE_ROWS`, or `MAINTENANCE_SUMMARY` of an **exact accumulator** family. Never a read-time data_state. |
//! | `SummaryEstimate.summary_input` | `MAINTENANCE_SUMMARY` (any family). Produces `READ_ROWS`. |
//! | `SummaryJoin.outer/inner` | `MAINTENANCE_ROWS` or `MAINTENANCE_SUMMARY`; never a read-time data_state. |
//! | `SummarySubtract`/`SummaryDelete`/`SummaryMerge` | `MAINTENANCE_SUMMARY`. |
//! | `ValueOperation.child` with `MaintenanceTime` | `MAINTENANCE_ROWS`. Produces `MAINTENANCE_ROWS`. |
//! | `ValueOperation.child` with `ReadTime` | `READ_ROWS`. Produces `READ_ROWS`. |
//!
//! ## `KeepPreAsap` declares its data_state through the derivation
//!
//! A [`SummaryExpr::KeepPreAsap`] leaf is a raw pre-ASAP computation that a
//! runtime can execute at either time: as maintenance input beneath a
//! `SummaryAgg`/maintenance-time `ValueOperation`, or as a query-time fallback
//! beneath a read-time `ValueOperation` (or at the root). It carries no timing
//! field of its own
//! — every existing consumer pattern-matches the one-field shape — so its
//! data_state is *assigned* by [`validate_execution_data_states`] from the edge that
//! reaches it and reported in the returned [`ExecutionDataStateAssignment`]. What it may
//! not do is stay ambiguous inside one mixed plan: the same `Rc<SummaryNode>`
//! reached once as update input and once as query-time fallback is
//! [`ExecutionDataStateError::AmbiguousKeepPreAsap`], because no single execution of that
//! subtree can serve both roles.

use std::collections::HashMap;
use std::rc::Rc;

use thiserror::Error;

use super::expr::{ExactOperation, SummaryExpr, SummaryNode, ValueOperation};
use super::schema::{SummaryFamilyType, SummaryField, SummarySchema};
use crate::pre_asap::query_expr::{aggregate_output_schema, QueryExprError};
use crate::pre_asap::schema::{Column, Schema};

/// When a post-ASAP value is produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ExecutionTiming {
    MaintenanceTime,
    ReadTime,
}

impl ExecutionTiming {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MaintenanceTime => "maintenance_time",
            Self::ReadTime => "read_time",
        }
    }
}

/// The primitive representation carried by a post-ASAP edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DataPrimitive {
    /// Directly usable values, including approximate summary readouts.
    /// This does not imply original input data or an exact guarantee.
    Raw,
    SummaryState,
}

impl DataPrimitive {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::SummaryState => "summary_state",
        }
    }
}

/// The two-dimensional edge contract: when a value exists and which data
/// primitive it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ExecutionDataState {
    pub timing: ExecutionTiming,
    pub primitive: DataPrimitive,
}

impl ExecutionDataState {
    pub const MAINTENANCE_ROWS: Self = Self {
        timing: ExecutionTiming::MaintenanceTime,
        primitive: DataPrimitive::Raw,
    };
    pub const MAINTENANCE_SUMMARY: Self = Self {
        timing: ExecutionTiming::MaintenanceTime,
        primitive: DataPrimitive::SummaryState,
    };
    pub const READ_ROWS: Self = Self {
        timing: ExecutionTiming::ReadTime,
        primitive: DataPrimitive::Raw,
    };
}

impl std::fmt::Display for ExecutionDataState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.timing.as_str(), self.primitive.as_str())
    }
}

/// Which parent/edge a [`ExecutionDataStateError`] is about — the variant name of the
/// parent `SummaryExpr` plus its field, for a message a plan author can act
/// on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionDataStateEdge {
    SummaryAggChild,
    SummaryEstimateInput,
    SummaryJoinInput,
    SummarySubtractInput,
    SummaryDeleteInput,
    SummaryMergeInput,
    ValueOperationChild,
}

impl ExecutionDataStateEdge {
    fn describe(self) -> &'static str {
        match self {
            Self::SummaryAggChild => "SummaryAgg.child",
            Self::SummaryEstimateInput => "SummaryEstimate.summary_input",
            Self::SummaryJoinInput => "SummaryJoin.{outer,inner}",
            Self::SummarySubtractInput => "SummarySubtract.{left,right}",
            Self::SummaryDeleteInput => "SummaryDelete.summary_input",
            Self::SummaryMergeInput => "SummaryMerge.children[]",
            Self::ValueOperationChild => "ValueOperation.child",
        }
    }
}

/// A plan-construction-time data_state violation. Typed (not a string) so a
/// strategy can degrade to a conservative fallback on the specific variant
/// it expects, and so tests can assert the *reason* a plan was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ExecutionDataStateError {
    /// A query-time value (`SummaryEstimate` / read-time `ValueOperation` output)
    /// placed beneath a maintained summary — the one shape issue #171's
    /// data_state split exists to make unrepresentable.
    #[error(
        "readout value under maintenance: {edge} received a {child} input, but a maintained \
         summary can only consume update-path values (or exact accumulator state)"
    )]
    ReadoutUnderMaintenance {
        edge: &'static str,
        child: ExecutionDataState,
    },
    /// Any other edge whose child data_state the parent does not accept
    /// (e.g. plain update rows fed straight into a `SummaryEstimate`, or a
    /// sketch's opaque state fed into a read-time `ValueOperation`).
    #[error("{edge} does not accept a {child} input")]
    IllegalChildDataState {
        edge: &'static str,
        child: ExecutionDataState,
    },
    /// A `SummaryAgg` whose child is summary state of a family other than an
    /// exact accumulator — re-accumulating opaque sketch/sample/… state on
    /// the update path has no defined semantics here.
    #[error(
        "SummaryAgg.child carries {family} summary state; only exact accumulator state can be \
         composed into another maintained summary"
    )]
    UnsupportedStateComposition { family: String },
    /// One shared `KeepPreAsap` node reached both as update-path raw input
    /// and as a query-time fallback — see the module docs.
    #[error(
        "KeepPreAsap subtree is data_state-ambiguous: reached as {first} and as {second} in the same \
         plan"
    )]
    AmbiguousKeepPreAsap {
        first: ExecutionDataState,
        second: ExecutionDataState,
    },
    /// A maintenance-time value operation at the root of a plan:
    /// nothing maintains state above it, so its output is never read.
    #[error("A maintenance-time value operation cannot be a plan root: its update-path output feeds nothing")]
    MaintenanceRowsAtRoot,
    /// An `ExactOperation` whose input columns are not all `Plain` at its
    /// declared data_state.
    #[error("exact operator consumes non-plain column {column:?} ({dtype})")]
    NonPlainOperand { column: String, dtype: String },
}

/// The data_state assigned to every node of a validated plan, keyed by
/// `Rc<SummaryNode>` pointer identity — the explicit per-node "execution_data_state" a
/// runtime or a DAG export reads instead of re-deriving it. For every
/// non-`KeepPreAsap` node this equals [`produced_data_state`]; for a
/// `KeepPreAsap` leaf it is the data_state the reaching edge assigned.
#[derive(Debug, Clone, Default)]
pub struct ExecutionDataStateAssignment {
    domains: HashMap<*const SummaryNode, ExecutionDataState>,
}

impl ExecutionDataStateAssignment {
    /// The data_state assigned to `node`, if it was part of the validated plan.
    pub fn data_state_of(&self, node: &Rc<SummaryNode>) -> Option<ExecutionDataState> {
        self.domains.get(&Rc::as_ptr(node)).copied()
    }

    /// The data_state assigned to the node at `ptr` — for callers walking a plan
    /// by reference rather than by `Rc`.
    pub fn data_state_of_ptr(&self, ptr: *const SummaryNode) -> Option<ExecutionDataState> {
        self.domains.get(&ptr).copied()
    }
}

/// The data_state `expr` *produces*, independent of context — `None` for
/// [`SummaryExpr::KeepPreAsap`], whose data_state is assigned by the edge reaching
/// it (see the module docs).
pub fn produced_data_state(expr: &SummaryExpr) -> Option<ExecutionDataState> {
    Some(match expr {
        SummaryExpr::KeepPreAsap(_) => return None,
        SummaryExpr::BinaryOp { .. }
        | SummaryExpr::CandidateTopK { .. }
        | SummaryExpr::RelationalJoin { .. } => ExecutionDataState::READ_ROWS,
        SummaryExpr::SummaryAgg { .. }
        | SummaryExpr::SummaryJoin { .. }
        | SummaryExpr::SummarySubtract { .. }
        | SummaryExpr::SummaryDelete { .. }
        | SummaryExpr::SummaryMerge { .. } => ExecutionDataState::MAINTENANCE_SUMMARY,
        SummaryExpr::SummaryEstimate { .. } => ExecutionDataState::READ_ROWS,
        SummaryExpr::ValueOperation { timing, .. } => match timing {
            ExecutionTiming::MaintenanceTime => ExecutionDataState::MAINTENANCE_ROWS,
            ExecutionTiming::ReadTime => ExecutionDataState::READ_ROWS,
        },
    })
}

/// Is `family` the exact-accumulator family whose partial state *is* the
/// value — the one summary state a `SummaryAgg` may re-accumulate?
fn is_exact_accumulator_state(schema: &SummarySchema) -> Result<(), ExecutionDataStateError> {
    for field in &schema.fields {
        match &field.dtype {
            SummaryFamilyType::Plain(_) | SummaryFamilyType::ExactAggregate(..) => {}
            other => {
                return Err(ExecutionDataStateError::UnsupportedStateComposition {
                    family: format!("{other:?}"),
                })
            }
        }
    }
    Ok(())
}

/// Validate every edge of the DAG rooted at `root` against the module-level
/// rules, returning each node's assigned data_state on success. Shared
/// `Rc<SummaryNode>`s are visited once per reaching edge (the assignment is
/// per node, so a conflict between two edges is what
/// [`ExecutionDataStateError::AmbiguousKeepPreAsap`] detects).
pub fn validate_execution_data_states(
    root: &Rc<SummaryNode>,
) -> Result<ExecutionDataStateAssignment, ExecutionDataStateError> {
    // The root may be a readable value or bare maintained state (a
    // deployment may hand an `ExactAggregate` accumulator straight to a
    // consumer) — only an update-path-only root is meaningless.
    let root_domain = match produced_data_state(&root.expr) {
        None => ExecutionDataState::READ_ROWS,
        Some(ExecutionDataState::MAINTENANCE_ROWS) => {
            return Err(ExecutionDataStateError::MaintenanceRowsAtRoot)
        }
        Some(data_state) => data_state,
    };
    validate_execution_data_states_at(root, root_domain)
}

/// [`validate_execution_data_states`] for a *sub*-plan whose root is known to
/// sit at `data_state` — e.g. a maintenance-time `ValueOperation` about to be placed beneath a
/// `SummaryAgg`, which would be rejected as a whole-plan root but is a
/// legal update-path input. Validates every edge beneath `root` exactly
/// as the whole-plan entry point does.
pub fn validate_execution_data_states_at(
    root: &Rc<SummaryNode>,
    data_state: ExecutionDataState,
) -> Result<ExecutionDataStateAssignment, ExecutionDataStateError> {
    let mut assignment = ExecutionDataStateAssignment::default();
    visit(root, data_state, &mut assignment)?;
    Ok(assignment)
}

/// Record `data_state` for `node` (detecting a conflicting earlier assignment
/// for a `KeepPreAsap`), then check and recurse into every child edge.
fn visit(
    node: &Rc<SummaryNode>,
    data_state: ExecutionDataState,
    assignment: &mut ExecutionDataStateAssignment,
) -> Result<(), ExecutionDataStateError> {
    let ptr = Rc::as_ptr(node);
    if let Some(previous) = assignment.domains.get(&ptr) {
        if *previous != data_state {
            return Err(ExecutionDataStateError::AmbiguousKeepPreAsap {
                first: *previous,
                second: data_state,
            });
        }
        // Already validated through another edge with the same data_state.
        return Ok(());
    }
    assignment.domains.insert(ptr, data_state);

    match &node.expr {
        SummaryExpr::KeepPreAsap(_) => Ok(()),
        SummaryExpr::BinaryOp { lhs, rhs, .. } => {
            for input in [lhs, rhs] {
                let state =
                    produced_data_state(&input.expr).unwrap_or(ExecutionDataState::READ_ROWS);
                if state != ExecutionDataState::READ_ROWS {
                    return Err(ExecutionDataStateError::IllegalChildDataState {
                        edge: "BinaryOp operand",
                        child: state,
                    });
                }
                visit(input, state, assignment)?;
            }
            Ok(())
        }
        SummaryExpr::CandidateTopK {
            candidates, values, ..
        } => {
            for input in [candidates, values] {
                let state =
                    produced_data_state(&input.expr).unwrap_or(ExecutionDataState::READ_ROWS);
                if state != ExecutionDataState::READ_ROWS {
                    return Err(ExecutionDataStateError::IllegalChildDataState {
                        edge: "CandidateTopK input",
                        child: state,
                    });
                }
                visit(input, state, assignment)?;
            }
            Ok(())
        }
        SummaryExpr::RelationalJoin { left, right, .. } => {
            for input in [left, right] {
                let state =
                    produced_data_state(&input.expr).unwrap_or(ExecutionDataState::READ_ROWS);
                if state != ExecutionDataState::READ_ROWS {
                    return Err(ExecutionDataStateError::IllegalChildDataState {
                        edge: "RelationalJoin input",
                        child: state,
                    });
                }
                visit(input, state, assignment)?;
            }
            Ok(())
        }
        SummaryExpr::SummaryAgg { child, .. } => {
            let child_domain = child_domain(
                child,
                ExecutionDataStateEdge::SummaryAggChild,
                |avail| match avail {
                    ExecutionDataState::MAINTENANCE_ROWS => Ok(()),
                    ExecutionDataState::MAINTENANCE_SUMMARY => {
                        is_exact_accumulator_state(&child.schema)
                    }
                    other => Err(ExecutionDataStateError::ReadoutUnderMaintenance {
                        edge: ExecutionDataStateEdge::SummaryAggChild.describe(),
                        child: other,
                    }),
                },
            )?;
            visit(child, child_domain, assignment)
        }
        SummaryExpr::SummaryJoin { outer, inner, .. } => {
            for input in [outer, inner] {
                let s = child_domain(input, ExecutionDataStateEdge::SummaryJoinInput, |avail| {
                    match avail {
                        ExecutionDataState::MAINTENANCE_ROWS
                        | ExecutionDataState::MAINTENANCE_SUMMARY => Ok(()),
                        other => Err(ExecutionDataStateError::ReadoutUnderMaintenance {
                            edge: ExecutionDataStateEdge::SummaryJoinInput.describe(),
                            child: other,
                        }),
                    }
                })?;
                visit(input, s, assignment)?;
            }
            Ok(())
        }
        SummaryExpr::SummarySubtract { left, right } => {
            for input in [left, right] {
                let s = state_only(input, ExecutionDataStateEdge::SummarySubtractInput)?;
                visit(input, s, assignment)?;
            }
            Ok(())
        }
        SummaryExpr::SummaryDelete { summary_input, .. } => {
            let s = state_only(summary_input, ExecutionDataStateEdge::SummaryDeleteInput)?;
            visit(summary_input, s, assignment)
        }
        SummaryExpr::SummaryMerge { children } => {
            for input in children {
                let s = state_only(input, ExecutionDataStateEdge::SummaryMergeInput)?;
                visit(input, s, assignment)?;
            }
            Ok(())
        }
        SummaryExpr::SummaryEstimate { summary_input, .. } => {
            let s = state_only(summary_input, ExecutionDataStateEdge::SummaryEstimateInput)?;
            visit(summary_input, s, assignment)
        }
        SummaryExpr::ValueOperation {
            child,
            operation,
            timing,
        } => {
            let required = match timing {
                ExecutionTiming::MaintenanceTime => ExecutionDataState::MAINTENANCE_ROWS,
                ExecutionTiming::ReadTime => ExecutionDataState::READ_ROWS,
            };
            let s = produced_data_state(&child.expr).unwrap_or(required);
            let exact_readout = (*timing == ExecutionTiming::ReadTime
                || matches!(operation, ValueOperation::FinalizeExactAccumulator))
                && s == ExecutionDataState::MAINTENANCE_SUMMARY
                && is_exact_accumulator_state(&child.schema).is_ok();
            if s != required && !exact_readout {
                return Err(ExecutionDataStateError::IllegalChildDataState {
                    edge: ExecutionDataStateEdge::ValueOperationChild.describe(),
                    child: s,
                });
            }
            check_plain_operands(operation, &child.schema)?;
            visit(child, s, assignment)
        }
    }
}

/// The data_state `child` takes as a direct input of `parent`, without
/// validating legality — `child`'s own produced data_state, or for a
/// `KeepPreAsap` leaf the data_state `parent`'s edge assigns it (update-path raw
/// input under maintenance-time operation edges, query-time fallback under a
/// a read-time operation, and — meaninglessly, but for a stable answer — maintenance rows
/// under a state-only edge). For DAG export and other reporting that needs
/// an explicit per-node data_state even on a plan that
/// [`validate_execution_data_states`] would reject.
pub fn assigned_child_data_state(parent: &SummaryExpr, child: &SummaryNode) -> ExecutionDataState {
    if let Some(avail) = produced_data_state(&child.expr) {
        return avail;
    }
    match parent {
        SummaryExpr::ValueOperation {
            timing: ExecutionTiming::ReadTime,
            ..
        } => ExecutionDataState::READ_ROWS,
        SummaryExpr::KeepPreAsap(_)
        | SummaryExpr::BinaryOp { .. }
        | SummaryExpr::CandidateTopK { .. }
        | SummaryExpr::RelationalJoin { .. }
        | SummaryExpr::SummaryAgg { .. }
        | SummaryExpr::SummaryJoin { .. }
        | SummaryExpr::SummarySubtract { .. }
        | SummaryExpr::SummaryDelete { .. }
        | SummaryExpr::SummaryEstimate { .. }
        | SummaryExpr::SummaryMerge { .. }
        | SummaryExpr::ValueOperation {
            timing: ExecutionTiming::MaintenanceTime,
            ..
        } => ExecutionDataState::MAINTENANCE_ROWS,
    }
}

/// The data_state `child` takes on `edge`: its own produced data_state
/// (checked via `accept`), or — for a `KeepPreAsap` leaf — the data_state the
/// edge assigns it, derived from what that edge accepts.
fn child_domain(
    child: &Rc<SummaryNode>,
    edge: ExecutionDataStateEdge,
    accept: impl Fn(ExecutionDataState) -> Result<(), ExecutionDataStateError>,
) -> Result<ExecutionDataState, ExecutionDataStateError> {
    match produced_data_state(&child.expr) {
        Some(avail) => {
            accept(avail)?;
            Ok(avail)
        }
        None => {
            // A raw pre-ASAP subtree executes at whichever data_state its consumer
            // needs: update-path input for maintenance-time operation edges,
            // query-time fallback for a read-time edge. State-only edges
            // can't consume plain rows at all.
            let assigned = match edge {
                ExecutionDataStateEdge::SummaryAggChild
                | ExecutionDataStateEdge::SummaryJoinInput
                | ExecutionDataStateEdge::ValueOperationChild => {
                    ExecutionDataState::MAINTENANCE_ROWS
                }
                ExecutionDataStateEdge::SummaryEstimateInput
                | ExecutionDataStateEdge::SummarySubtractInput
                | ExecutionDataStateEdge::SummaryDeleteInput
                | ExecutionDataStateEdge::SummaryMergeInput => {
                    return Err(ExecutionDataStateError::IllegalChildDataState {
                        edge: edge.describe(),
                        child: ExecutionDataState::MAINTENANCE_ROWS,
                    })
                }
            };
            accept(assigned)?;
            Ok(assigned)
        }
    }
}

fn state_only(
    child: &Rc<SummaryNode>,
    edge: ExecutionDataStateEdge,
) -> Result<ExecutionDataState, ExecutionDataStateError> {
    child_domain(child, edge, |avail| match avail {
        ExecutionDataState::MAINTENANCE_SUMMARY => Ok(()),
        other => Err(ExecutionDataStateError::IllegalChildDataState {
            edge: edge.describe(),
            child: other,
        }),
    })
}

/// The exact operator must consume only `Plain` columns of its input: for
/// an `Aggregate` payload, every grouping key and every measure's input
/// column.
fn check_plain_operands(
    op: &ValueOperation,
    input: &SummarySchema,
) -> Result<(), ExecutionDataStateError> {
    if matches!(
        op,
        ValueOperation::Sort { .. }
            | ValueOperation::Limit { .. }
            | ValueOperation::Project { .. }
            | ValueOperation::Filter { .. }
            | ValueOperation::FinalizeExactAccumulator
    ) {
        return check_plain_or_exact_values(input);
    }
    let ValueOperation::Exact(op) = op else {
        return check_all_plain(input);
    };
    let ExactOperation::Aggregate {
        reduction,
        measures,
        ..
    } = op;
    let mut referenced: Vec<usize> = reduction
        .group_keys()
        .map(|keys| keys.keys().to_vec())
        .unwrap_or_default();
    for m in measures {
        if let Some(col) = m.input_col() {
            referenced.push(col);
        }
    }
    // With no explicit input column (the PromQL sample-value convention)
    // the operator reads every non-key column, so all must be plain.
    let implicit = measures.iter().any(|m| m.input_col().is_none());
    for (i, field) in input.fields.iter().enumerate() {
        if !(implicit || referenced.contains(&i)) {
            continue;
        }
        if !matches!(field.dtype, SummaryFamilyType::Plain(_)) {
            return Err(ExecutionDataStateError::NonPlainOperand {
                column: field.name.clone(),
                dtype: format!("{:?}", field.dtype),
            });
        }
    }
    Ok(())
}

fn check_plain_or_exact_values(input: &SummarySchema) -> Result<(), ExecutionDataStateError> {
    for field in &input.fields {
        if !matches!(
            field.dtype,
            SummaryFamilyType::Plain(_) | SummaryFamilyType::ExactAggregate(..)
        ) {
            return Err(ExecutionDataStateError::NonPlainOperand {
                column: field.name.clone(),
                dtype: format!("{:?}", field.dtype),
            });
        }
    }
    Ok(())
}

fn check_all_plain(input: &SummarySchema) -> Result<(), ExecutionDataStateError> {
    for field in &input.fields {
        if !matches!(field.dtype, SummaryFamilyType::Plain(_)) {
            return Err(ExecutionDataStateError::NonPlainOperand {
                column: field.name.clone(),
                dtype: format!("{:?}", field.dtype),
            });
        }
    }
    Ok(())
}

/// The plain pre-ASAP `Schema` underlying an all-`Plain` `SummarySchema`, or
/// `None` if any column carries summary state.
pub fn plain_schema(schema: &SummarySchema) -> Option<Schema> {
    let mut columns = Vec::with_capacity(schema.fields.len());
    for field in &schema.fields {
        let SummaryFamilyType::Plain(dtype) = &field.dtype else {
            return None;
        };
        columns.push(Column::new(&field.name, dtype.clone(), field.nullable));
    }
    Some(Schema {
        columns,
        time_index: schema.time_index,
        unique_keys: Vec::new(),
        closed: true,
    })
}

/// Lift a plain pre-ASAP schema to a `SummarySchema` with every column
/// `Plain` — the output of every exact operator.
pub fn lift_plain(schema: &Schema) -> SummarySchema {
    SummarySchema {
        fields: schema
            .columns
            .iter()
            .map(|c| SummaryField {
                name: c.name.clone(),
                dtype: SummaryFamilyType::Plain(c.dtype.clone()),
                nullable: c.nullable,
            })
            .collect(),
        time_index: schema.time_index,
    }
}

/// Output schema of `op` applied to a child whose edge carries `input` —
/// the same canonical derivation the pre-ASAP `Aggregate` node uses, so an
/// exact `ValueOperation` never disagrees with the pre-ASAP
/// target it was lowered from. `Err` when the child carries non-plain
/// state the operator cannot read.
pub fn exact_operation_output_schema(
    op: &ExactOperation,
    input: &SummarySchema,
) -> Result<SummarySchema, ExactOperationSchemaError> {
    let plain = plain_schema(input).ok_or(ExactOperationSchemaError::NonPlainInput)?;
    let ExactOperation::Aggregate {
        reduction,
        measures,
        output_names,
        ..
    } = op;
    let out = aggregate_output_schema(&plain, reduction, measures, output_names)?;
    Ok(lift_plain(&out))
}

/// Why [`exact_operation_output_schema`] could not derive a schema.
#[derive(Debug, Error)]
pub enum ExactOperationSchemaError {
    #[error("exact operator input carries summary state, not plain columns")]
    NonPlainInput,
    #[error("schema derivation failed: {0}")]
    Schema(#[from] QueryExprError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::post_asap::{ExactKind, ExactParams, GroupingStrategy, SketchQuery};
    use crate::pre_asap::agg_intent::AggIntent;
    use crate::pre_asap::expr_ir::ColumnRef;
    use crate::pre_asap::query_expr::{QueryExpr, Reduction, Source};
    use crate::pre_asap::schema::DataType;

    /// Both execution phases use raw values, distinct from maintained state.
    #[test]
    fn raw_primitive_labels() {
        assert_eq!(
            ExecutionDataState::MAINTENANCE_ROWS.primitive,
            DataPrimitive::Raw
        );
        assert_eq!(ExecutionDataState::READ_ROWS.primitive, DataPrimitive::Raw);
        assert_eq!(
            ExecutionDataState::MAINTENANCE_ROWS.to_string(),
            "maintenance_time/raw"
        );
        assert_eq!(ExecutionDataState::READ_ROWS.to_string(), "read_time/raw");
        assert_eq!(DataPrimitive::SummaryState.as_str(), "summary_state");
    }

    fn scan() -> Rc<QueryExpr> {
        Rc::new(QueryExpr::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Column::new("ts", DataType::Timestamp, false),
                    Column::new("value", DataType::Float64, false),
                    Column::new("zone", DataType::Utf8, true),
                ],
                0,
                vec![],
            ),
        })
    }

    fn keep() -> Rc<SummaryNode> {
        let s = scan();
        let schema = lift_plain(&s.output_schema().unwrap());
        Rc::new(SummaryNode {
            expr: SummaryExpr::KeepPreAsap(s),
            schema,
            guarantee: None,
        })
    }

    fn plain(names: &[&str]) -> SummarySchema {
        SummarySchema {
            fields: names
                .iter()
                .map(|n| SummaryField {
                    name: (*n).into(),
                    dtype: SummaryFamilyType::Plain(DataType::Float64),
                    nullable: false,
                })
                .collect(),
            time_index: None,
        }
    }

    fn agg(child: Rc<SummaryNode>, family: SummaryFamilyType) -> Rc<SummaryNode> {
        Rc::new(SummaryNode {
            expr: SummaryExpr::SummaryAgg {
                child,
                family: family.clone(),
                input: crate::post_asap::SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::by(vec![]),
                grouping: GroupingStrategy::default(),
            },
            schema: SummarySchema {
                fields: vec![SummaryField {
                    name: "state".into(),
                    dtype: family,
                    nullable: false,
                }],
                time_index: None,
            },
            guarantee: None,
        })
    }

    fn kll() -> SummaryFamilyType {
        use crate::post_asap::{SketchAlgorithm, SketchKind, SketchParams};
        SummaryFamilyType::Sketch(
            SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 200 }),
            GroupingStrategy::default(),
        )
    }

    fn estimate(child: Rc<SummaryNode>) -> Rc<SummaryNode> {
        Rc::new(SummaryNode {
            expr: SummaryExpr::SummaryEstimate {
                summary_input: child,
                query: SketchQuery::Quantile { q: 0.99 },
            },
            schema: plain(&["quantile_0_99"]),
            guarantee: None,
        })
    }

    fn max_op() -> ExactOperation {
        ExactOperation::Aggregate {
            reduction: Reduction::by(vec![]),
            measures: vec![AggIntent::Max { col: None }],
            output_names: vec![],
            having: None,
        }
    }

    #[test]
    fn keep_pre_asap_under_summary_agg_is_update_input() {
        let leaf = keep();
        let root = agg(Rc::clone(&leaf), kll());
        let assignment = validate_execution_data_states(&root).unwrap();
        assert_eq!(
            assignment.data_state_of(&leaf),
            Some(ExecutionDataState::MAINTENANCE_ROWS)
        );
        assert_eq!(
            assignment.data_state_of(&root),
            Some(ExecutionDataState::MAINTENANCE_SUMMARY)
        );
    }

    #[test]
    fn exact_accumulator_state_may_feed_another_summary_agg() {
        let inner = agg(
            keep(),
            SummaryFamilyType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
        );
        let root = estimate(agg(inner, kll()));
        assert!(validate_execution_data_states(&root).is_ok());
    }

    #[test]
    fn readout_under_summary_agg_is_rejected() {
        let inner = estimate(agg(keep(), kll()));
        let root = agg(inner, kll());
        assert!(matches!(
            validate_execution_data_states(&root),
            Err(ExecutionDataStateError::ReadoutUnderMaintenance { .. })
        ));
    }

    #[test]
    fn read_time_operation_over_readout_is_legal_and_root_is_readout() {
        let inner = estimate(agg(keep(), kll()));
        let root = Rc::new(SummaryNode {
            expr: SummaryExpr::ValueOperation {
                child: inner,
                operation: ValueOperation::Exact(max_op()),
                timing: ExecutionTiming::ReadTime,
            },
            schema: plain(&["max"]),
            guarantee: None,
        });
        let assignment = validate_execution_data_states(&root).unwrap();
        assert_eq!(
            assignment.data_state_of(&root),
            Some(ExecutionDataState::READ_ROWS)
        );
    }

    #[test]
    fn non_exact_operator_uses_the_same_read_domain_contract() {
        let inner = estimate(agg(keep(), kll()));
        let root = Rc::new(SummaryNode {
            expr: SummaryExpr::ValueOperation {
                child: inner,
                operation: ValueOperation::Extension {
                    name: "approximate_calibration".into(),
                },
                timing: ExecutionTiming::ReadTime,
            },
            schema: plain(&["calibrated"]),
            guarantee: None,
        });

        let assignment = validate_execution_data_states(&root).unwrap();
        assert_eq!(
            assignment.data_state_of(&root),
            Some(ExecutionDataState::READ_ROWS)
        );
    }

    #[test]
    fn read_time_operation_under_summary_agg_is_rejected() {
        let inner = estimate(agg(keep(), kll()));
        let post = Rc::new(SummaryNode {
            expr: SummaryExpr::ValueOperation {
                child: inner,
                operation: ValueOperation::Exact(max_op()),
                timing: ExecutionTiming::ReadTime,
            },
            schema: plain(&["max"]),
            guarantee: None,
        });
        let root = agg(post, kll());
        assert_eq!(
            validate_execution_data_states(&root).err(),
            Some(ExecutionDataStateError::ReadoutUnderMaintenance {
                edge: "SummaryAgg.child",
                child: ExecutionDataState::READ_ROWS,
            })
        );
    }

    #[test]
    fn function_under_summary_agg_is_legal_but_not_at_root() {
        let operation = Rc::new(SummaryNode {
            expr: SummaryExpr::ValueOperation {
                child: keep(),
                operation: ValueOperation::Exact(max_op()),
                timing: ExecutionTiming::MaintenanceTime,
            },
            schema: plain(&["max"]),
            guarantee: None,
        });
        assert_eq!(
            validate_execution_data_states(&operation).err(),
            Some(ExecutionDataStateError::MaintenanceRowsAtRoot)
        );
        let root = estimate(agg(Rc::clone(&operation), kll()));
        let assignment = validate_execution_data_states(&root).unwrap();
        assert_eq!(
            assignment.data_state_of(&operation),
            Some(ExecutionDataState::MAINTENANCE_ROWS)
        );
    }

    #[test]
    fn function_over_readout_is_rejected() {
        let inner = estimate(agg(keep(), kll()));
        let operation = Rc::new(SummaryNode {
            expr: SummaryExpr::ValueOperation {
                child: inner,
                operation: ValueOperation::Exact(max_op()),
                timing: ExecutionTiming::MaintenanceTime,
            },
            schema: plain(&["max"]),
            guarantee: None,
        });
        let root = agg(operation, kll());
        assert!(matches!(
            validate_execution_data_states(&root),
            Err(ExecutionDataStateError::IllegalChildDataState {
                edge: "ValueOperation.child",
                child: ExecutionDataState::READ_ROWS
            })
        ));
    }

    #[test]
    fn a_shared_keep_pre_asap_reached_in_two_domains_is_ambiguous() {
        // One raw subtree used both as update input (under a SummaryAgg) and
        // as a query-time fallback (under an ExactRead) — no single
        // execution can serve both, so the plan is rejected.
        let shared = keep();
        let maintained = estimate(agg(Rc::clone(&shared), kll()));
        let post_over_raw = Rc::new(SummaryNode {
            expr: SummaryExpr::ValueOperation {
                child: Rc::clone(&shared),
                operation: ValueOperation::Exact(max_op()),
                timing: ExecutionTiming::ReadTime,
            },
            schema: plain(&["max"]),
            guarantee: None,
        });
        let root = Rc::new(SummaryNode {
            expr: SummaryExpr::SummaryMerge {
                children: vec![
                    Rc::new(SummaryNode {
                        expr: SummaryExpr::ValueOperation {
                            child: maintained,
                            operation: ValueOperation::Exact(max_op()),
                            timing: ExecutionTiming::ReadTime,
                        },
                        schema: plain(&["max"]),
                        guarantee: None,
                    }),
                    post_over_raw,
                ],
            },
            schema: plain(&["max"]),
            guarantee: None,
        });
        // SummaryMerge only accepts state, so this fails earlier for a
        // different reason; probe the ambiguity through a direct visit.
        let mut assignment = ExecutionDataStateAssignment::default();
        visit(
            &shared,
            ExecutionDataState::MAINTENANCE_ROWS,
            &mut assignment,
        )
        .unwrap();
        assert_eq!(
            visit(&shared, ExecutionDataState::READ_ROWS, &mut assignment),
            Err(ExecutionDataStateError::AmbiguousKeepPreAsap {
                first: ExecutionDataState::MAINTENANCE_ROWS,
                second: ExecutionDataState::READ_ROWS,
            })
        );
        assert!(validate_execution_data_states(&root).is_err());
    }

    #[test]
    fn exact_operator_schema_matches_pre_asap_aggregate_derivation() {
        let child_schema = lift_plain(&scan().output_schema().unwrap());
        let op = ExactOperation::Aggregate {
            reduction: Reduction::by(vec![2]),
            measures: vec![AggIntent::Max { col: None }],
            output_names: vec![],
            having: None,
        };
        let out = exact_operation_output_schema(&op, &child_schema).unwrap();
        let names: Vec<_> = out.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["zone", "max"]);
        assert!(out
            .fields
            .iter()
            .all(|f| matches!(f.dtype, SummaryFamilyType::Plain(_))));
    }

    #[test]
    fn exact_operator_rejects_non_plain_input() {
        let state = agg(keep(), kll());
        assert!(matches!(
            exact_operation_output_schema(&max_op(), &state.schema),
            Err(ExactOperationSchemaError::NonPlainInput)
        ));
    }
}
