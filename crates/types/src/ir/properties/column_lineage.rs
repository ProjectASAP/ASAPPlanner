//! Column lineage: where each output column of a memo group comes from.
//! Design: concepts doc `memo-groups-properties-requirements.md`, section 3,
//! "Column lineage".
//!
//! A group's lineage is derived once from its input groups' lineage, one
//! entry per output column in schema order. A column copied unchanged from a
//! source column (a `Scan`'s table or metric column), however renamed on the
//! way, has that source column's lineage; any other column is `Computed`.
//! It identifies columns where a schema field's name cannot: two columns of
//! one name from two join sides have different lineage, and a renamed column
//! keeps its lineage.

use crate::ir::operator::asap::ASAPOp;
use crate::ir::operator::input::OperatorInput;
use crate::ir::operator::node::Operator;
use crate::ir::operator::non_asap::NonASAPOp;
use crate::ir::operator::operator_properties::{
    BinaryOpKind, GroupSide, JoinKind, PromQLVectorSetOpKind, Reduction, Source,
};
use crate::ir::operator::AggIntent;
use crate::ir::scalar::{resolve_column_ref, ColumnRef, ProjectItem, ScalarExpr};
use crate::ir::schema::Schema;
use serde::{Deserialize, Serialize};

/// A column of a source (a `Scan`'s table or metric), by name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceColumn {
    pub(crate) source: String,
    pub(crate) column: String,
}

#[cfg(any(test, feature = "test-support"))]
impl SourceColumn {
    pub fn new(source: impl Into<String>, column: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            column: column.into(),
        }
    }
}

/// Where an output column's values come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnLineage {
    /// Copied unchanged from a column of a source.
    SourceColumn(SourceColumn),
    /// Computed by an operator: a measure, an expression, a summary state or readout.
    Computed,
}

impl<I: OperatorInput> Operator<I> {
    /// The lineage of each column of `output`, this operator's output
    /// schema, from its inputs' lineage (`lineage_of`).
    pub(crate) fn output_column_lineage(
        &self,
        output: &Schema,
        lineage_of: &dyn Fn(&I) -> Vec<ColumnLineage>,
    ) -> Vec<ColumnLineage> {
        let lineage = match self {
            Operator::NonASAP(op) => non_asap_lineage(op, output, lineage_of),
            Operator::ASAP(op) => asap_lineage(op, output, lineage_of),
        };
        debug_assert_eq!(
            lineage.len(),
            output.fields.len(),
            "one lineage entry per output column of {}",
            self.kind_name()
        );
        lineage
    }
}

fn non_asap_lineage<I: OperatorInput>(
    op: &NonASAPOp<I>,
    output: &Schema,
    lineage_of: &dyn Fn(&I) -> Vec<ColumnLineage>,
) -> Vec<ColumnLineage> {
    use NonASAPOp::*;
    let computed = || vec![ColumnLineage::Computed; output.fields.len()];
    match op {
        Scan { source, schema, .. } => {
            let source = match source {
                Source::TimeSeries { metric } => metric,
                Source::Table { table_ref } => table_ref,
            };
            schema
                .fields
                .iter()
                .map(|field| {
                    ColumnLineage::SourceColumn(SourceColumn {
                        source: source.clone(),
                        column: field.name.clone(),
                    })
                })
                .collect()
        }
        Values { .. } | PromqlVectorFromScalar(_) => computed(),
        Filter { child, .. }
        | Sort { child, .. }
        | Limit { child, .. }
        | Dedup { child, .. }
        | TimeRange { child, .. }
        | TimeShift { child, .. }
        | PromqlSubquery { child, .. }
        | PromqlSeriesSample { child, .. }
        | PromqlInfoEnrich { child, .. } => lineage_of(child),
        // The relabeled column is written; every other column is copied.
        PromqlRelabel { dst, child, .. } => {
            let mut lineage = lineage_of(child);
            match child.schema().column_id(dst) {
                Some(written) => lineage[written] = ColumnLineage::Computed,
                None => lineage.push(ColumnLineage::Computed),
            }
            lineage
        }
        Project { cols, child } => project_lineage(cols, &lineage_of(child)),
        Aggregate {
            reduction,
            measures,
            output_names,
            child,
            ..
        } => {
            if let [AggIntent::TopK { k, .. }] = measures.as_slice() {
                let name = crate::ir::schema::topk::topk_output_name(*k, output_names);
                return match crate::ir::schema::topk::ranked_row_projection(child, reduction, &name)
                {
                    Ok(cols) => project_lineage(&cols, &lineage_of(child)),
                    Err(_) => computed(),
                };
            }
            aggregate_lineage(child, reduction, measures, output, lineage_of)
        }
        Join {
            kind, left, right, ..
        } => {
            let mut lineage = lineage_of(left);
            if !matches!(kind, JoinKind::Semi | JoinKind::Anti) {
                lineage.extend(lineage_of(right));
            }
            lineage
        }
        SetOp { left, right, .. } => agreed(&[lineage_of(left), lineage_of(right)]),
        Concat { children } => agreed(&children.iter().map(lineage_of).collect::<Vec<_>>()),
        SQLWindowFunc { child, .. } => {
            let mut lineage = lineage_of(child);
            lineage.push(ColumnLineage::Computed);
            lineage
        }
        // The output is the left side's columns, its value computed, then the
        // right side's labels the match adds. A label that may come from
        // either side keeps its lineage only if both sides agree.
        BinaryOp {
            operator, lhs, rhs, ..
        } => {
            let mut lineage = lineage_of(lhs);
            if let Ok(value) = resolve_column_ref(&ColumnRef::SampleValue, lhs.schema()) {
                lineage[value] = ColumnLineage::Computed;
            }
            let right = lineage_of(rhs);
            let right_named = |name: &str| {
                rhs.schema()
                    .column_id(name)
                    .map_or(ColumnLineage::Computed, |column| right[column].clone())
            };
            let right_rows = matches!(operator.kind, BinaryOpKind::Set(PromQLVectorSetOpKind::Or))
                || operator
                    .vector_match
                    .as_ref()
                    .and_then(|m| m.grouping.as_ref())
                    .is_some_and(|grouping| grouping.side == GroupSide::Right);
            if right_rows {
                for (column, field) in lhs.schema().fields.iter().enumerate() {
                    if right_named(&field.name) != lineage[column] {
                        lineage[column] = ColumnLineage::Computed;
                    }
                }
            }
            let added = output.fields.get(lineage.len()..).unwrap_or_default();
            lineage.extend(added.iter().map(|field| right_named(&field.name)));
            lineage
        }
    }
}

fn asap_lineage<I: OperatorInput>(
    op: &ASAPOp<I>,
    output: &Schema,
    lineage_of: &dyn Fn(&I) -> Vec<ColumnLineage>,
) -> Vec<ColumnLineage> {
    use ASAPOp::*;
    match op {
        // Laid out as an aggregate of one measure, the state in its place.
        SummaryAgg {
            child, reduction, ..
        }
        | SummaryTumblingWindowChain {
            child, reduction, ..
        } => aggregate_lineage(
            child,
            reduction,
            &[AggIntent::Sum { col: None }],
            output,
            lineage_of,
        ),
        SummaryEstimate {
            summary_input: child,
            ..
        }
        | FinalizeExactAccumulator { child } => readout_lineage(child, output, lineage_of),
        MaintainPopulation { child, .. } => lineage_of(child),
        EvaluatePopulation { child, .. } if output == child.schema() => lineage_of(child),
        SummaryMerge {
            children, group_by, ..
        } => {
            let merged = agreed(&children.iter().map(lineage_of).collect::<Vec<_>>());
            match group_by {
                None => merged,
                Some(keys) => keys
                    .iter()
                    .map(|&key| merged.get(key).cloned().unwrap_or(ColumnLineage::Computed))
                    .chain([ColumnLineage::Computed])
                    .collect(),
            }
        }
        EvaluatePopulation { .. }
        | SummarySubtract { .. }
        | SummaryDelete { .. }
        | SummaryJoin { .. }
        | Extension { .. } => vec![ColumnLineage::Computed; output.fields.len()],
    }
}

/// A column item, renamed or not, keeps its input column's lineage; an
/// expression item is computed.
fn project_lineage<C>(cols: &[ProjectItem<C>], input: &[ColumnLineage]) -> Vec<ColumnLineage> {
    cols.iter()
        .map(|item| match item.expr {
            ScalarExpr::Column(column) => input
                .get(column)
                .cloned()
                .unwrap_or(ColumnLineage::Computed),
            _ => ColumnLineage::Computed,
        })
        .collect()
}

/// An aggregate's (`aggregate_output_schema`'s layout): a per-entity
/// reduction keeps its input's columns in place, its value computed; a
/// `by` reduction lists its keys, then its measures; a `without` reduction
/// the input's other label columns, then its measures.
fn aggregate_lineage<I: OperatorInput>(
    child: &I,
    reduction: &Reduction,
    measures: &[AggIntent],
    output: &Schema,
    lineage_of: &dyn Fn(&I) -> Vec<ColumnLineage>,
) -> Vec<ColumnLineage> {
    let input = lineage_of(child);
    let schema = child.schema();
    let sample_value = resolve_column_ref(&ColumnRef::SampleValue, schema).ok();
    let keys: Vec<ColumnLineage> = match reduction {
        Reduction::PerEntity => {
            let mut lineage = input;
            let value = measures
                .first()
                .and_then(|measure| measure.input_cols().first().copied())
                .or(sample_value);
            if let Some(value) = value.and_then(|value| lineage.get_mut(value)) {
                *value = ColumnLineage::Computed;
            }
            return lineage;
        }
        Reduction::Reduce(by) if by.is_without() => (0..schema.fields.len())
            .filter(|&column| {
                schema.time_index != Some(column)
                    && sample_value != Some(column)
                    && !by.keys().contains(&column)
            })
            .map(|column| input[column].clone())
            .collect(),
        Reduction::Reduce(by) => by
            .keys()
            .iter()
            .map(|&key| input.get(key).cloned().unwrap_or(ColumnLineage::Computed))
            .collect(),
    };
    let measured = output.fields.len().saturating_sub(keys.len());
    keys.into_iter()
        .chain(std::iter::repeat_n(ColumnLineage::Computed, measured))
        .collect()
}

/// A readout of summary state: the state's plain (key) columns keep their
/// lineage in place; the value read out is computed.
fn readout_lineage<I: OperatorInput>(
    state: &I,
    output: &Schema,
    lineage_of: &dyn Fn(&I) -> Vec<ColumnLineage>,
) -> Vec<ColumnLineage> {
    let input = state.schema();
    if input.fields.len() != output.fields.len() {
        return vec![ColumnLineage::Computed; output.fields.len()];
    }
    lineage_of(state)
        .into_iter()
        .zip(input.fields.iter().zip(&output.fields))
        .map(|(lineage, (at_state, at_output))| {
            if at_state.is_plain() && at_output.is_plain() {
                lineage
            } else {
                ColumnLineage::Computed
            }
        })
        .collect()
}

/// Per column, the inputs' lineage if they all agree, else `Computed`.
fn agreed(inputs: &[Vec<ColumnLineage>]) -> Vec<ColumnLineage> {
    let Some((first, rest)) = inputs.split_first() else {
        return Vec::new();
    };
    first
        .iter()
        .enumerate()
        .map(|(column, lineage)| {
            if rest.iter().all(|other| other.get(column) == Some(lineage)) {
                lineage.clone()
            } else {
                ColumnLineage::Computed
            }
        })
        .collect()
}
