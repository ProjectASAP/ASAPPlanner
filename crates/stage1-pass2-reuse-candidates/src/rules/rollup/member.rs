//! One aggregate group as the rollup rule reads it ([`Member`]): its keys,
//! selection and definition, and whether two are of one cluster.

use std::collections::BTreeMap;

use asap_types::ir::operator::arguments::SourceColumn;
use asap_types::ir::operator::Reduction;
use asap_types::ir::properties::selection::{constraint_of, Constraint};
use asap_types::ir::schema::{DataType, FieldDataType, Schema};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::stages::{AggregateDefinition, ColumnLineage, Memo, MemoGroupId};
use asap_types::summary_type::SummaryTypeCapability;

use super::rewrite::rolled_up;

/// One aggregate group as a rollup rule reads it.
pub(super) struct Member {
    pub(super) group: MemoGroupId,
    /// The group's expression.
    pub(super) expression: Operator<MemoGroupId>,
    /// The rows it aggregates: the `SummaryAgg`'s or `Aggregate`'s input.
    pub(super) rows: MemoGroupId,
    /// Its group-by keys, in output order, by the source column they copy.
    pub(super) keys: Vec<SourceColumn>,
    /// Its coverage's selection, one box.
    pub(super) selection: BTreeMap<SourceColumn, Constraint>,
    /// Its coverage's definition without its keys: the top operator's
    /// reduction (and an `Aggregate`'s output names) blanked, and the input
    /// written out down to the source ([`Member::see_through_per_series_layers`]),
    /// so equal computations over one source compare equal whatever their
    /// keys, label selections and the labels their scans list.
    pub(super) definition: AggregateDefinition,
}

impl Member {
    pub(super) fn of(memo: &Memo, group: MemoGroupId) -> Option<Self> {
        let memo_group = &memo.groups()[group.0 as usize];
        let (rows, by) = match &memo_group.expression {
            Operator::ASAP(ASAPOp::SummaryAgg {
                child,
                summary_type,
                reduction: Reduction::Reduce(by),
                ..
            }) if SummaryTypeCapability::merges_groups(summary_type) => (*child, by),
            Operator::NonASAP(NonASAPOp::Aggregate {
                child,
                reduction: Reduction::Reduce(by),
                measures,
                filters,
                having: None,
                ..
            }) if filters.iter().all(Option::is_none)
                && measures
                    .iter()
                    .all(|measure| rolled_up(measure, 0).is_some()) =>
            {
                (*child, by)
            }
            _ => return None,
        };
        if by.is_without() {
            return None;
        }
        let coverage = memo_group.group_properties.aggregate_coverage.as_ref()?;
        let [selection] = coverage.selection() else {
            return None;
        };
        let rows_lineage = &memo.groups()[rows.0 as usize]
            .group_properties
            .column_lineage;
        let keys = by
            .keys()
            .iter()
            .map(|&key| match rows_lineage.get(key)? {
                ColumnLineage::SourceColumn(column) => Some(column.clone()),
                ColumnLineage::Computed { .. } => None,
            })
            .collect::<Option<Vec<_>>>()?;
        let mut definition = coverage.definition().clone();
        match definition.unlifted_operators.first_mut()? {
            Operator::ASAP(ASAPOp::SummaryAgg { reduction, .. }) => {
                *reduction = Reduction::PerEntity
            }
            Operator::NonASAP(NonASAPOp::Aggregate {
                reduction,
                output_names,
                ..
            }) => {
                *reduction = Reduction::PerEntity;
                output_names.clear();
            }
            _ => return None,
        }
        let mut selection = selection.columns().clone();
        Self::see_through_per_series_layers(memo, &mut definition, &mut selection);
        Some(Self {
            group,
            expression: memo_group.expression.clone(),
            rows,
            keys,
            selection,
            definition,
        })
    }

    /// Writes the input of `definition` out down to its source. Each
    /// per-series layer below it ([`per_series_layer`]: a `TimeRange`, a
    /// `TimeShift`, a per-series `Aggregate`) becomes an unlifted operator,
    /// so two members compare equal only over the same range and the same
    /// per-series function. A time-series `Scan` under them is the last
    /// unlifted operator: each label matcher on it moves into `selection`
    /// (by source column), since a label is constant within a series and a
    /// per-series layer keeps or drops whole series alike; the rest stay
    /// its predicates. Its schema is dropped when nothing left names a
    /// column by position ([`names_columns_by_position`]): the labels a
    /// scan lists are those its query reads, its rows (one per sample of
    /// each series, the series identity holding every label) are the same
    /// whatever it lists, and members name their keys and selections by
    /// source column.
    fn see_through_per_series_layers(
        memo: &Memo,
        definition: &mut AggregateDefinition,
        selection: &mut BTreeMap<SourceColumn, Constraint>,
    ) {
        while let Some(input) = definition.input {
            let expression = &memo.groups()[input.0 as usize].expression;
            if !per_series_layer(expression) {
                break;
            }
            definition
                .unlifted_operators
                .push(expression.map_children(|_| None));
            definition.input = Some(*expression.children()[0]);
        }
        // The bottom `Scan`: the input group, or the last unlifted operator
        // when the coverage lifted one of its predicates already.
        let (scan, lineage) = match definition.input {
            Some(input) => {
                let scan_group = &memo.groups()[input.0 as usize];
                (
                    scan_group.expression.map_children(|&child| Some(child)),
                    Some(&scan_group.group_properties.column_lineage),
                )
            }
            None => match definition.unlifted_operators.last() {
                Some(last) => (last.clone(), None),
                None => return,
            },
        };
        let Operator::NonASAP(NonASAPOp::Scan {
            source,
            predicates,
            schema,
        }) = scan
        else {
            return;
        };
        let mut kept = Vec::new();
        for predicate in predicates {
            let label_matcher = constraint_of(&predicate.0).and_then(|(column, constraint)| {
                let is_label = source.is_time_series()
                    && schema.fields.get(column)?.dtype == FieldDataType::Plain(DataType::Utf8);
                match lineage?.get(column)? {
                    ColumnLineage::SourceColumn(source_column)
                        if is_label && !selection.contains_key(source_column) =>
                    {
                        Some((source_column.clone(), constraint))
                    }
                    _ => None,
                }
            });
            match label_matcher {
                Some((column, constraint)) => {
                    selection.insert(column, constraint);
                }
                None => kept.push(predicate),
            }
        }
        if definition.input.take().is_none() {
            definition.unlifted_operators.pop();
        }
        let schema = if source.is_time_series()
            && kept.is_empty()
            && !definition
                .unlifted_operators
                .iter()
                .any(names_columns_by_position)
        {
            Schema::default()
        } else {
            schema
        };
        definition
            .unlifted_operators
            .push(Operator::NonASAP(NonASAPOp::Scan {
                source,
                predicates: kept,
                schema,
            }));
    }

    /// Whether `other` computes the same thing over the same time window:
    /// equal definitions without keys and equal interval constraints.
    pub(super) fn same_cluster(&self, other: &Self) -> bool {
        self.definition == other.definition && self.intervals() == other.intervals()
    }

    /// Its interval constraints (the time window, value ranges): equal
    /// across a cluster, never compensated.
    fn intervals(&self) -> BTreeMap<&SourceColumn, &Constraint> {
        self.selection
            .iter()
            .filter(|(_, constraint)| matches!(constraint, Constraint::Interval { .. }))
            .collect()
    }
}

/// Whether `expression` is a per-series layer a rollup sees through: a
/// `TimeRange` or `TimeShift`, or an `Aggregate` computing one value per
/// series (`Reduction::PerEntity`, such as `rate`) without row filters.
fn per_series_layer(expression: &Operator<MemoGroupId>) -> bool {
    match expression {
        Operator::NonASAP(NonASAPOp::TimeRange { .. } | NonASAPOp::TimeShift { .. }) => true,
        Operator::NonASAP(NonASAPOp::Aggregate {
            reduction: Reduction::PerEntity,
            filters,
            having: None,
            ..
        }) => filters.iter().all(Option::is_none),
        _ => false,
    }
}

/// Whether unlifted operator `operator` names a column of its input by
/// position, so it means the same over two scans only when they list the
/// same columns: a measure over a column (`SUM(col)`; a PromQL measure
/// reads the implicit sample value and names none), a row filter, or any
/// operator other than a `SummaryAgg`, an `Aggregate`, a `TimeRange` and a
/// `TimeShift`. The top operator's keys are blanked, and a `SummaryAgg`'s
/// update names its columns by name.
fn names_columns_by_position(operator: &Operator<Option<MemoGroupId>>) -> bool {
    match operator {
        Operator::ASAP(ASAPOp::SummaryAgg { filter, .. }) => filter.is_some(),
        Operator::NonASAP(NonASAPOp::Aggregate {
            measures,
            filters,
            having,
            ..
        }) => {
            having.is_some()
                || filters.iter().any(Option::is_some)
                || measures
                    .iter()
                    .any(|measure| !measure.input_cols().is_empty())
        }
        Operator::NonASAP(NonASAPOp::TimeRange { .. } | NonASAPOp::TimeShift { .. }) => false,
        _ => true,
    }
}

/// The column of `lineage` that copies `column`, when exactly one does.
pub(super) fn column_of(lineage: &[ColumnLineage], column: &SourceColumn) -> Option<usize> {
    let mut at = lineage
        .iter()
        .enumerate()
        .filter(|(_, known)| matches!(known, ColumnLineage::SourceColumn(c) if c == column))
        .map(|(index, _)| index);
    let first = at.next()?;
    at.next().is_none().then_some(first)
}
