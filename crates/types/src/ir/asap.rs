//! ASAP operators: summary-state construction, state operations and evaluations.
//! The summary family, kind/algorithm and parameters are committed here.

use std::rc::Rc;

use serde::{Deserialize, Serialize};

use super::node::{OperatorNode, OperatorResultKind};
use crate::ir::operator_properties::Reduction;
use crate::ir::SchemaDerivationError;
use crate::post_asap::maintained_population::{MaintainedPopulation, PopulationStatistic};
use crate::post_asap::sketch::{GroupingStrategy, SketchStatistic, SummaryUpdate};
use crate::pre_asap::schema::{ColumnId, DataType, Field, FieldDataType, Schema};

/// Why an ASAP operator cannot be used yet.
pub const UNIMPLEMENTED_ASAP_OP: &str =
    "this ASAP operator is reserved: schema, accuracy, timing and export are not implemented";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ASAPOp {
    /// Summary aggregation. Output: grouping columns + one field carrying
    /// partial summary state per group, typed `family`.
    SummaryAgg {
        child: Rc<OperatorNode>,
        /// Which summary family realizes this aggregation. Never
        /// `FieldDataType::Plain`.
        family: FieldDataType,
        input: SummaryUpdate,
        reduction: Reduction,
        grouping: GroupingStrategy,
        #[serde(default)]
        filter: Option<super::scalar::Predicate>,
    },
    /// Read out a query result from built summary state. Output is a
    /// row-shaped schema.
    SummaryEstimate {
        summary_input: Rc<OperatorNode>,
        query: SketchStatistic,
    },
    /// Read an exact accumulator's state as its finalized value: the
    /// maintenance-to-read boundary before query-time operators.
    FinalizeExactAccumulator { child: Rc<OperatorNode> },
    /// Maintain the full declared population, including membership changes.
    MaintainPopulation {
        child: Rc<OperatorNode>,
        population: MaintainedPopulation,
    },
    /// Read an aggregate or TopK prefix from the maintained population.
    EvaluatePopulation {
        child: Rc<OperatorNode>,
        evaluation: PopulationStatistic,
    },
    /// Merge compatible partial states for the same grouping and family.
    SummaryMerge { children: Vec<Rc<OperatorNode>> },
    // ── Reserved: migrated but unimplemented ──
    SummarySubtract {
        left: Rc<OperatorNode>,
        right: Rc<OperatorNode>,
    },
    SummaryDelete {
        summary_input: Rc<OperatorNode>,
        key: ColumnId,
    },
    SummaryJoin {
        outer: Rc<OperatorNode>,
        inner: Rc<OperatorNode>,
        key: ColumnId,
        family: FieldDataType,
    },
    Extension {
        child: Rc<OperatorNode>,
        name: String,
    },
}

impl ASAPOp {
    pub fn children(&self) -> Vec<&Rc<OperatorNode>> {
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

    pub fn map_children(&self, mut f: impl FnMut(&Rc<OperatorNode>) -> Rc<OperatorNode>) -> Self {
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
                    .map(|p| super::scalar::Predicate(p.0.map_operator_refs(&mut f))),
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
                let mut schema = crate::pre_asap::aggregate_output_schema(
                    &child.schema,
                    reduction,
                    &[crate::pre_asap::AggIntent::Sum { col: None }],
                    &[],
                )?;
                let index = match reduction {
                    Reduction::PerEntity => crate::pre_asap::resolve_column_ref(
                        &crate::pre_asap::ColumnRef::SampleValue,
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
                    SketchStatistic::TopK { .. } => ("topk", DataType::Utf8),
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
                let value_result = if let Some(ASAPOp::SummaryAgg {
                    child: source,
                    family: FieldDataType::ExactAggregate(kind, _),
                    input,
                    reduction,
                    ..
                }) = child.asap()
                {
                    {
                        use crate::post_asap::{ExactKind, SummaryInputExpr};
                        use crate::pre_asap::AggIntent;
                        let column = match &input.weight {
                            SummaryInputExpr::Column(col) => Some(
                                crate::pre_asap::column_resolution::resolve_column_ref(
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
                                super::NonASAPOp::Aggregate {
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
                use crate::post_asap::maintained_population::PopulationInput;
                use crate::pre_asap::{AggIntent, GroupKeys};
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
                    super::NonASAPOp::Aggregate {
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
                for child in children {
                    needs_state(child, "SummaryMerge")?;
                    if child.schema != first.schema {
                        return Err(SchemaDerivationError::InvalidScalarSignature(
                            "summary merge inputs must have identical state and grouping schemas"
                                .into(),
                        ));
                    }
                }
                Ok(())
            }
            SummaryEstimate {
                summary_input,
                query,
            } => {
                needs_state(summary_input, "SummaryEstimate")?;
                use crate::post_asap::sketch::SketchCategory as C;
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
                    expr: &crate::post_asap::SummaryInputExpr,
                    schema: &Schema,
                ) -> Result<(), SchemaDerivationError> {
                    use crate::post_asap::SummaryInputExpr;
                    match expr {
                        SummaryInputExpr::Column(col) => {
                            crate::pre_asap::resolve_column_ref(col, schema).map_err(|e| {
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

/// The plain value an exact accumulator finalizes to.
fn finalized_data_type(kind: &crate::post_asap::sketch::ExactKind) -> DataType {
    use crate::post_asap::sketch::ExactKind;
    match kind {
        ExactKind::Count => DataType::Int64,
        _ => DataType::Float64,
    }
}

/// The category of the values a evaluation of `state` produces: the category
/// of the relational input the state was built from.
fn source_kind(node: &OperatorNode) -> OperatorResultKind {
    match &node.operator {
        super::node::Operator::ASAP(op) => match op.children().first() {
            Some(child) => source_kind(child),
            None => OperatorResultKind::Relation,
        },
        super::node::Operator::NonASAP(_) => match node.result_kind {
            OperatorResultKind::RangeVector => OperatorResultKind::InstantVector,
            OperatorResultKind::State => OperatorResultKind::Relation,
            other => other,
        },
    }
}
