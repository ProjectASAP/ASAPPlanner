//! ASAP operators: summary-state construction, state operations and evaluations.
//! The summary family, kind/algorithm and parameters are committed here.

use std::rc::Rc;

use serde::{Deserialize, Serialize};

use crate::ir::operator::maintained_population::{MaintainedPopulation, PopulationStatistic};
use crate::ir::operator::node::{OperatorNode, OperatorResultKind};
use crate::ir::operator::Reduction;
use crate::ir::schema::{ColumnId, DataType, Field, FieldDataType, Schema};
use crate::ir::schema::{GroupingStrategy, SketchStatistic, SummaryUpdate};
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
    /// Read out a query result from built summary state. Output is a
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
    /// Merge compatible partial states for the same grouping and family.
    SummaryMerge {
        children: Vec<C>,
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
    pub fn children(&self) -> Vec<&C> {
        use ASAPOp::*;
        match self {
            SummaryAgg { child, filter, .. } => {
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
            SummaryMerge { children } => children.iter().collect(),
        }
    }

    /// `f` may change the reference type, e.g. from `Rc<OperatorNode>` to a
    /// node id.
    pub fn map_children<D>(&self, mut f: impl FnMut(&C) -> D) -> ASAPOp<D> {
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
            SummaryMerge { children } => SummaryMerge {
                children: children.iter().map(&mut f).collect(),
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

    pub fn kind_name(&self) -> &'static str {
        use ASAPOp::*;
        match self {
            SummaryAgg { .. } => "SummaryAgg",
            SummaryEstimate { .. } => "SummaryEstimate",
            FinalizeExactAccumulator { .. } => "FinalizeExactAccumulator",
            MaintainPopulation { .. } => "MaintainPopulation",
            EvaluatePopulation { .. } => "EvaluatePopulation",
            SummaryMerge { .. } => "SummaryMerge",
            SummarySubtract { .. } => "SummarySubtract",
            SummaryDelete { .. } => "SummaryDelete",
            SummaryJoin { .. } => "SummaryJoin",
            Extension { .. } => "Extension",
        }
    }
}

impl ASAPOp {
    /// Reserved variants that are migrated but not implemented.
    pub fn is_unimplemented(&self) -> bool {
        use ASAPOp::*;
        matches!(
            self,
            SummarySubtract { .. } | SummaryDelete { .. } | SummaryJoin { .. } | Extension { .. }
        )
    }

    fn unimplemented() -> SchemaDerivationError {
        SchemaDerivationError::InvalidScalarSignature(UNIMPLEMENTED_ASAP_OP.into())
    }

    /// The summary state this operator produces, if it produces state.
    pub fn produced_state(&self) -> Option<&FieldDataType> {
        match self {
            ASAPOp::SummaryAgg { family, .. } | ASAPOp::SummaryJoin { family, .. } => Some(family),
            ASAPOp::SummaryMerge { children } => children.first().and_then(|child| {
                child
                    .schema
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
    pub fn output_schema(&self) -> Result<Schema, SchemaDerivationError> {
        use ASAPOp::*;
        Ok(match self {
            SummaryAgg {
                child,
                family,
                reduction,
                ..
            } => {
                let mut schema = crate::ir::schema::aggregate_output_schema(
                    &child.schema,
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
                schema
            }
            SummaryEstimate {
                summary_input,
                query,
            } => {
                let input = &summary_input.schema;
                let (name, mut dtype) = match query {
                    SketchStatistic::Quantile { .. } => ("quantile", DataType::Float64),
                    SketchStatistic::Cardinality => ("cardinality", DataType::Int64),
                    SketchStatistic::PointCount { .. } => ("count", DataType::Int64),
                    SketchStatistic::FrequencyL2 => ("frequency_l2", DataType::Float64),
                    SketchStatistic::FrequencyEntropy => ("frequency_entropy", DataType::Float64),
                    SketchStatistic::TopK { .. } => return ranked_rows_schema(summary_input),
                };
                if matches!(
                    summary_input.asap(),
                    Some(SummaryAgg {
                        reduction: Reduction::PerEntity,
                        ..
                    })
                ) && dtype == DataType::Int64
                {
                    dtype = DataType::Float64;
                }
                let mut schema = input.clone();
                for field in &mut schema.fields {
                    if !field.is_plain() {
                        *field = Field::plain(name, dtype.clone(), false);
                    }
                }
                schema
            }
            FinalizeExactAccumulator { child } => {
                // A merge's inputs have identical schemas (tumbling panes),
                // so its first input names the finalized value.
                let mut built = child;
                while let Some(SummaryMerge { children }) = built.asap() {
                    match children.first() {
                        Some(first) => built = first,
                        None => break,
                    }
                }
                let value_result = if let Some(ASAPOp::SummaryAgg {
                    child: source,
                    family: FieldDataType::ExactAggregate(kind, _),
                    input,
                    reduction,
                    ..
                }) = built.asap()
                {
                    {
                        use crate::ir::operator::AggIntent;
                        use crate::ir::schema::{ExactKind, SummaryInputExpr};
                        let column = match &input.weight {
                            SummaryInputExpr::Column(col) => Some(
                                crate::ir::scalar::column_resolution::resolve_column_ref(
                                    col,
                                    &source.schema,
                                )
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
                                accuracy: crate::types::AccuracyTarget::Exact,
                            }),
                            ExactKind::Rate => Some(AggIntent::Rate),
                            ExactKind::IRate => Some(AggIntent::IRate),
                            ExactKind::Increase => Some(AggIntent::Increase),
                        };
                        measure
                            .map(|measure| {
                                crate::ir::NonASAPOp::Aggregate {
                                    child: Rc::clone(source),
                                    reduction: reduction.clone(),
                                    measures: vec![measure],
                                    output_names: vec![],
                                    filters: vec![],
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
                let mut out = child.schema.clone();
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
                out
            }
            MaintainPopulation { child, .. } => child.schema.clone(),
            EvaluatePopulation { child, evaluation } => {
                use crate::ir::operator::maintained_population::PopulationInput;
                use crate::ir::operator::{AggIntent, GroupKeys};
                let Some(MaintainPopulation {
                    child: source,
                    population,
                }) = child.asap()
                else {
                    return Err(SchemaDerivationError::InvalidScalarSignature(
                        "population evaluation requires maintained membership".into(),
                    ));
                };
                if matches!(evaluation, PopulationStatistic::TopK { .. }) {
                    source.schema.clone()
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
                                    source.schema.column_id(name).ok_or_else(|| {
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
                    let accuracy = crate::types::AccuracyTarget::Exact;
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
            SummaryMerge { children } => {
                self.validate_inputs()?;
                children[0].schema.clone()
            }
            SummarySubtract { .. }
            | SummaryDelete { .. }
            | SummaryJoin { .. }
            | Extension { .. } => return Err(Self::unimplemented()),
        })
    }

    pub fn output_kind(&self) -> OperatorResultKind {
        use ASAPOp::*;
        match self {
            SummaryAgg { .. }
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
    pub fn validate_inputs(&self) -> Result<(), SchemaDerivationError> {
        use ASAPOp::*;
        let needs_state = |node: &OperatorNode, what: &str| {
            if node.result_kind != OperatorResultKind::State {
                Err(SchemaDerivationError::InvalidScalarSignature(format!(
                    "{what} requires summary state as input, got {:?}",
                    node.result_kind
                )))
            } else {
                Ok(())
            }
        };
        match self {
            SummaryMerge { children } => {
                let Some(first) = children.first() else {
                    return Err(SchemaDerivationError::InvalidScalarSignature(
                        "summary merge requires at least one state input".into(),
                    ));
                };
                // Matching state parameters and grouping positions are necessary;
                // matching names alone cannot prove two states compatible.
                if first
                    .schema
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
                if let Some(family) = self.produced_state().filter(|f| !f.family_merges()) {
                    return Err(SchemaDerivationError::InvalidScalarSignature(format!(
                        "summary merge over {family:?} is unsupported: the family has no sound merge"
                    )));
                }
                for child in children {
                    needs_state(child, "SummaryMerge")?;
                    if child.schema != first.schema {
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
                use crate::ir::schema::SketchCategory as C;
                let states: Vec<_> = summary_input
                    .schema
                    .fields
                    .iter()
                    .filter(|f| !f.is_plain())
                    .collect();
                let valid = match states.as_slice() {
                    [field] => match &field.dtype {
                        FieldDataType::Sketch(kind, _) => matches!(
                            (kind.category(), query),
                            (C::Quantile, SketchStatistic::Quantile { .. })
                                | (C::Cardinality | C::Universal, SketchStatistic::Cardinality)
                                | (
                                    C::Frequency | C::Universal,
                                    SketchStatistic::PointCount { .. }
                                )
                                | (
                                    C::Universal,
                                    SketchStatistic::FrequencyL2
                                        | SketchStatistic::FrequencyEntropy
                                )
                                | (C::TopK | C::Universal, SketchStatistic::TopK { .. })
                        ),
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
                    .schema
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
                if !matches!(child.asap(), Some(MaintainPopulation { population, .. }) if population.supports(evaluation))
                {
                    return Err(SchemaDerivationError::InvalidScalarSignature(
                        "population evaluation requires compatible maintained membership".into(),
                    ));
                }
                Ok(())
            }
            SummaryAgg {
                child,
                family,
                input,
                filter,
                ..
            } => {
                if family.is_plain() || child.result_kind == OperatorResultKind::State {
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
                check(&input.weight, &child.schema)?;
                if let Some(item) = &input.item {
                    check(item, &child.schema)?;
                }
                if let Some(filter) = filter {
                    if filter.0.scalar_type(&child.schema)?.0 != DataType::Bool {
                        return Err(SchemaDerivationError::InvalidScalarSignature(
                            "summary filter must be boolean".into(),
                        ));
                    }
                }
                Ok(())
            }
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
}

/// A top-k readout returns the selected rows, as every executable top-k does
/// (exact Sort → Limit, `EvaluatePopulation`): the state's partition keys, the
/// ranked item's identity columns, and the item's estimated `value`.
fn ranked_rows_schema(state: &OperatorNode) -> Result<Schema, SchemaDerivationError> {
    use crate::ir::schema::state_type::{EntityIdentity, SummaryInputExpr};
    fn source(state: &OperatorNode) -> Option<(&Schema, &SummaryUpdate)> {
        match state.asap()? {
            ASAPOp::SummaryAgg { child, input, .. } => Some((&child.schema, input)),
            ASAPOp::SummaryMerge { children } => source(children.first()?),
            _ => None,
        }
    }
    fn items(
        item: &SummaryInputExpr,
        source: &Schema,
        fields: &mut Vec<Field>,
    ) -> Result<(), SchemaDerivationError> {
        match item {
            SummaryInputExpr::Column(column) => {
                let index = crate::ir::scalar::resolve_column_ref(column, source)
                    .map_err(|e| SchemaDerivationError::InvalidScalarSignature(e.to_string()))?;
                fields.push(source.fields[index].clone());
            }
            SummaryInputExpr::Tuple(parts) => {
                for part in parts {
                    items(part, source, fields)?;
                }
            }
            // A label set without its columns is read back as its encoded identity.
            SummaryInputExpr::EntityIdentity(EntityIdentity::PromqlLabelSet { .. }) => {
                fields.push(Field::plain(
                    crate::ir::schema::PROMQL_SERIES_IDENTITY,
                    DataType::Utf8,
                    false,
                ))
            }
            SummaryInputExpr::Constant(_) => {
                return Err(SchemaDerivationError::InvalidScalarSignature(
                    "top-k item identity cannot be a constant".into(),
                ))
            }
        }
        Ok(())
    }
    let Some((
        source,
        SummaryUpdate {
            item: Some(item), ..
        },
    )) = source(state)
    else {
        return Err(SchemaDerivationError::InvalidScalarSignature(
            "top-k readout requires state keyed by an item identity".into(),
        ));
    };
    let mut fields: Vec<_> = state
        .schema
        .fields
        .iter()
        .filter(|f| f.is_plain())
        .cloned()
        .collect();
    items(item, source, &mut fields)?;
    let key = (0..fields.len()).collect();
    fields.push(Field::plain("value", DataType::Float64, false));
    Ok(Schema {
        fields,
        time_index: None,
        unique_keys: vec![key],
        closed: true,
    })
}

/// The plain value an exact accumulator finalizes to.
fn finalized_data_type(kind: &crate::ir::schema::state_type::ExactKind) -> DataType {
    use crate::ir::schema::ExactKind;
    match kind {
        ExactKind::Count => DataType::Int64,
        _ => DataType::Float64,
    }
}

/// The category of the values a evaluation of `state` produces: the category
/// of the relational input the state was built from.
fn source_kind(node: &OperatorNode) -> OperatorResultKind {
    match &node.operator {
        crate::ir::operator::node::Operator::ASAP(op) => match op.children().first() {
            Some(child) => source_kind(child),
            None => OperatorResultKind::Relation,
        },
        crate::ir::operator::node::Operator::NonASAP(_) => match node.result_kind {
            OperatorResultKind::RangeVector => OperatorResultKind::InstantVector,
            OperatorResultKind::State => OperatorResultKind::Relation,
            other => other,
        },
    }
}
