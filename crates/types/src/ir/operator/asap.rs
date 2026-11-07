//! ASAP operators: summary-state construction, state operations and evaluations.
//! The summary family, kind/algorithm and parameters are committed here.

use std::rc::Rc;

use serde::{Deserialize, Serialize};

use super::input::{InputOrTemporaryBuild, OperatorInput};
use super::node::{Operator, OperatorNode, OperatorResultKind};
use crate::ir::operator::maintained_population::{MaintainedPopulation, PopulationStatistic};
use crate::ir::operator::operator_properties::Reduction;
use crate::ir::schema::sketch::{GroupingStrategy, SketchStatistic, SummaryUpdate};
use crate::ir::schema::{ColumnId, DataType, FamilyCapability, Field, FieldDataType, Schema};
use crate::ir::SchemaDerivationError;

/// Why an ASAP operator cannot be used yet.
pub const UNIMPLEMENTED_ASAP_OP: &str =
    "this ASAP operator is reserved: schema, accuracy, timing and export are not implemented";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
// `#[serde(default)]` fields would otherwise make serde require `C: Default`.
#[serde(bound(deserialize = "C: Deserialize<'de>"))]
pub enum ASAPOp<C = Rc<OperatorNode>> {
    /// Summary aggregation. Output: grouping columns + one field carrying
    /// partial summary state per group, typed `family`.
    SummaryAgg {
        child: C,
        /// Which summary family realizes this aggregation. Never
        /// `FieldDataType::Plain`.
        family: FieldDataType,
        input: SummaryUpdate,
        reduction: Reduction,
        grouping: GroupingStrategy,
        #[serde(default)]
        filter: Option<crate::ir::scalar::Predicate<C>>,
    },
    /// Evaluate a query result from built summary state. Output is a
    /// row-shaped schema.
    SummaryEstimate {
        summary_input: C,
        query: SketchStatistic,
    },
    /// Read an exact accumulator's state as its finalized value: the
    /// maintenance-to-read boundary before query-time operators.
    FinalizeExactAccumulator {
        child: C,
    },
    /// Maintain the full declared population, including membership changes.
    MaintainPopulation {
        child: C,
        population: MaintainedPopulation<OperatorNode>,
    },
    /// Read an aggregate or TopK prefix from the maintained population.
    EvaluatePopulation {
        child: C,
        evaluation: PopulationStatistic,
    },
    /// Merge compatible partial states of one family: every input state of
    /// a group into one state per group, or, with `group_by`, regrouped.
    /// Over a time-indexed state (a `TimeRange` over a
    /// [`SummaryTumblingWindowChain`](ASAPOp::SummaryTumblingWindowChain)), the windows it
    /// selects; there is no window-selection field.
    SummaryMerge {
        children: Vec<C>,
        /// A rollup (regrouping merge): the input's plain columns the
        /// merged states are keyed by, in output order. Every input state
        /// whose values in these columns match goes into one output state,
        /// so states keyed by `(region, job)` merge into one per `job`.
        /// `None` keeps the input's groups.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group_by: Option<Vec<ColumnId>>,
    },
    /// A chain of tumbling windows (concepts doc, section 3, "Chain
    /// groups"): the `SummaryAgg` with these `family`, `input`, `reduction`,
    /// `grouping` and `filter`, built over each window
    /// `[k·w, (k+1)·w)` of `child`, `w` = `tumbling_window_width_ms`, on a
    /// grid anchored at the epoch. Each window is built when it ends. Output:
    /// one row per window, (window end, keys, state), a state with a time
    /// index. It has no range and no window count: a `TimeRange` (and
    /// `TimeShift`) over it selects the windows a query reads, and how many
    /// windows it retains is $h_v$, a Stage 3 choice.
    // Written as `PaneChain` before #1163; the aliases read those documents.
    #[serde(alias = "PaneChain")]
    SummaryTumblingWindowChain {
        child: C,
        family: FieldDataType,
        input: SummaryUpdate,
        reduction: Reduction,
        grouping: GroupingStrategy,
        #[serde(default)]
        filter: Option<crate::ir::scalar::Predicate<C>>,
        #[serde(alias = "pane_width_ms")]
        tumbling_window_width_ms: u64,
    },
    // ── Reserved: migrated but unimplemented ──
    SummarySubtract {
        left: C,
        right: C,
    },
    SummaryDelete {
        summary_input: C,
        key: ColumnId,
    },
    SummaryJoin {
        outer: C,
        inner: C,
        key: ColumnId,
        family: FieldDataType,
    },
    Extension {
        child: C,
        name: String,
    },
}

impl<C> ASAPOp<C> {
    pub(crate) fn children(&self) -> Vec<&C> {
        use ASAPOp::*;
        match self {
            SummaryAgg { child, filter, .. } | SummaryTumblingWindowChain { child, filter, .. } => {
                let mut inputs = vec![child];
                if let Some(filter) = filter {
                    inputs.extend(filter.0.operator_refs());
                }
                inputs
            }
            FinalizeExactAccumulator { child }
            | MaintainPopulation { child, .. }
            | EvaluatePopulation { child, .. }
            | Extension { child, .. } => vec![child],
            SummaryEstimate { summary_input, .. } | SummaryDelete { summary_input, .. } => {
                vec![summary_input]
            }
            SummarySubtract { left, right } => vec![left, right],
            SummaryJoin { outer, inner, .. } => vec![outer, inner],
            SummaryMerge { children, .. } => children.iter().collect(),
        }
    }

    /// `f` may change the reference type, e.g. from `Rc<OperatorNode>` to a
    /// node id.
    pub(crate) fn map_children<D>(&self, mut f: impl FnMut(&C) -> D) -> ASAPOp<D> {
        use ASAPOp::*;
        match self {
            SummaryAgg {
                child,
                family,
                input,
                reduction,
                grouping,
                filter,
            } => SummaryAgg {
                child: f(child),
                family: family.clone(),
                input: input.clone(),
                reduction: reduction.clone(),
                grouping: grouping.clone(),
                filter: filter
                    .as_ref()
                    .map(|p| crate::ir::scalar::Predicate(p.0.map_operator_refs(&mut f))),
            },
            SummaryTumblingWindowChain {
                child,
                family,
                input,
                reduction,
                grouping,
                filter,
                tumbling_window_width_ms,
            } => SummaryTumblingWindowChain {
                child: f(child),
                family: family.clone(),
                input: input.clone(),
                reduction: reduction.clone(),
                grouping: grouping.clone(),
                filter: filter
                    .as_ref()
                    .map(|p| crate::ir::scalar::Predicate(p.0.map_operator_refs(&mut f))),
                tumbling_window_width_ms: *tumbling_window_width_ms,
            },
            SummaryEstimate {
                summary_input,
                query,
            } => SummaryEstimate {
                summary_input: f(summary_input),
                query: query.clone(),
            },
            FinalizeExactAccumulator { child } => FinalizeExactAccumulator { child: f(child) },
            MaintainPopulation { child, population } => MaintainPopulation {
                child: f(child),
                population: population.clone(),
            },
            EvaluatePopulation { child, evaluation } => EvaluatePopulation {
                child: f(child),
                evaluation: evaluation.clone(),
            },
            SummaryMerge { children, group_by } => SummaryMerge {
                children: children.iter().map(&mut f).collect(),
                group_by: group_by.clone(),
            },
            SummarySubtract { left, right } => SummarySubtract {
                left: f(left),
                right: f(right),
            },
            SummaryDelete { summary_input, key } => SummaryDelete {
                summary_input: f(summary_input),
                key: *key,
            },
            SummaryJoin {
                outer,
                inner,
                key,
                family,
            } => SummaryJoin {
                outer: f(outer),
                inner: f(inner),
                key: *key,
                family: family.clone(),
            },
            Extension { child, name } => Extension {
                child: f(child),
                name: name.clone(),
            },
        }
    }

    pub(crate) fn kind_name(&self) -> &'static str {
        use ASAPOp::*;
        match self {
            SummaryAgg { .. } => "SummaryAgg",
            SummaryEstimate { .. } => "SummaryEstimate",
            FinalizeExactAccumulator { .. } => "FinalizeExactAccumulator",
            MaintainPopulation { .. } => "MaintainPopulation",
            EvaluatePopulation { .. } => "EvaluatePopulation",
            SummaryMerge { .. } => "SummaryMerge",
            SummaryTumblingWindowChain { .. } => "SummaryTumblingWindowChain",
            SummarySubtract { .. } => "SummarySubtract",
            SummaryDelete { .. } => "SummaryDelete",
            SummaryJoin { .. } => "SummaryJoin",
            Extension { .. } => "Extension",
        }
    }
}

impl<I: OperatorInput> ASAPOp<I> {
    fn unimplemented() -> SchemaDerivationError {
        SchemaDerivationError::InvalidScalarSignature(UNIMPLEMENTED_ASAP_OP.into())
    }

    /// The summary state this operator produces, if it produces state.
    pub fn produced_state(&self) -> Option<&FieldDataType> {
        match self {
            ASAPOp::SummaryAgg { family, .. }
            | ASAPOp::SummaryTumblingWindowChain { family, .. }
            | ASAPOp::SummaryJoin { family, .. } => Some(family),
            ASAPOp::SummaryMerge { children, .. } => children.first().and_then(|child| {
                child
                    .schema()
                    .fields
                    .iter()
                    .find(|field| !field.is_plain())
                    .map(|field| &field.dtype)
            }),
            _ => None,
        }
    }

    /// Output schema derived from the operator and its children. Summary
    /// planning may retain a more specific schema (evaluation column naming)
    /// through [`OperatorNode::with_schema`]; all structural metadata must
    /// still agree with this derivation.
    pub(crate) fn output_schema(&self) -> Result<Schema, SchemaDerivationError> {
        use ASAPOp::*;
        Ok(match self {
            SummaryAgg {
                child,
                family,
                reduction,
                ..
            }
            | SummaryTumblingWindowChain {
                child,
                family,
                reduction,
                ..
            } => summary_agg_schema(child.schema(), family, reduction)?,
            SummaryEstimate {
                summary_input,
                query,
            } => {
                let input = summary_input.schema();
                let (name, mut dtype) = match query {
                    SketchStatistic::Quantile { .. } => ("quantile", DataType::Float64),
                    SketchStatistic::Cardinality => ("cardinality", DataType::Int64),
                    SketchStatistic::PointCount { .. } => ("count", DataType::Int64),
                    SketchStatistic::FrequencyL2 => ("frequency_l2", DataType::Float64),
                    SketchStatistic::FrequencyEntropy => ("frequency_entropy", DataType::Float64),
                    SketchStatistic::TopK { .. } => ("topk", DataType::Utf8),
                };
                if matches!(
                    summary_build(summary_input),
                    Some(
                        SummaryAgg {
                            reduction: Reduction::PerEntity,
                            ..
                        } | SummaryTumblingWindowChain {
                            reduction: Reduction::PerEntity,
                            ..
                        }
                    )
                ) && dtype == DataType::Int64
                {
                    dtype = DataType::Float64;
                }
                if let SketchStatistic::TopK { .. } = query {
                    if let Some(schema) = topk_estimate_schema(summary_input) {
                        return Ok(schema);
                    }
                }
                let nullable = matches!(query, SketchStatistic::Quantile { .. })
                    && quantile_may_be_empty(summary_input);
                let mut schema = input.clone();
                for field in &mut schema.fields {
                    if !field.is_plain() {
                        *field = Field::plain(name, dtype.clone(), nullable);
                    }
                }
                schema
            }
            FinalizeExactAccumulator { child } => {
                // A rollup finalizes as the coarse build it stands for.
                if let Some((build, schema)) = rolled_up_build(child) {
                    return finalized_schema(Some(&build), &schema);
                }
                if super::tumbling_window_chain::merged_chain(child).is_some() {
                    todo!("#1207 PR 1: finalize the windows a TimeRange over a chain selects as one build over that TimeRange")
                }
                finalized_schema(summary_build(child).as_ref(), child.schema())?
            }
            MaintainPopulation { child, .. } => child.schema().clone(),
            EvaluatePopulation { child, evaluation } => {
                use crate::ir::operator::maintained_population::PopulationInput;
                use crate::ir::operator::{AggIntent, GroupKeys};
                let Operator::ASAP(MaintainPopulation {
                    child: source,
                    population,
                }) = child.operator()
                else {
                    return Err(SchemaDerivationError::InvalidScalarSignature(
                        "population evaluation requires maintained membership".into(),
                    ));
                };
                if matches!(evaluation, PopulationStatistic::TopK { .. }) {
                    source.schema().clone()
                } else {
                    let (keys, column) = match &population.input {
                        PopulationInput::Rows {
                            grouping,
                            value_column,
                            ..
                        } => (grouping.clone(), Some(*value_column)),
                        PopulationInput::CurrentSeries(spec) => {
                            let keys = spec
                                .grouping
                                .iter()
                                .map(|name| {
                                    source.schema().column_id(name).ok_or_else(|| {
                                        SchemaDerivationError::InvalidScalarSignature(
                                            "population grouping column is absent".into(),
                                        )
                                    })
                                })
                                .collect::<Result<Vec<_>, _>>()?;
                            (
                                if spec.without {
                                    GroupKeys::without(keys)
                                } else {
                                    GroupKeys::by(keys)
                                },
                                None,
                            )
                        }
                    };
                    let accuracy = crate::ir::properties::AccuracyTarget::exact();
                    let measure = match evaluation {
                        PopulationStatistic::Quantile { q } => AggIntent::Quantile {
                            q: *q,
                            col: column,
                            accuracy,
                        },
                        PopulationStatistic::Sum => AggIntent::Sum { col: column },
                        PopulationStatistic::Count => AggIntent::Count { accuracy },
                        PopulationStatistic::Average => AggIntent::Avg { col: column },
                        PopulationStatistic::TopK { .. } => unreachable!(),
                    };
                    crate::ir::NonASAPOp::Aggregate {
                        child: source.clone(),
                        reduction: Reduction::Reduce(keys),
                        measures: vec![measure],
                        output_names: vec![],
                        filters: vec![],
                        having: None,
                    }
                    .output_schema()?
                }
            }
            SummaryMerge {
                children, group_by, ..
            } => {
                self.validate_inputs()?;
                let input = children[0].schema();
                match group_by {
                    None => input.clone(),
                    Some(keys) => regrouped_schema(input, keys)?,
                }
            }
            SummarySubtract { .. }
            | SummaryDelete { .. }
            | SummaryJoin { .. }
            | Extension { .. } => return Err(Self::unimplemented()),
        })
    }

    pub(crate) fn output_kind(&self) -> OperatorResultKind {
        use ASAPOp::*;
        match self {
            SummaryAgg { .. }
            | SummaryTumblingWindowChain { .. }
            | MaintainPopulation { .. }
            | SummaryMerge { .. }
            | SummarySubtract { .. }
            | SummaryDelete { .. }
            | SummaryJoin { .. }
            | Extension { .. } => OperatorResultKind::State,
            SummaryEstimate { summary_input, .. } => source_kind(summary_input),
            FinalizeExactAccumulator { child } | EvaluatePopulation { child, .. } => {
                source_kind(child)
            }
        }
    }

    /// Local input-contract checks.
    pub(crate) fn validate_inputs(&self) -> Result<(), SchemaDerivationError> {
        use ASAPOp::*;
        let needs_state = |node: &I, what: &str| {
            if node.result_kind() != OperatorResultKind::State {
                Err(SchemaDerivationError::InvalidScalarSignature(format!(
                    "{what} requires summary state as input, got {:?}",
                    node.result_kind()
                )))
            } else {
                Ok(())
            }
        };
        match self {
            SummaryMerge { children, .. } => {
                let Some(first) = children.first() else {
                    return Err(SchemaDerivationError::InvalidScalarSignature(
                        "summary merge requires at least one state input".into(),
                    ));
                };
                // Matching state parameters and grouping positions are necessary;
                // matching names alone cannot prove two states compatible.
                if first
                    .schema()
                    .fields
                    .iter()
                    .filter(|field| !field.is_plain())
                    .count()
                    != 1
                {
                    return Err(SchemaDerivationError::InvalidScalarSignature(
                        "summary merge requires exactly one state column".into(),
                    ));
                }
                for child in children {
                    needs_state(child, "SummaryMerge")?;
                    if child.schema() != first.schema() {
                        return Err(SchemaDerivationError::InvalidScalarSignature(
                            "summary merge inputs must have identical state and grouping schemas"
                                .into(),
                        ));
                    }
                }
                // Equal schemas cannot tell a KLL over `latency` from one over
                // `size`, nor prove the inputs disjoint; summary coverage (#646)
                // decides whether a structurally valid merge is semantically valid.
                Ok(())
            }
            SummaryEstimate {
                summary_input,
                query,
            } => {
                needs_state(summary_input, "SummaryEstimate")?;
                // The family-capability table is the one source of which
                // statistic a state's family answers.
                let states: Vec<_> = summary_input
                    .schema()
                    .fields
                    .iter()
                    .filter(|f| !f.is_plain())
                    .collect();
                let valid = match states.as_slice() {
                    [field] => match &field.dtype {
                        family @ FieldDataType::Sketch(..) => {
                            FamilyCapability::of(family).answers(query)
                        }
                        _ => false,
                    },
                    _ => false,
                };
                if !valid {
                    return Err(SchemaDerivationError::InvalidScalarSignature(
                        "evaluation does not match its summary family".into(),
                    ));
                }
                Ok(())
            }
            FinalizeExactAccumulator { child } => {
                needs_state(child, "FinalizeExactAccumulator")?;
                if child
                    .schema()
                    .fields
                    .iter()
                    .all(|f| !matches!(f.dtype, FieldDataType::ExactAggregate(..)))
                {
                    return Err(SchemaDerivationError::InvalidScalarSignature(
                        "FinalizeExactAccumulator requires exact accumulator state".into(),
                    ));
                }
                Ok(())
            }
            EvaluatePopulation { child, evaluation } => {
                needs_state(child, "EvaluatePopulation")?;
                if !matches!(child.operator(), Operator::ASAP(MaintainPopulation { population, .. }) if population.supports(evaluation))
                {
                    return Err(SchemaDerivationError::InvalidScalarSignature(
                        "population evaluation requires compatible maintained membership".into(),
                    ));
                }
                Ok(())
            }
            SummaryTumblingWindowChain {
                child,
                tumbling_window_width_ms,
                ..
            } => {
                if *tumbling_window_width_ms == 0 {
                    return Err(SchemaDerivationError::InvalidScalarSignature(
                        "a tumbling-window chain needs a positive tumbling-window width".into(),
                    ));
                }
                // The chain is the range selector: it reads the series the
                // selector would, or a SQL table's rows for a trailing window.
                if !matches!(
                    child.result_kind(),
                    OperatorResultKind::InstantVector | OperatorResultKind::Relation
                ) {
                    return Err(SchemaDerivationError::InvalidScalarSignature(format!(
                        "a tumbling-window chain reads an instant vector or a relation, got {:?}",
                        child.result_kind()
                    )));
                }
                self.summary_agg_inputs()
            }
            SummaryAgg { .. } => self.summary_agg_inputs(),
            MaintainPopulation { child, population } => {
                if !population.matches_node(child) {
                    return Err(SchemaDerivationError::InvalidScalarSignature(
                        "population input differs from its membership contract".into(),
                    ));
                }
                Ok(())
            }
            _ => Err(Self::unimplemented()),
        }
    }

    /// The input checks of a `SummaryAgg`, which a `SummaryTumblingWindowChain` shares.
    fn summary_agg_inputs(&self) -> Result<(), SchemaDerivationError> {
        let (ASAPOp::SummaryAgg {
            child,
            family,
            input,
            filter,
            ..
        }
        | ASAPOp::SummaryTumblingWindowChain {
            child,
            family,
            input,
            filter,
            ..
        }) = self
        else {
            unreachable!("called on a summary build");
        };
        if family.is_plain() || child.result_kind() == OperatorResultKind::State {
            return Err(SchemaDerivationError::InvalidScalarSignature(
                "summary aggregation requires values and produces a state family".into(),
            ));
        }
        fn check(
            expr: &crate::ir::schema::SummaryInputExpr,
            schema: &Schema,
        ) -> Result<(), SchemaDerivationError> {
            use crate::ir::schema::SummaryInputExpr;
            match expr {
                SummaryInputExpr::Column(col) => {
                    crate::ir::scalar::resolve_column_ref(col, schema).map_err(|e| {
                        SchemaDerivationError::InvalidScalarSignature(e.to_string())
                    })?;
                }
                SummaryInputExpr::Tuple(items) => {
                    for item in items {
                        check(item, schema)?;
                    }
                }
                _ => {}
            }
            Ok(())
        }
        check(&input.weight, child.schema())?;
        if let Some(item) = &input.item {
            check(item, child.schema())?;
        }
        if let Some(filter) = filter {
            if filter.0.scalar_type(child.schema())?.0 != DataType::Bool {
                return Err(SchemaDerivationError::InvalidScalarSignature(
                    "summary filter must be boolean".into(),
                ));
            }
        }
        Ok(())
    }
}

/// The build a rollup (a `SummaryMerge` with `group_by` of one input)
/// stands for, with its schema: the input's build (one build over a merged
/// chain's window) grouped by the input columns the rollup's keys name.
/// `None` for any other input, or a rollup whose build does not derive.
fn rolled_up_build<I: OperatorInput>(
    state: &I,
) -> Option<(ASAPOp<InputOrTemporaryBuild<I>>, Schema)> {
    let Operator::ASAP(ASAPOp::SummaryMerge {
        children,
        group_by: Some(keys),
        ..
    }) = state.operator()
    else {
        return None;
    };
    let [input] = children.as_slice() else {
        return None;
    };
    let input = selected_from(input).unwrap_or_else(|| input.clone());
    let build = match input.operator() {
        Operator::ASAP(op @ ASAPOp::SummaryAgg { .. }) => {
            op.map_children(|child| InputOrTemporaryBuild::Input(child.clone()))
        }
        _ if super::tumbling_window_chain::merged_chain(&input).is_some() => {
            todo!("#1207 PR 1: roll up the windows a TimeRange over a chain selects as one build over that TimeRange")
        }
        _ => return None,
    };
    let ASAPOp::SummaryAgg {
        child,
        family,
        input: update,
        reduction,
        grouping,
        filter,
    } = build
    else {
        return None;
    };
    let mapped = keys
        .iter()
        .map(|&key| match &reduction {
            Reduction::Reduce(by) if !by.is_without() => by.keys().get(key).copied(),
            Reduction::PerEntity => Some(key),
            Reduction::Reduce(_) => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let reduction = Reduction::by(mapped);
    let schema = summary_agg_schema(child.schema(), &family, &reduction).ok()?;
    Some((
        ASAPOp::SummaryAgg {
            child,
            family,
            input: update,
            reduction,
            grouping,
            filter,
        },
        schema,
    ))
}

/// The schema of a `SummaryAgg` (or `SummaryTumblingWindowChain`) of
/// `family` by `reduction` over rows of schema `child`: the reduction's
/// output columns, its value column replaced by the state.
pub(crate) fn summary_agg_schema(
    child: &Schema,
    family: &FieldDataType,
    reduction: &Reduction,
) -> Result<Schema, SchemaDerivationError> {
    let mut schema = crate::ir::schema::aggregate_schema::aggregate_output_schema(
        child,
        reduction,
        &[crate::ir::operator::AggIntent::Sum { col: None }],
        &[],
    )?;
    let index = match reduction {
        Reduction::PerEntity => crate::ir::scalar::resolve_column_ref(
            &crate::ir::scalar::ColumnRef::SampleValue,
            &schema,
        )
        .map_err(|e| SchemaDerivationError::InvalidScalarSignature(e.to_string()))?,
        Reduction::Reduce(_) => schema.fields.len() - 1,
    };
    schema.fields[index] = Field::new("state", family.clone(), false);
    Ok(schema)
}

/// The schema `FinalizeExactAccumulator` derives over state of
/// `state_schema` that `build` (the state's `SummaryAgg`, if any) builds:
/// each exact aggregate replaced by the value it finalizes to.
fn finalized_schema<J: OperatorInput>(
    build: Option<&ASAPOp<J>>,
    state_schema: &Schema,
) -> Result<Schema, SchemaDerivationError> {
    let value_result = if let Some(ASAPOp::SummaryAgg {
        child: source,
        family: FieldDataType::ExactAggregate(kind, _),
        input,
        reduction,
        filter,
        ..
    }) = build
    {
        {
            use crate::ir::operator::AggIntent;
            use crate::ir::schema::{ExactKind, SummaryInputExpr};
            let column = match &input.weight {
                SummaryInputExpr::Column(col) => Some(
                    crate::ir::scalar::column_resolution::resolve_column_ref(col, source.schema())
                        .map_err(|e| {
                            SchemaDerivationError::InvalidScalarSignature(e.to_string())
                        })?,
                ),
                _ => None,
            };
            let measure = match kind {
                ExactKind::Sum => Some(AggIntent::Sum { col: column }),
                ExactKind::Min => Some(AggIntent::Min { col: column }),
                ExactKind::Max => Some(AggIntent::Max { col: column }),
                ExactKind::Count => Some(AggIntent::Count {
                    accuracy: crate::ir::properties::AccuracyTarget::exact(),
                }),
                ExactKind::Rate => Some(AggIntent::Rate),
                ExactKind::IRate => Some(AggIntent::IRate),
                ExactKind::Increase => Some(AggIntent::Increase),
            };
            measure
                .map(|measure| {
                    crate::ir::NonASAPOp::Aggregate {
                        child: source.clone(),
                        reduction: reduction.clone(),
                        measures: vec![measure],
                        output_names: vec![],
                        // A filtered SUM/MIN/MAX is NULL for a
                        // group without a matching row.
                        filters: vec![filter.clone()],
                        having: None,
                    }
                    .output_schema()
                })
                .transpose()?
                .and_then(|s| match reduction {
                    Reduction::PerEntity => {
                        s.column_id("value").and_then(|i| s.fields.get(i).cloned())
                    }
                    Reduction::Reduce(_) => s.fields.last().cloned(),
                })
        }
    } else {
        None
    };
    let mut out = state_schema.clone();
    for f in &mut out.fields {
        if let FieldDataType::ExactAggregate(kind, _) = &f.dtype {
            if let Some(result) = &value_result {
                // The finalized value replaces the aggregate it
                // realizes, so it takes that aggregate's column.
                f.name = result.name.clone();
                f.dtype = result.dtype.clone();
                f.nullable = result.nullable;
            } else {
                f.dtype = FieldDataType::Plain(finalized_data_type(kind));
            }
        }
    }
    Ok(out)
}

/// The schema of a regrouping `SummaryMerge` over states of schema `input`:
/// the key columns `keys`, in order, then the state column. Each key must
/// be a distinct plain column of `input` other than its time column.
fn regrouped_schema(input: &Schema, keys: &[ColumnId]) -> Result<Schema, SchemaDerivationError> {
    let invalid = |message: &str| SchemaDerivationError::InvalidScalarSignature(message.into());
    let mut fields = Vec::with_capacity(keys.len() + 1);
    for (position, &key) in keys.iter().enumerate() {
        let field = input
            .fields
            .get(key)
            .ok_or_else(|| invalid("a regrouping merge key is not an input column"))?;
        if !field.is_plain() || input.time_index == Some(key) || keys[..position].contains(&key) {
            return Err(invalid(
                "a regrouping merge keys by distinct plain input columns other than time",
            ));
        }
        fields.push(field.clone());
    }
    fields.extend(input.fields.iter().find(|field| !field.is_plain()).cloned());
    // As an aggregate's: one row per key value, and no other column.
    Ok(Schema {
        fields,
        time_index: None,
        unique_keys: if keys.is_empty() {
            Vec::new()
        } else {
            vec![(0..keys.len()).collect()]
        },
        closed: true,
    })
}

/// The plain value an exact accumulator finalizes to.
fn finalized_data_type(kind: &crate::ir::schema::sketch::ExactKind) -> DataType {
    use crate::ir::schema::sketch::ExactKind;
    match kind {
        ExactKind::Count => DataType::Int64,
        _ => DataType::Float64,
    }
}

/// The states `node` selects from when it is a `Filter` that keeps whole
/// summary states by their plain columns (its output is state); `None` for
/// any other input.
pub fn selected_from<I: OperatorInput>(node: &I) -> Option<I> {
    match node.operator() {
        Operator::NonASAP(crate::ir::NonASAPOp::Filter { child, .. })
            if node.result_kind() == OperatorResultKind::State =>
        {
            Some(child)
        }
        _ => None,
    }
}

/// The build whose state this summary state carries: a `SummaryAgg` or
/// `SummaryTumblingWindowChain` itself, or the first input's build under
/// `SummaryMerge`s (merged inputs have one definition, which summary
/// coverage checks). `None` under a regrouping merge, whose states no build
/// carries (its coverage's definition is the build it stands for), and for
/// any other input.
pub fn summary_build<I: OperatorInput>(state: &I) -> Option<ASAPOp<I>> {
    match state.operator() {
        Operator::ASAP(
            op @ (ASAPOp::SummaryAgg { .. } | ASAPOp::SummaryTumblingWindowChain { .. }),
        ) => Some(op),
        Operator::ASAP(ASAPOp::SummaryMerge {
            children,
            group_by: None,
            ..
        }) => summary_build(children.first()?),
        _ => None,
    }
}

/// The category of the values a evaluation of `state` produces: the category
/// of the relational input the state was built from.
fn source_kind<I: OperatorInput>(node: &I) -> OperatorResultKind {
    if let Some(states) = selected_from(node) {
        return source_kind(&states);
    }
    match node.operator() {
        Operator::ASAP(op) => match op.children().first() {
            Some(child) => source_kind(*child),
            None => OperatorResultKind::Relation,
        },
        Operator::NonASAP(_) => match node.result_kind() {
            OperatorResultKind::RangeVector => OperatorResultKind::InstantVector,
            OperatorResultKind::State => OperatorResultKind::Relation,
            other => other,
        },
    }
}

/// The rows a heap sketch's top-k estimate returns: per state row, its `k`
/// heaviest items, each as the state's partition columns, then the item
/// columns of the build's update (the inner group labels, or the PromQL
/// series identity), then the estimated weight, `topk` (Float64). `None`
/// when `state` is not a keyed build whose item columns resolve in its
/// input.
fn topk_estimate_schema<I: OperatorInput>(state: &I) -> Option<Schema> {
    // A rollup reads items as the coarse build it stands for.
    match rolled_up_build(state) {
        Some((build, _)) => topk_schema_of_build(&build, state.schema()),
        None => topk_schema_of_build(&summary_build(state)?, state.schema()),
    }
}

/// [`topk_estimate_schema`] of state of `state_schema` built by `build`.
fn topk_schema_of_build<J: OperatorInput>(
    build: &ASAPOp<J>,
    state_schema: &Schema,
) -> Option<Schema> {
    use crate::ir::schema::SummaryInputExpr;
    let (ASAPOp::SummaryAgg { child, input, .. }
    | ASAPOp::SummaryTumblingWindowChain { child, input, .. }) = build
    else {
        return None;
    };
    let child_schema = child.schema();
    let resolve = |column: &crate::ir::scalar::ColumnRef| {
        crate::ir::scalar::resolve_column_ref(column, child_schema).ok()
    };
    let columns = match input.item.as_ref()? {
        SummaryInputExpr::Column(column) => vec![resolve(column)?],
        SummaryInputExpr::Tuple(items) => items
            .iter()
            .map(|item| match item {
                SummaryInputExpr::Column(column) => resolve(column),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?,
        SummaryInputExpr::EntityIdentity(crate::ir::schema::EntityIdentity::PromqlLabelSet {
            excluding,
        }) => {
            let excluded: Vec<_> = excluding.iter().filter_map(resolve).collect();
            let value = resolve(&crate::ir::scalar::ColumnRef::SampleValue);
            (0..child_schema.fields.len())
                .filter(|index| {
                    Some(*index) != child_schema.time_index
                        && Some(*index) != value
                        && !excluded.contains(index)
                })
                .collect()
        }
        _ => return None,
    };
    // A column/tuple item is completely represented by these output columns.
    // An open PromQL label-set item may also contain unlisted labels, so its
    // visible columns alone cannot certify uniqueness.
    let complete_item_identity = matches!(
        input.item.as_ref()?,
        SummaryInputExpr::Column(_) | SummaryInputExpr::Tuple(_)
    ) || child_schema.closed
        || columns.iter().any(|&index| {
            child_schema.fields[index].name == crate::ir::schema::PROMQL_SERIES_IDENTITY
        });
    let mut fields: Vec<Field> = state_schema
        .fields
        .iter()
        .filter(|field| field.is_plain())
        .cloned()
        .collect();
    for index in columns {
        if !fields
            .iter()
            .any(|field| field.name == child_schema.fields[index].name)
        {
            fields.push(child_schema.fields[index].clone());
        }
    }
    // One row per (partition, item), not one row per partition. Preserve this
    // key through the ranked-row projection used by the logical TopK result.
    let unique_keys = if complete_item_identity && !fields.is_empty() {
        vec![(0..fields.len()).collect()]
    } else {
        Vec::new()
    };
    fields.push(Field::plain("topk", DataType::Float64, false));
    Some(Schema {
        fields,
        unique_keys,
        ..state_schema.clone()
    })
}

/// Whether a quantile read from `state` can have no value to rank, and so is
/// NULL as the SQL aggregate it realizes is: the `SummaryAgg` under it (seen
/// through `SummaryMerge`) is filtered, or, over a relation, is global (zero
/// rows give one empty state) or summarizes a nullable column (a group of
/// NULLs gives an empty state).
fn quantile_may_be_empty<I: OperatorInput>(state: &I) -> bool {
    match state.operator() {
        Operator::ASAP(ASAPOp::SummaryMerge {
            children, group_by, ..
        }) => {
            // A global rollup must also represent the empty input population,
            // even when every existing grouped state is nonempty.
            (source_kind(state) == OperatorResultKind::Relation
                && group_by.as_ref().is_some_and(Vec::is_empty))
                || children.iter().any(quantile_may_be_empty)
        }
        Operator::ASAP(
            ASAPOp::SummaryAgg {
                child,
                input,
                reduction,
                filter,
                ..
            }
            | ASAPOp::SummaryTumblingWindowChain {
                child,
                input,
                reduction,
                filter,
                ..
            },
        ) => {
            if filter.is_some() {
                return true;
            }
            if child.result_kind() != OperatorResultKind::Relation {
                return false;
            }
            let global = matches!(reduction, Reduction::Reduce(by) if by.keys().is_empty());
            let nullable = match &input.weight {
                crate::ir::schema::sketch::SummaryInputExpr::Column(column) => {
                    crate::ir::scalar::resolve_column_ref(column, child.schema())
                        .ok()
                        .and_then(|index| child.schema().fields.get(index))
                        .is_some_and(|field| field.nullable)
                }
                _ => false,
            };
            global || nullable
        }
        // Selecting whole states empties none of them.
        _ => selected_from(state).is_some_and(|states| quantile_may_be_empty(&states)),
    }
}
