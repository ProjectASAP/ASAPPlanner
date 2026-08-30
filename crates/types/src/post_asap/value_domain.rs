//! Execution-domain contract for mixed exact/summary plans (issue #171).
//!
//! A post-ASAP DAG mixes two very different moments of execution: the
//! **update/ingest path** (rows arrive, maintained summary state is updated)
//! and **query evaluation** (maintained state is read out and a final result
//! is produced). A plan that places a query-time residual *underneath* a
//! maintained summary is not merely expensive — it is unexecutable, because
//! the maintenance loop has no readout values to feed into that summary.
//! [`SummaryExpr::ReadoutPostProcess`] is exactly such a residual, which is
//! why it and [`SummaryExpr::UpdateTransform`] are two separate variants
//! rather than one domain-ambiguous value operation.
//!
//! [`ValueDomain`] is what a node's output *is*, at which domain;
//! [`validate_execution_domains`] checks every edge of a DAG against the
//! rules below at plan construction, returning a typed [`DomainError`] rather
//! than deferring to a runtime failure.
//!
//! ## Edge rules
//!
//! | Parent | Accepts from `child` |
//! |---|---|
//! | `SummaryAgg.child` | `MAINTENANCE_ROWS`, or `MAINTENANCE_SUMMARY` of an **exact accumulator** family. Never a read-time domain. |
//! | `SummaryEstimate.summary_input` | `MAINTENANCE_SUMMARY` (any family). Produces `READ_ROWS`. |
//! | `SummaryJoin.outer/inner` | `MAINTENANCE_ROWS` or `MAINTENANCE_SUMMARY`; never a read-time domain. |
//! | `SummarySubtract`/`SummaryDelete`/`SummaryMerge` | `MAINTENANCE_SUMMARY`. |
//! | `UpdateTransform.child` | `MAINTENANCE_ROWS`. Produces `MAINTENANCE_ROWS`. |
//! | `ReadoutPostProcess.child` | `READ_ROWS`. Produces `READ_ROWS`. |
//!
//! ## `KeepPreAsap` declares its domain through the derivation
//!
//! A [`SummaryExpr::KeepPreAsap`] leaf is a raw pre-ASAP computation that a
//! runtime can execute at either domain: as update-path raw input beneath a
//! `SummaryAgg`/`UpdateTransform`, or as a query-time fallback beneath a
//! `ReadoutPostProcess` (or at the root). It carries no domain field of its own
//! — every existing consumer pattern-matches the one-field shape — so its
//! domain is *assigned* by [`validate_execution_domains`] from the edge that
//! reaches it and reported in the returned [`DomainAssignment`]. What it may
//! not do is stay ambiguous inside one mixed plan: the same `Rc<SummaryNode>`
//! reached once as update input and once as query-time fallback is
//! [`DomainError::AmbiguousKeepPreAsap`], because no single execution of that
//! subtree can serve both roles.

use std::collections::HashMap;
use std::rc::Rc;

use thiserror::Error;

use super::expr::{ExactOperator, SummaryExpr, SummaryNode, ValueOperator};
use super::schema::{SummaryFamilyType, SummaryField, SummarySchema};
use crate::pre_asap::query_expr::{aggregate_output_schema, QueryExprError};
use crate::pre_asap::schema::{Column, Schema};

/// When a post-ASAP value is produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataPrimitive {
    Rows,
    SummaryState,
}

impl DataPrimitive {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rows => "rows",
            Self::SummaryState => "summary_state",
        }
    }
}

/// The two-dimensional edge contract: when a value exists and which data
/// primitive it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ValueDomain {
    pub timing: ExecutionTiming,
    pub primitive: DataPrimitive,
}

impl ValueDomain {
    pub const MAINTENANCE_ROWS: Self = Self {
        timing: ExecutionTiming::MaintenanceTime,
        primitive: DataPrimitive::Rows,
    };
    pub const MAINTENANCE_SUMMARY: Self = Self {
        timing: ExecutionTiming::MaintenanceTime,
        primitive: DataPrimitive::SummaryState,
    };
    pub const READ_ROWS: Self = Self {
        timing: ExecutionTiming::ReadTime,
        primitive: DataPrimitive::Rows,
    };
}

impl std::fmt::Display for ValueDomain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.timing.as_str(), self.primitive.as_str())
    }
}

/// Which parent/edge a [`DomainError`] is about — the variant name of the
/// parent `SummaryExpr` plus its field, for a message a plan author can act
/// on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainEdge {
    SummaryAggChild,
    SummaryEstimateInput,
    SummaryJoinInput,
    SummarySubtractInput,
    SummaryDeleteInput,
    SummaryMergeInput,
    UpdateTransformChild,
    ReadoutPostProcessChild,
}

impl DomainEdge {
    fn describe(self) -> &'static str {
        match self {
            Self::SummaryAggChild => "SummaryAgg.child",
            Self::SummaryEstimateInput => "SummaryEstimate.summary_input",
            Self::SummaryJoinInput => "SummaryJoin.{outer,inner}",
            Self::SummarySubtractInput => "SummarySubtract.{left,right}",
            Self::SummaryDeleteInput => "SummaryDelete.summary_input",
            Self::SummaryMergeInput => "SummaryMerge.children[]",
            Self::UpdateTransformChild => "UpdateTransform.child",
            Self::ReadoutPostProcessChild => "ReadoutPostProcess.child",
        }
    }
}

/// A plan-construction-time domain violation. Typed (not a string) so a
/// strategy can degrade to a conservative fallback on the specific variant
/// it expects, and so tests can assert the *reason* a plan was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DomainError {
    /// A query-time value (`SummaryEstimate` / `ReadoutPostProcess` output)
    /// placed beneath a maintained summary — the one shape issue #171's
    /// domain split exists to make unrepresentable.
    #[error(
        "readout value under maintenance: {edge} received a {child} input, but a maintained \
         summary can only consume update-path values (or exact accumulator state)"
    )]
    ReadoutUnderMaintenance {
        edge: &'static str,
        child: ValueDomain,
    },
    /// Any other edge whose child domain the parent does not accept
    /// (e.g. plain update rows fed straight into a `SummaryEstimate`, or a
    /// sketch's opaque state fed into a `ReadoutPostProcess`).
    #[error("{edge} does not accept a {child} input")]
    IllegalChildPhase {
        edge: &'static str,
        child: ValueDomain,
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
        "KeepPreAsap subtree is domain-ambiguous: reached as {first} and as {second} in the same \
         plan"
    )]
    AmbiguousKeepPreAsap {
        first: ValueDomain,
        second: ValueDomain,
    },
    /// An update-path-only node (`UpdateTransform`) at the root of a plan:
    /// nothing maintains state above it, so its output is never read.
    #[error("UpdateTransform cannot be a plan root: its update-path output feeds nothing")]
    MaintenanceRowsAtRoot,
    /// An `ExactOperator` whose input columns are not all `Plain` at its
    /// declared domain.
    #[error("exact operator consumes non-plain column {column:?} ({dtype})")]
    NonPlainOperand { column: String, dtype: String },
}

/// The domain assigned to every node of a validated plan, keyed by
/// `Rc<SummaryNode>` pointer identity — the explicit per-node "domain" a
/// runtime or a DAG export reads instead of re-deriving it. For every
/// non-`KeepPreAsap` node this equals [`produced_domain`]; for a
/// `KeepPreAsap` leaf it is the domain the reaching edge assigned.
#[derive(Debug, Clone, Default)]
pub struct DomainAssignment {
    domains: HashMap<*const SummaryNode, ValueDomain>,
}

impl DomainAssignment {
    /// The domain assigned to `node`, if it was part of the validated plan.
    pub fn domain_of(&self, node: &Rc<SummaryNode>) -> Option<ValueDomain> {
        self.domains.get(&Rc::as_ptr(node)).copied()
    }

    /// The domain assigned to the node at `ptr` — for callers walking a plan
    /// by reference rather than by `Rc`.
    pub fn domain_of_ptr(&self, ptr: *const SummaryNode) -> Option<ValueDomain> {
        self.domains.get(&ptr).copied()
    }
}

/// The domain `expr` *produces*, independent of context — `None` for
/// [`SummaryExpr::KeepPreAsap`], whose domain is assigned by the edge reaching
/// it (see the module docs).
pub fn produced_domain(expr: &SummaryExpr) -> Option<ValueDomain> {
    Some(match expr {
        SummaryExpr::KeepPreAsap(_) => return None,
        SummaryExpr::SummaryAgg { .. }
        | SummaryExpr::SummaryJoin { .. }
        | SummaryExpr::SummarySubtract { .. }
        | SummaryExpr::SummaryDelete { .. }
        | SummaryExpr::SummaryMerge { .. } => ValueDomain::MAINTENANCE_SUMMARY,
        SummaryExpr::SummaryEstimate { .. } | SummaryExpr::ReadoutPostProcess { .. } => {
            ValueDomain::READ_ROWS
        }
        SummaryExpr::UpdateTransform { .. } => ValueDomain::MAINTENANCE_ROWS,
    })
}

/// Is `family` the exact-accumulator family whose partial state *is* the
/// value — the one summary state a `SummaryAgg` may re-accumulate?
fn is_exact_accumulator_state(schema: &SummarySchema) -> Result<(), DomainError> {
    for field in &schema.fields {
        match &field.dtype {
            SummaryFamilyType::Plain(_) | SummaryFamilyType::ExactAggregate(..) => {}
            other => {
                return Err(DomainError::UnsupportedStateComposition {
                    family: format!("{other:?}"),
                })
            }
        }
    }
    Ok(())
}

/// Validate every edge of the DAG rooted at `root` against the module-level
/// rules, returning each node's assigned domain on success. Shared
/// `Rc<SummaryNode>`s are visited once per reaching edge (the assignment is
/// per node, so a conflict between two edges is what
/// [`DomainError::AmbiguousKeepPreAsap`] detects).
pub fn validate_execution_domains(root: &Rc<SummaryNode>) -> Result<DomainAssignment, DomainError> {
    // The root may be a readable value or bare maintained state (a
    // deployment may hand an `ExactAggregate` accumulator straight to a
    // consumer) — only an update-path-only root is meaningless.
    let root_domain = match produced_domain(&root.expr) {
        None => ValueDomain::READ_ROWS,
        Some(ValueDomain::MAINTENANCE_ROWS) => return Err(DomainError::MaintenanceRowsAtRoot),
        Some(domain) => domain,
    };
    validate_execution_domains_at(root, root_domain)
}

/// [`validate_execution_domains`] for a *sub*-plan whose root is known to
/// sit at `domain` — e.g. an `UpdateTransform` about to be placed beneath a
/// `SummaryAgg`, which would be rejected as a whole-plan root but is a
/// legal update-path input. Validates every edge beneath `root` exactly
/// as the whole-plan entry point does.
pub fn validate_execution_domains_at(
    root: &Rc<SummaryNode>,
    domain: ValueDomain,
) -> Result<DomainAssignment, DomainError> {
    let mut assignment = DomainAssignment::default();
    visit(root, domain, &mut assignment)?;
    Ok(assignment)
}

/// Record `domain` for `node` (detecting a conflicting earlier assignment
/// for a `KeepPreAsap`), then check and recurse into every child edge.
fn visit(
    node: &Rc<SummaryNode>,
    domain: ValueDomain,
    assignment: &mut DomainAssignment,
) -> Result<(), DomainError> {
    let ptr = Rc::as_ptr(node);
    if let Some(previous) = assignment.domains.get(&ptr) {
        if *previous != domain {
            return Err(DomainError::AmbiguousKeepPreAsap {
                first: *previous,
                second: domain,
            });
        }
        // Already validated through another edge with the same domain.
        return Ok(());
    }
    assignment.domains.insert(ptr, domain);

    match &node.expr {
        SummaryExpr::KeepPreAsap(_) => Ok(()),
        SummaryExpr::SummaryAgg { child, .. } => {
            let child_domain =
                child_domain(child, DomainEdge::SummaryAggChild, |avail| match avail {
                    ValueDomain::MAINTENANCE_ROWS => Ok(()),
                    ValueDomain::MAINTENANCE_SUMMARY => is_exact_accumulator_state(&child.schema),
                    other => Err(DomainError::ReadoutUnderMaintenance {
                        edge: DomainEdge::SummaryAggChild.describe(),
                        child: other,
                    }),
                })?;
            visit(child, child_domain, assignment)
        }
        SummaryExpr::SummaryJoin { outer, inner, .. } => {
            for input in [outer, inner] {
                let s = child_domain(input, DomainEdge::SummaryJoinInput, |avail| match avail {
                    ValueDomain::MAINTENANCE_ROWS | ValueDomain::MAINTENANCE_SUMMARY => Ok(()),
                    other => Err(DomainError::ReadoutUnderMaintenance {
                        edge: DomainEdge::SummaryJoinInput.describe(),
                        child: other,
                    }),
                })?;
                visit(input, s, assignment)?;
            }
            Ok(())
        }
        SummaryExpr::SummarySubtract { left, right } => {
            for input in [left, right] {
                let s = state_only(input, DomainEdge::SummarySubtractInput)?;
                visit(input, s, assignment)?;
            }
            Ok(())
        }
        SummaryExpr::SummaryDelete { summary_input, .. } => {
            let s = state_only(summary_input, DomainEdge::SummaryDeleteInput)?;
            visit(summary_input, s, assignment)
        }
        SummaryExpr::SummaryMerge { children } => {
            for input in children {
                let s = state_only(input, DomainEdge::SummaryMergeInput)?;
                visit(input, s, assignment)?;
            }
            Ok(())
        }
        SummaryExpr::SummaryEstimate { summary_input, .. } => {
            let s = state_only(summary_input, DomainEdge::SummaryEstimateInput)?;
            visit(summary_input, s, assignment)
        }
        SummaryExpr::UpdateTransform { child, op } => {
            let s = child_domain(
                child,
                DomainEdge::UpdateTransformChild,
                |avail| match avail {
                    ValueDomain::MAINTENANCE_ROWS => Ok(()),
                    other => Err(DomainError::IllegalChildPhase {
                        edge: DomainEdge::UpdateTransformChild.describe(),
                        child: other,
                    }),
                },
            )?;
            check_plain_operands(op, &child.schema)?;
            visit(child, s, assignment)
        }
        SummaryExpr::ReadoutPostProcess { child, op } => {
            let s = child_domain(
                child,
                DomainEdge::ReadoutPostProcessChild,
                |avail| match avail {
                    ValueDomain::READ_ROWS => Ok(()),
                    other => Err(DomainError::IllegalChildPhase {
                        edge: DomainEdge::ReadoutPostProcessChild.describe(),
                        child: other,
                    }),
                },
            )?;
            check_plain_operands(op, &child.schema)?;
            visit(child, s, assignment)
        }
    }
}

/// The domain `child` takes as a direct input of `parent`, without
/// validating legality — `child`'s own produced domain, or for a
/// `KeepPreAsap` leaf the domain `parent`'s edge assigns it (update-path raw
/// input under maintenance/transform edges, query-time fallback under a
/// post-process, and — meaninglessly, but for a stable answer — maintenance rows
/// under a state-only edge). For DAG export and other reporting that needs
/// an explicit per-node domain even on a plan that
/// [`validate_execution_domains`] would reject.
pub fn assigned_child_domain(parent: &SummaryExpr, child: &SummaryNode) -> ValueDomain {
    if let Some(avail) = produced_domain(&child.expr) {
        return avail;
    }
    match parent {
        SummaryExpr::ReadoutPostProcess { .. } => ValueDomain::READ_ROWS,
        SummaryExpr::KeepPreAsap(_)
        | SummaryExpr::SummaryAgg { .. }
        | SummaryExpr::SummaryJoin { .. }
        | SummaryExpr::SummarySubtract { .. }
        | SummaryExpr::SummaryDelete { .. }
        | SummaryExpr::SummaryEstimate { .. }
        | SummaryExpr::SummaryMerge { .. }
        | SummaryExpr::UpdateTransform { .. } => ValueDomain::MAINTENANCE_ROWS,
    }
}

/// The domain `child` takes on `edge`: its own produced domain
/// (checked via `accept`), or — for a `KeepPreAsap` leaf — the domain the
/// edge assigns it, derived from what that edge accepts.
fn child_domain(
    child: &Rc<SummaryNode>,
    edge: DomainEdge,
    accept: impl Fn(ValueDomain) -> Result<(), DomainError>,
) -> Result<ValueDomain, DomainError> {
    match produced_domain(&child.expr) {
        Some(avail) => {
            accept(avail)?;
            Ok(avail)
        }
        None => {
            // A raw pre-ASAP subtree executes at whichever domain its consumer
            // needs: update-path input for maintenance/transform edges,
            // query-time fallback for a post-process edge. State-only edges
            // can't consume plain rows at all.
            let assigned = match edge {
                DomainEdge::SummaryAggChild
                | DomainEdge::SummaryJoinInput
                | DomainEdge::UpdateTransformChild => ValueDomain::MAINTENANCE_ROWS,
                DomainEdge::ReadoutPostProcessChild => ValueDomain::READ_ROWS,
                DomainEdge::SummaryEstimateInput
                | DomainEdge::SummarySubtractInput
                | DomainEdge::SummaryDeleteInput
                | DomainEdge::SummaryMergeInput => {
                    return Err(DomainError::IllegalChildPhase {
                        edge: edge.describe(),
                        child: ValueDomain::MAINTENANCE_ROWS,
                    })
                }
            };
            accept(assigned)?;
            Ok(assigned)
        }
    }
}

fn state_only(child: &Rc<SummaryNode>, edge: DomainEdge) -> Result<ValueDomain, DomainError> {
    child_domain(child, edge, |avail| match avail {
        ValueDomain::MAINTENANCE_SUMMARY => Ok(()),
        other => Err(DomainError::IllegalChildPhase {
            edge: edge.describe(),
            child: other,
        }),
    })
}

/// The exact operator must consume only `Plain` columns of its input: for
/// an `Aggregate` payload, every grouping key and every measure's input
/// column.
fn check_plain_operands(op: &ValueOperator, input: &SummarySchema) -> Result<(), DomainError> {
    let ValueOperator::Exact(op) = op else {
        return check_all_plain(input);
    };
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
            return Err(DomainError::NonPlainOperand {
                column: field.name.clone(),
                dtype: format!("{:?}", field.dtype),
            });
        }
    }
    Ok(())
}

fn check_all_plain(input: &SummarySchema) -> Result<(), DomainError> {
    for field in &input.fields {
        if !matches!(field.dtype, SummaryFamilyType::Plain(_)) {
            return Err(DomainError::NonPlainOperand {
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
/// exact `ReadoutPostProcess`/`UpdateTransform` never disagrees with the pre-ASAP
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
        let assignment = validate_execution_domains(&root).unwrap();
        assert_eq!(
            assignment.domain_of(&leaf),
            Some(ValueDomain::MAINTENANCE_ROWS)
        );
        assert_eq!(
            assignment.domain_of(&root),
            Some(ValueDomain::MAINTENANCE_SUMMARY)
        );
    }

    #[test]
    fn exact_accumulator_state_may_feed_another_summary_agg() {
        let inner = agg(
            keep(),
            SummaryFamilyType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
        );
        let root = estimate(agg(inner, kll()));
        assert!(validate_execution_domains(&root).is_ok());
    }

    #[test]
    fn readout_under_summary_agg_is_rejected() {
        let inner = estimate(agg(keep(), kll()));
        let root = agg(inner, kll());
        assert!(matches!(
            validate_execution_domains(&root),
            Err(DomainError::ReadoutUnderMaintenance { .. })
        ));
    }

    #[test]
    fn post_process_over_readout_is_legal_and_root_is_readout() {
        let inner = estimate(agg(keep(), kll()));
        let root = Rc::new(SummaryNode {
            expr: SummaryExpr::ReadoutPostProcess {
                child: inner,
                op: ValueOperator::Exact(max_op()),
            },
            schema: plain(&["max"]),
            guarantee: None,
        });
        let assignment = validate_execution_domains(&root).unwrap();
        assert_eq!(assignment.domain_of(&root), Some(ValueDomain::READ_ROWS));
    }

    #[test]
    fn non_exact_operator_uses_the_same_read_domain_contract() {
        let inner = estimate(agg(keep(), kll()));
        let root = Rc::new(SummaryNode {
            expr: SummaryExpr::ReadoutPostProcess {
                child: inner,
                op: ValueOperator::Extension {
                    name: "approximate_calibration".into(),
                },
            },
            schema: plain(&["calibrated"]),
            guarantee: None,
        });

        let assignment = validate_execution_domains(&root).unwrap();
        assert_eq!(assignment.domain_of(&root), Some(ValueDomain::READ_ROWS));
    }

    #[test]
    fn post_process_under_summary_agg_is_rejected() {
        let inner = estimate(agg(keep(), kll()));
        let post = Rc::new(SummaryNode {
            expr: SummaryExpr::ReadoutPostProcess {
                child: inner,
                op: ValueOperator::Exact(max_op()),
            },
            schema: plain(&["max"]),
            guarantee: None,
        });
        let root = agg(post, kll());
        assert_eq!(
            validate_execution_domains(&root).err(),
            Some(DomainError::ReadoutUnderMaintenance {
                edge: "SummaryAgg.child",
                child: ValueDomain::READ_ROWS,
            })
        );
    }

    #[test]
    fn transform_under_summary_agg_is_legal_but_not_at_root() {
        let transform = Rc::new(SummaryNode {
            expr: SummaryExpr::UpdateTransform {
                child: keep(),
                op: ValueOperator::Exact(max_op()),
            },
            schema: plain(&["max"]),
            guarantee: None,
        });
        assert_eq!(
            validate_execution_domains(&transform).err(),
            Some(DomainError::MaintenanceRowsAtRoot)
        );
        let root = estimate(agg(Rc::clone(&transform), kll()));
        let assignment = validate_execution_domains(&root).unwrap();
        assert_eq!(
            assignment.domain_of(&transform),
            Some(ValueDomain::MAINTENANCE_ROWS)
        );
    }

    #[test]
    fn transform_over_readout_is_rejected() {
        let inner = estimate(agg(keep(), kll()));
        let transform = Rc::new(SummaryNode {
            expr: SummaryExpr::UpdateTransform {
                child: inner,
                op: ValueOperator::Exact(max_op()),
            },
            schema: plain(&["max"]),
            guarantee: None,
        });
        let root = agg(transform, kll());
        assert!(matches!(
            validate_execution_domains(&root),
            Err(DomainError::IllegalChildPhase {
                edge: "UpdateTransform.child",
                child: ValueDomain::READ_ROWS
            })
        ));
    }

    #[test]
    fn a_shared_keep_pre_asap_reached_in_two_domains_is_ambiguous() {
        // One raw subtree used both as update input (under a SummaryAgg) and
        // as a query-time fallback (under an ExactPostProcess) — no single
        // execution can serve both, so the plan is rejected.
        let shared = keep();
        let maintained = estimate(agg(Rc::clone(&shared), kll()));
        let post_over_raw = Rc::new(SummaryNode {
            expr: SummaryExpr::ReadoutPostProcess {
                child: Rc::clone(&shared),
                op: ValueOperator::Exact(max_op()),
            },
            schema: plain(&["max"]),
            guarantee: None,
        });
        let root = Rc::new(SummaryNode {
            expr: SummaryExpr::SummaryMerge {
                children: vec![
                    Rc::new(SummaryNode {
                        expr: SummaryExpr::ReadoutPostProcess {
                            child: maintained,
                            op: ValueOperator::Exact(max_op()),
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
        let mut assignment = DomainAssignment::default();
        visit(&shared, ValueDomain::MAINTENANCE_ROWS, &mut assignment).unwrap();
        assert_eq!(
            visit(&shared, ValueDomain::READ_ROWS, &mut assignment),
            Err(DomainError::AmbiguousKeepPreAsap {
                first: ValueDomain::MAINTENANCE_ROWS,
                second: ValueDomain::READ_ROWS,
            })
        );
        assert!(validate_execution_domains(&root).is_err());
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
