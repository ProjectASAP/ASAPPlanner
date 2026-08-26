//! Execution-phase contract for mixed exact/summary plans (issue #171).
//!
//! A post-ASAP DAG mixes two very different moments of execution: the
//! **update/ingest path** (rows arrive, maintained summary state is updated)
//! and **query evaluation** (maintained state is read out and a final result
//! is produced). A plan that places a query-time residual *underneath* a
//! maintained summary is not merely expensive — it is unexecutable, because
//! the maintenance loop has no readout values to feed into that summary.
//! [`SummaryExpr::ExactPostProcess`] is exactly such a residual, which is why
//! it and [`SummaryExpr::ExactTransform`] are two separate variants rather
//! than one phase-ambiguous `ExactOp { child }`.
//!
//! [`ExecutionAvailability`] is what a node's output *is*, at which phase;
//! [`validate_execution_phases`] checks every edge of a DAG against the
//! rules below at plan construction, returning a typed [`PhaseError`] rather
//! than deferring to a runtime failure.
//!
//! ## Edge rules
//!
//! | Parent | Accepts from `child` |
//! |---|---|
//! | `SummaryAgg.child` | `UpdateValue`, or `SummaryState` of an **exact accumulator** family (the one explicitly supported state-composition input — `ExactAggregate` state *is* the value, so it can be re-accumulated on the update path). Never `ReadoutValue`. |
//! | `SummaryEstimate.summary_input` | `SummaryState` (any family). Produces `ReadoutValue`. |
//! | `SummaryJoin.outer/inner` | `UpdateValue` or `SummaryState`; never `ReadoutValue`. |
//! | `SummarySubtract`/`SummaryDelete`/`SummaryMerge` | `SummaryState`. |
//! | `ExactTransform.child` | `UpdateValue`. Produces `UpdateValue`. |
//! | `ExactPostProcess.child` | `ReadoutValue`. Produces `ReadoutValue`. |
//!
//! ## `KeepPreAsap` declares its phase through the derivation
//!
//! A [`SummaryExpr::KeepPreAsap`] leaf is a raw pre-ASAP computation that a
//! runtime can execute at either phase: as update-path raw input beneath a
//! `SummaryAgg`/`ExactTransform`, or as a query-time fallback beneath an
//! `ExactPostProcess` (or at the root). It carries no phase field of its own
//! — every existing consumer pattern-matches the one-field shape — so its
//! phase is *assigned* by [`validate_execution_phases`] from the edge that
//! reaches it and reported in the returned [`PhaseAssignment`]. What it may
//! not do is stay ambiguous inside one mixed plan: the same `Rc<SummaryNode>`
//! reached once as update input and once as query-time fallback is
//! [`PhaseError::AmbiguousKeepPreAsap`], because no single execution of that
//! subtree can serve both roles.

use std::collections::HashMap;
use std::rc::Rc;

use thiserror::Error;

use super::expr::{ExactOperator, SummaryExpr, SummaryNode};
use super::schema::{SummaryFamilyType, SummaryField, SummarySchema};
use crate::pre_asap::query_expr::{aggregate_output_schema, QueryExprError};
use crate::pre_asap::schema::{Column, Schema};

/// What a post-ASAP node's output is, and at which execution phase it
/// exists — the edge-level contract [`validate_execution_phases`] enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExecutionAvailability {
    /// Plain rows available on the update/ingest path, while maintaining
    /// downstream state.
    UpdateValue,
    /// Partial, mergeable summary state — not directly readable as a plain
    /// value (except for exact accumulators, whose state *is* the value).
    SummaryState,
    /// Plain values available at query evaluation, after a readout.
    ReadoutValue,
}

impl ExecutionAvailability {
    /// Stable lower-case name for JSON/DAG export (`"update_value"`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UpdateValue => "update_value",
            Self::SummaryState => "summary_state",
            Self::ReadoutValue => "readout_value",
        }
    }
}

impl std::fmt::Display for ExecutionAvailability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which parent/edge a [`PhaseError`] is about — the variant name of the
/// parent `SummaryExpr` plus its field, for a message a plan author can act
/// on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseEdge {
    SummaryAggChild,
    SummaryEstimateInput,
    SummaryJoinInput,
    SummarySubtractInput,
    SummaryDeleteInput,
    SummaryMergeInput,
    ExactTransformChild,
    ExactPostProcessChild,
}

impl PhaseEdge {
    fn describe(self) -> &'static str {
        match self {
            Self::SummaryAggChild => "SummaryAgg.child",
            Self::SummaryEstimateInput => "SummaryEstimate.summary_input",
            Self::SummaryJoinInput => "SummaryJoin.{outer,inner}",
            Self::SummarySubtractInput => "SummarySubtract.{left,right}",
            Self::SummaryDeleteInput => "SummaryDelete.summary_input",
            Self::SummaryMergeInput => "SummaryMerge.children[]",
            Self::ExactTransformChild => "ExactTransform.child",
            Self::ExactPostProcessChild => "ExactPostProcess.child",
        }
    }
}

/// A plan-construction-time phase violation. Typed (not a string) so a
/// strategy can degrade to a conservative fallback on the specific variant
/// it expects, and so tests can assert the *reason* a plan was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PhaseError {
    /// A query-time value (`SummaryEstimate` / `ExactPostProcess` output)
    /// placed beneath a maintained summary — the one shape issue #171's
    /// phase split exists to make unrepresentable.
    #[error(
        "readout value under maintenance: {edge} received a {child} input, but a maintained \
         summary can only consume update-path values (or exact accumulator state)"
    )]
    ReadoutUnderMaintenance {
        edge: &'static str,
        child: ExecutionAvailability,
    },
    /// Any other edge whose child availability the parent does not accept
    /// (e.g. plain update rows fed straight into a `SummaryEstimate`, or a
    /// sketch's opaque state fed into an `ExactPostProcess`).
    #[error("{edge} does not accept a {child} input")]
    IllegalChildPhase {
        edge: &'static str,
        child: ExecutionAvailability,
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
        "KeepPreAsap subtree is phase-ambiguous: reached as {first} and as {second} in the same \
         plan"
    )]
    AmbiguousKeepPreAsap {
        first: ExecutionAvailability,
        second: ExecutionAvailability,
    },
    /// An update-path-only node (`ExactTransform`) at the root of a plan:
    /// nothing maintains state above it, so its output is never read.
    #[error("ExactTransform cannot be a plan root: its update-path output feeds nothing")]
    UpdateValueAtRoot,
    /// An `ExactOperator` whose input columns are not all `Plain` at its
    /// declared phase.
    #[error("exact operator consumes non-plain column {column:?} ({dtype})")]
    NonPlainOperand { column: String, dtype: String },
}

/// The phase assigned to every node of a validated plan, keyed by
/// `Rc<SummaryNode>` pointer identity — the explicit per-node "stage" a
/// runtime or a DAG export reads instead of re-deriving it. For every
/// non-`KeepPreAsap` node this equals [`produced_availability`]; for a
/// `KeepPreAsap` leaf it is the phase the reaching edge assigned.
#[derive(Debug, Clone, Default)]
pub struct PhaseAssignment {
    stages: HashMap<*const SummaryNode, ExecutionAvailability>,
}

impl PhaseAssignment {
    /// The stage assigned to `node`, if it was part of the validated plan.
    pub fn stage_of(&self, node: &Rc<SummaryNode>) -> Option<ExecutionAvailability> {
        self.stages.get(&Rc::as_ptr(node)).copied()
    }

    /// The stage assigned to the node at `ptr` — for callers walking a plan
    /// by reference rather than by `Rc`.
    pub fn stage_of_ptr(&self, ptr: *const SummaryNode) -> Option<ExecutionAvailability> {
        self.stages.get(&ptr).copied()
    }
}

/// The availability `expr` *produces*, independent of context — `None` for
/// [`SummaryExpr::KeepPreAsap`], whose phase is assigned by the edge reaching
/// it (see the module docs).
pub fn produced_availability(expr: &SummaryExpr) -> Option<ExecutionAvailability> {
    Some(match expr {
        SummaryExpr::KeepPreAsap(_) => return None,
        SummaryExpr::SummaryAgg { .. }
        | SummaryExpr::SummaryJoin { .. }
        | SummaryExpr::SummarySubtract { .. }
        | SummaryExpr::SummaryDelete { .. }
        | SummaryExpr::SummaryMerge { .. } => ExecutionAvailability::SummaryState,
        SummaryExpr::SummaryEstimate { .. } | SummaryExpr::ExactPostProcess { .. } => {
            ExecutionAvailability::ReadoutValue
        }
        SummaryExpr::ExactTransform { .. } => ExecutionAvailability::UpdateValue,
    })
}

/// Is `family` the exact-accumulator family whose partial state *is* the
/// value — the one summary state a `SummaryAgg` may re-accumulate?
fn is_exact_accumulator_state(schema: &SummarySchema) -> Result<(), PhaseError> {
    for field in &schema.fields {
        match &field.dtype {
            SummaryFamilyType::Plain(_) | SummaryFamilyType::ExactAggregate(..) => {}
            other => {
                return Err(PhaseError::UnsupportedStateComposition {
                    family: format!("{other:?}"),
                })
            }
        }
    }
    Ok(())
}

/// Validate every edge of the DAG rooted at `root` against the module-level
/// rules, returning each node's assigned stage on success. Shared
/// `Rc<SummaryNode>`s are visited once per reaching edge (the assignment is
/// per node, so a conflict between two edges is what
/// [`PhaseError::AmbiguousKeepPreAsap`] detects).
pub fn validate_execution_phases(root: &Rc<SummaryNode>) -> Result<PhaseAssignment, PhaseError> {
    // The root may be a readable value or bare maintained state (a
    // deployment may hand an `ExactAggregate` accumulator straight to a
    // consumer) — only an update-path-only root is meaningless.
    let root_stage = match produced_availability(&root.expr) {
        None => ExecutionAvailability::ReadoutValue,
        Some(ExecutionAvailability::UpdateValue) => return Err(PhaseError::UpdateValueAtRoot),
        Some(stage) => stage,
    };
    validate_execution_phases_at(root, root_stage)
}

/// [`validate_execution_phases`] for a *sub*-plan whose root is known to
/// sit at `stage` — e.g. an `ExactTransform` about to be placed beneath a
/// `SummaryAgg`, which would be rejected as a whole-plan root but is a
/// legal update-path input. Validates every edge beneath `root` exactly
/// as the whole-plan entry point does.
pub fn validate_execution_phases_at(
    root: &Rc<SummaryNode>,
    stage: ExecutionAvailability,
) -> Result<PhaseAssignment, PhaseError> {
    let mut assignment = PhaseAssignment::default();
    visit(root, stage, &mut assignment)?;
    Ok(assignment)
}

/// Record `stage` for `node` (detecting a conflicting earlier assignment
/// for a `KeepPreAsap`), then check and recurse into every child edge.
fn visit(
    node: &Rc<SummaryNode>,
    stage: ExecutionAvailability,
    assignment: &mut PhaseAssignment,
) -> Result<(), PhaseError> {
    let ptr = Rc::as_ptr(node);
    if let Some(previous) = assignment.stages.get(&ptr) {
        if *previous != stage {
            return Err(PhaseError::AmbiguousKeepPreAsap {
                first: *previous,
                second: stage,
            });
        }
        // Already validated through another edge with the same stage.
        return Ok(());
    }
    assignment.stages.insert(ptr, stage);

    match &node.expr {
        SummaryExpr::KeepPreAsap(_) => Ok(()),
        SummaryExpr::SummaryAgg { child, .. } => {
            let child_stage =
                child_stage(child, PhaseEdge::SummaryAggChild, |avail| match avail {
                    ExecutionAvailability::UpdateValue => Ok(()),
                    ExecutionAvailability::SummaryState => {
                        is_exact_accumulator_state(&child.schema)
                    }
                    ExecutionAvailability::ReadoutValue => {
                        Err(PhaseError::ReadoutUnderMaintenance {
                            edge: PhaseEdge::SummaryAggChild.describe(),
                            child: avail,
                        })
                    }
                })?;
            visit(child, child_stage, assignment)
        }
        SummaryExpr::SummaryJoin { outer, inner, .. } => {
            for input in [outer, inner] {
                let s = child_stage(input, PhaseEdge::SummaryJoinInput, |avail| match avail {
                    ExecutionAvailability::UpdateValue | ExecutionAvailability::SummaryState => {
                        Ok(())
                    }
                    ExecutionAvailability::ReadoutValue => {
                        Err(PhaseError::ReadoutUnderMaintenance {
                            edge: PhaseEdge::SummaryJoinInput.describe(),
                            child: avail,
                        })
                    }
                })?;
                visit(input, s, assignment)?;
            }
            Ok(())
        }
        SummaryExpr::SummarySubtract { left, right } => {
            for input in [left, right] {
                let s = state_only(input, PhaseEdge::SummarySubtractInput)?;
                visit(input, s, assignment)?;
            }
            Ok(())
        }
        SummaryExpr::SummaryDelete { summary_input, .. } => {
            let s = state_only(summary_input, PhaseEdge::SummaryDeleteInput)?;
            visit(summary_input, s, assignment)
        }
        SummaryExpr::SummaryMerge { children } => {
            for input in children {
                let s = state_only(input, PhaseEdge::SummaryMergeInput)?;
                visit(input, s, assignment)?;
            }
            Ok(())
        }
        SummaryExpr::SummaryEstimate { summary_input, .. } => {
            let s = state_only(summary_input, PhaseEdge::SummaryEstimateInput)?;
            visit(summary_input, s, assignment)
        }
        SummaryExpr::ExactTransform { child, op } => {
            let s = child_stage(child, PhaseEdge::ExactTransformChild, |avail| match avail {
                ExecutionAvailability::UpdateValue => Ok(()),
                other => Err(PhaseError::IllegalChildPhase {
                    edge: PhaseEdge::ExactTransformChild.describe(),
                    child: other,
                }),
            })?;
            check_plain_operands(op, &child.schema)?;
            visit(child, s, assignment)
        }
        SummaryExpr::ExactPostProcess { child, op } => {
            let s = child_stage(
                child,
                PhaseEdge::ExactPostProcessChild,
                |avail| match avail {
                    ExecutionAvailability::ReadoutValue => Ok(()),
                    other => Err(PhaseError::IllegalChildPhase {
                        edge: PhaseEdge::ExactPostProcessChild.describe(),
                        child: other,
                    }),
                },
            )?;
            check_plain_operands(op, &child.schema)?;
            visit(child, s, assignment)
        }
    }
}

/// The stage `child` takes as a direct input of `parent`, without
/// validating legality — `child`'s own produced availability, or for a
/// `KeepPreAsap` leaf the phase `parent`'s edge assigns it (update-path raw
/// input under maintenance/transform edges, query-time fallback under a
/// post-process, and — meaninglessly, but for a stable answer — `UpdateValue`
/// under a state-only edge). For DAG export and other reporting that needs
/// an explicit per-node stage even on a plan that
/// [`validate_execution_phases`] would reject.
pub fn assigned_child_stage(parent: &SummaryExpr, child: &SummaryNode) -> ExecutionAvailability {
    if let Some(avail) = produced_availability(&child.expr) {
        return avail;
    }
    match parent {
        SummaryExpr::ExactPostProcess { .. } => ExecutionAvailability::ReadoutValue,
        SummaryExpr::KeepPreAsap(_)
        | SummaryExpr::SummaryAgg { .. }
        | SummaryExpr::SummaryJoin { .. }
        | SummaryExpr::SummarySubtract { .. }
        | SummaryExpr::SummaryDelete { .. }
        | SummaryExpr::SummaryEstimate { .. }
        | SummaryExpr::SummaryMerge { .. }
        | SummaryExpr::ExactTransform { .. } => ExecutionAvailability::UpdateValue,
    }
}

/// The stage `child` takes on `edge`: its own produced availability
/// (checked via `accept`), or — for a `KeepPreAsap` leaf — the phase the
/// edge assigns it, derived from what that edge accepts.
fn child_stage(
    child: &Rc<SummaryNode>,
    edge: PhaseEdge,
    accept: impl Fn(ExecutionAvailability) -> Result<(), PhaseError>,
) -> Result<ExecutionAvailability, PhaseError> {
    match produced_availability(&child.expr) {
        Some(avail) => {
            accept(avail)?;
            Ok(avail)
        }
        None => {
            // A raw pre-ASAP subtree executes at whichever phase its consumer
            // needs: update-path input for maintenance/transform edges,
            // query-time fallback for a post-process edge. State-only edges
            // can't consume plain rows at all.
            let assigned = match edge {
                PhaseEdge::SummaryAggChild
                | PhaseEdge::SummaryJoinInput
                | PhaseEdge::ExactTransformChild => ExecutionAvailability::UpdateValue,
                PhaseEdge::ExactPostProcessChild => ExecutionAvailability::ReadoutValue,
                PhaseEdge::SummaryEstimateInput
                | PhaseEdge::SummarySubtractInput
                | PhaseEdge::SummaryDeleteInput
                | PhaseEdge::SummaryMergeInput => {
                    return Err(PhaseError::IllegalChildPhase {
                        edge: edge.describe(),
                        child: ExecutionAvailability::UpdateValue,
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
    edge: PhaseEdge,
) -> Result<ExecutionAvailability, PhaseError> {
    child_stage(child, edge, |avail| match avail {
        ExecutionAvailability::SummaryState => Ok(()),
        other => Err(PhaseError::IllegalChildPhase {
            edge: edge.describe(),
            child: other,
        }),
    })
}

/// The exact operator must consume only `Plain` columns of its input: for
/// an `Aggregate` payload, every grouping key and every measure's input
/// column.
fn check_plain_operands(op: &ExactOperator, input: &SummarySchema) -> Result<(), PhaseError> {
    let ExactOperator::Aggregate {
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
            return Err(PhaseError::NonPlainOperand {
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
/// `ExactPostProcess`/`ExactTransform` never disagrees with the pre-ASAP
/// target it was lowered from. `Err` when the child carries non-plain
/// state the operator cannot read.
pub fn exact_operator_output_schema(
    op: &ExactOperator,
    input: &SummarySchema,
) -> Result<SummarySchema, ExactOperatorSchemaError> {
    let plain = plain_schema(input).ok_or(ExactOperatorSchemaError::NonPlainInput)?;
    let ExactOperator::Aggregate {
        reduction,
        measures,
        output_names,
        ..
    } = op;
    let out = aggregate_output_schema(&plain, reduction, measures, output_names)?;
    Ok(lift_plain(&out))
}

/// Why [`exact_operator_output_schema`] could not derive a schema.
#[derive(Debug, Error)]
pub enum ExactOperatorSchemaError {
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
                col: ColumnRef::SampleValue,
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

    fn max_op() -> ExactOperator {
        ExactOperator::Aggregate {
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
        let assignment = validate_execution_phases(&root).unwrap();
        assert_eq!(
            assignment.stage_of(&leaf),
            Some(ExecutionAvailability::UpdateValue)
        );
        assert_eq!(
            assignment.stage_of(&root),
            Some(ExecutionAvailability::SummaryState)
        );
    }

    #[test]
    fn exact_accumulator_state_may_feed_another_summary_agg() {
        let inner = agg(
            keep(),
            SummaryFamilyType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
        );
        let root = estimate(agg(inner, kll()));
        assert!(validate_execution_phases(&root).is_ok());
    }

    #[test]
    fn readout_under_summary_agg_is_rejected() {
        let inner = estimate(agg(keep(), kll()));
        let root = agg(inner, kll());
        assert!(matches!(
            validate_execution_phases(&root),
            Err(PhaseError::ReadoutUnderMaintenance { .. })
        ));
    }

    #[test]
    fn post_process_over_readout_is_legal_and_root_is_readout() {
        let inner = estimate(agg(keep(), kll()));
        let root = Rc::new(SummaryNode {
            expr: SummaryExpr::ExactPostProcess {
                child: inner,
                op: max_op(),
            },
            schema: plain(&["max"]),
            guarantee: None,
        });
        let assignment = validate_execution_phases(&root).unwrap();
        assert_eq!(
            assignment.stage_of(&root),
            Some(ExecutionAvailability::ReadoutValue)
        );
    }

    #[test]
    fn post_process_under_summary_agg_is_rejected() {
        let inner = estimate(agg(keep(), kll()));
        let post = Rc::new(SummaryNode {
            expr: SummaryExpr::ExactPostProcess {
                child: inner,
                op: max_op(),
            },
            schema: plain(&["max"]),
            guarantee: None,
        });
        let root = agg(post, kll());
        assert_eq!(
            validate_execution_phases(&root).err(),
            Some(PhaseError::ReadoutUnderMaintenance {
                edge: "SummaryAgg.child",
                child: ExecutionAvailability::ReadoutValue,
            })
        );
    }

    #[test]
    fn transform_under_summary_agg_is_legal_but_not_at_root() {
        let transform = Rc::new(SummaryNode {
            expr: SummaryExpr::ExactTransform {
                child: keep(),
                op: max_op(),
            },
            schema: plain(&["max"]),
            guarantee: None,
        });
        assert_eq!(
            validate_execution_phases(&transform).err(),
            Some(PhaseError::UpdateValueAtRoot)
        );
        let root = estimate(agg(Rc::clone(&transform), kll()));
        let assignment = validate_execution_phases(&root).unwrap();
        assert_eq!(
            assignment.stage_of(&transform),
            Some(ExecutionAvailability::UpdateValue)
        );
    }

    #[test]
    fn transform_over_readout_is_rejected() {
        let inner = estimate(agg(keep(), kll()));
        let transform = Rc::new(SummaryNode {
            expr: SummaryExpr::ExactTransform {
                child: inner,
                op: max_op(),
            },
            schema: plain(&["max"]),
            guarantee: None,
        });
        let root = agg(transform, kll());
        assert!(matches!(
            validate_execution_phases(&root),
            Err(PhaseError::IllegalChildPhase {
                edge: "ExactTransform.child",
                child: ExecutionAvailability::ReadoutValue
            })
        ));
    }

    #[test]
    fn a_shared_keep_pre_asap_reached_at_two_phases_is_ambiguous() {
        // One raw subtree used both as update input (under a SummaryAgg) and
        // as a query-time fallback (under an ExactPostProcess) — no single
        // execution can serve both, so the plan is rejected.
        let shared = keep();
        let maintained = estimate(agg(Rc::clone(&shared), kll()));
        let post_over_raw = Rc::new(SummaryNode {
            expr: SummaryExpr::ExactPostProcess {
                child: Rc::clone(&shared),
                op: max_op(),
            },
            schema: plain(&["max"]),
            guarantee: None,
        });
        let root = Rc::new(SummaryNode {
            expr: SummaryExpr::SummaryMerge {
                children: vec![
                    Rc::new(SummaryNode {
                        expr: SummaryExpr::ExactPostProcess {
                            child: maintained,
                            op: max_op(),
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
        let mut assignment = PhaseAssignment::default();
        visit(&shared, ExecutionAvailability::UpdateValue, &mut assignment).unwrap();
        assert_eq!(
            visit(
                &shared,
                ExecutionAvailability::ReadoutValue,
                &mut assignment
            ),
            Err(PhaseError::AmbiguousKeepPreAsap {
                first: ExecutionAvailability::UpdateValue,
                second: ExecutionAvailability::ReadoutValue,
            })
        );
        assert!(validate_execution_phases(&root).is_err());
    }

    #[test]
    fn exact_operator_schema_matches_pre_asap_aggregate_derivation() {
        let child_schema = lift_plain(&scan().output_schema().unwrap());
        let op = ExactOperator::Aggregate {
            reduction: Reduction::by(vec![2]),
            measures: vec![AggIntent::Max { col: None }],
            output_names: vec![],
            having: None,
        };
        let out = exact_operator_output_schema(&op, &child_schema).unwrap();
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
            exact_operator_output_schema(&max_op(), &state.schema),
            Err(ExactOperatorSchemaError::NonPlainInput)
        ));
    }
}
