//! Memo expressions over group ids that the memo's unit tests and this
//! crate's memo integration tests (`memo_group_properties`,
//! `memo_aggregate_coverage`) build their examples from.
use std::time::Duration;

use crate::ir::operator::arguments::{AtModifier, Reduction, Source, SourceColumn, TimeShift};
use crate::ir::operator::AggIntent;
use crate::ir::scalar::ColumnRef;
use crate::ir::schema::{DataType, Field, FieldDataType, Schema};
use crate::ir::{ASAPOp, NonASAPOp, Operator, Predicate, ScalarExpr, TimeRangeKind};
use crate::stages::MemoGroupId;
use crate::summary_type::{SketchStatistic, SummaryUpdate};

/// `Scan metric{predicates}`: `[ts, value, columns…]`, `ts` its time index.
pub fn scan(
    metric: &str,
    columns: &[(&str, DataType)],
    predicates: Vec<ScalarExpr<MemoGroupId>>,
) -> Operator<MemoGroupId> {
    let fields = [
        Field::plain("ts", DataType::Timestamp, false),
        Field::plain("value", DataType::Float64, false),
    ]
    .into_iter()
    .chain(
        columns
            .iter()
            .map(|(name, dtype)| Field::plain(*name, dtype.clone(), false)),
    )
    .collect();
    Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: metric.into(),
        },
        predicates: predicates.into_iter().map(Predicate).collect(),
        schema: Schema::with_time_index(fields, 0, vec![]),
    })
}

/// Column `column` of scan `scan_ordinal` of the series `metric`.
pub fn series_column(metric: &str, scan_ordinal: u32, column: &str) -> SourceColumn {
    SourceColumn {
        source: Source::TimeSeries {
            metric: metric.into(),
        },
        scan_ordinal,
        column: column.into(),
    }
}

/// Column `column` of scan `scan_ordinal` of the table `table`.
pub fn table_column(table: &str, scan_ordinal: u32, column: &str) -> SourceColumn {
    SourceColumn {
        source: Source::Table {
            table_ref: table.into(),
        },
        scan_ordinal,
        column: column.into(),
    }
}

pub fn filter(pred: ScalarExpr<MemoGroupId>, child: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::Filter {
        pred: Predicate(pred),
        child,
    })
}

/// `TimeRange[width_ms]`: a PromQL range `[w]`.
pub fn range(width_ms: i64, child: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_millis(width_ms as u64),
        kind: TimeRangeKind::Range,
        child,
    })
}

/// `TimeShift(offset_ms, at)`: PromQL `offset` and `@`.
pub fn shift(offset_ms: i64, at: Option<AtModifier>, child: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::TimeShift {
        shift: TimeShift { offset_ms, at },
        child,
    })
}

/// `SummaryAgg[summary_type]` of the sample value per series.
pub fn state(summary_type: FieldDataType, child: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::ASAP(ASAPOp::SummaryAgg {
        child,
        summary_type,
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        filter: None,
    })
}

/// `Aggregate(reduction; measure)` over `child`, over every row.
pub fn aggregate(
    reduction: Reduction,
    measure: AggIntent,
    child: MemoGroupId,
) -> Operator<MemoGroupId> {
    aggregate_with(reduction, vec![(measure, None)], None, child)
}

/// `Aggregate(reduction; measures)` over `child` with `having`: each measure
/// unnamed, over the rows its filter keeps (`None`: every row). With no
/// filter the filter list is empty, as the frontends write it.
pub fn aggregate_with(
    reduction: Reduction,
    measures: Vec<(AggIntent, Option<Predicate<MemoGroupId>>)>,
    having: Option<Predicate<MemoGroupId>>,
    child: MemoGroupId,
) -> Operator<MemoGroupId> {
    let (measures, mut filters): (Vec<_>, Vec<_>) = measures.into_iter().unzip();
    if filters.iter().all(Option::is_none) {
        filters.clear();
    }
    Operator::NonASAP(NonASAPOp::Aggregate {
        reduction,
        output_names: vec![String::new(); measures.len()],
        measures,
        filters,
        having,
        child,
    })
}

/// `SummaryEstimate[query](state)`: a readout of `state`.
pub fn readout(query: SketchStatistic, state: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input: state,
        query,
    })
}
