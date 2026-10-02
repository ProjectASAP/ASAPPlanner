//! ASAP operators: summary-state construction, state operations and readouts.
//! The summary family, kind/algorithm and parameters are committed here.

use std::rc::Rc;

use serde::{Deserialize, Serialize};

use super::node::{OperatorNode, OperatorResultKind};
use crate::post_asap::maintained_population::{MaintainedPopulation, PopulationReadout};
use crate::post_asap::sketch::{GroupingStrategy, SketchQuery, SummaryUpdate};
use crate::pre_asap::schema::{ColumnId, DataType, Field, FieldDataType, Schema};
use crate::pre_asap::vocabulary::{QueryExprError, Reduction};

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
        query: SketchQuery,
    },
    /// Read an exact accumulator's state as its finalized value: the
    /// maintenance-to-read boundary before query-time operators.
    FinalizeExactAccumulator {
        child: Rc<OperatorNode>,
    },
    /// Maintain the full declared population, including membership changes.
    MaintainPopulation {
        child: Rc<OperatorNode>,
        population: MaintainedPopulation,
    },
    /// Read an aggregate or TopK prefix from the maintained population.
    ReadPopulation {
        child: Rc<OperatorNode>,
        readout: PopulationReadout,
    },
    // ── Reserved: migrated but unimplemented (§1.3 of the proposal) ──
    SummaryMerge {
        children: Vec<Rc<OperatorNode>>,
    },
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
            | ReadPopulation { child, .. }
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
            ReadPopulation { child, readout } => ReadPopulation {
                child: f(child),
                readout: readout.clone(),
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
            ReadPopulation { .. } => "ReadPopulation",
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
            SummaryMerge { .. }
                | SummarySubtract { .. }
                | SummaryDelete { .. }
                | SummaryJoin { .. }
                | Extension { .. }
        )
    }

    fn unimplemented() -> QueryExprError {
        QueryExprError::InvalidScalarSignature(UNIMPLEMENTED_ASAP_OP.into())
    }

    /// The summary state this operator produces, if it produces state.
    pub fn produced_state(&self) -> Option<&FieldDataType> {
        match self {
            ASAPOp::SummaryAgg { family, .. } | ASAPOp::SummaryJoin { family, .. } => Some(family),
            _ => None,
        }
    }

    /// Output schema derived from the operator and its children. Summary
    /// planning may retain a more specific schema (readout column naming)
    /// through [`OperatorNode::with_schema`]; the derived shape agrees with it
    /// in field types.
    pub fn output_schema(&self) -> Result<Schema, QueryExprError> {
        use ASAPOp::*;
        Ok(match self {
            SummaryAgg {
                child,
                family,
                reduction,
                ..
            } => {
                let input = &child.schema;
                let mut fields = Vec::new();
                if let Reduction::Reduce(by) = reduction {
                    if !by.is_without() {
                        for &id in by.keys() {
                            let f = input.fields.get(id).ok_or(
                                QueryExprError::InvalidGroupByColumn(id, input.fields.len()),
                            )?;
                            fields.push(f.clone());
                        }
                    }
                }
                fields.push(Field::new("state", family.clone(), false));
                Schema::lifted(fields, None)
            }
            SummaryEstimate {
                summary_input,
                query,
            } => {
                let input = &summary_input.schema;
                let mut fields: Vec<Field> = input
                    .fields
                    .iter()
                    .filter(|f| f.is_plain())
                    .cloned()
                    .collect();
                let (name, dtype) = match query {
                    SketchQuery::Quantile { .. } => ("quantile", DataType::Float64),
                    SketchQuery::Cardinality => ("cardinality", DataType::Int64),
                    SketchQuery::PointCount { .. } => ("count", DataType::Int64),
                    SketchQuery::FrequencyL2 => ("frequency_l2", DataType::Float64),
                    SketchQuery::FrequencyEntropy => ("frequency_entropy", DataType::Float64),
                    SketchQuery::TopK { .. } => ("topk", DataType::Utf8),
                };
                fields.push(Field::plain(name, dtype, false));
                Schema::lifted(fields, input.time_index)
            }
            FinalizeExactAccumulator { child } => {
                let mut out = Schema::lifted(child.schema.fields.clone(), child.schema.time_index);
                for f in &mut out.fields {
                    if let FieldDataType::ExactAggregate(kind, _) = &f.dtype {
                        f.dtype = FieldDataType::Plain(finalized_data_type(kind));
                    }
                }
                out
            }
            MaintainPopulation { child, .. } => {
                Schema::lifted(child.schema.fields.clone(), child.schema.time_index)
            }
            ReadPopulation { child, readout } => {
                let (name, dtype) = match readout {
                    PopulationReadout::Quantile { .. } => ("quantile", DataType::Float64),
                    PopulationReadout::TopK { .. } => ("topk", DataType::Utf8),
                    PopulationReadout::Sum => ("sum", DataType::Float64),
                    PopulationReadout::Count => ("count", DataType::Int64),
                    PopulationReadout::Average => ("avg", DataType::Float64),
                };
                let mut fields: Vec<Field> = child
                    .schema
                    .fields
                    .iter()
                    .filter(|f| f.is_plain() && f.name != "value")
                    .cloned()
                    .collect();
                fields.push(Field::plain(name, dtype, false));
                Schema::lifted(fields, None)
            }
            SummaryMerge { .. }
            | SummarySubtract { .. }
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
            FinalizeExactAccumulator { child } | ReadPopulation { child, .. } => source_kind(child),
        }
    }

    /// Local input-contract checks.
    pub fn validate_inputs(&self) -> Result<(), QueryExprError> {
        use ASAPOp::*;
        let needs_state = |node: &OperatorNode, what: &str| {
            if node.result_kind != OperatorResultKind::State {
                Err(QueryExprError::InvalidScalarSignature(format!(
                    "{what} requires summary state as input, got {:?}",
                    node.result_kind
                )))
            } else {
                Ok(())
            }
        };
        match self {
            SummaryEstimate { summary_input, .. } => needs_state(summary_input, "SummaryEstimate"),
            FinalizeExactAccumulator { child } => {
                needs_state(child, "FinalizeExactAccumulator")?;
                if child
                    .schema
                    .fields
                    .iter()
                    .all(|f| !matches!(f.dtype, FieldDataType::ExactAggregate(..)))
                {
                    return Err(QueryExprError::InvalidScalarSignature(
                        "FinalizeExactAccumulator requires exact accumulator state".into(),
                    ));
                }
                Ok(())
            }
            ReadPopulation { child, .. } => needs_state(child, "ReadPopulation"),
            SummaryAgg { child, filter, .. } => {
                if let Some(filter) = filter {
                    if filter.0.scalar_type(&child.schema)?.0 != DataType::Bool {
                        return Err(QueryExprError::InvalidScalarSignature(
                            "summary filter must be boolean".into(),
                        ));
                    }
                }
                Ok(())
            }
            MaintainPopulation { .. } => Ok(()),
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

/// The category of the values a readout of `state` produces: the category
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
