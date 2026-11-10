//! The memo expressions `memo_group_properties` and `memo_summary_coverage`
//! build their examples from.
use std::time::Duration;

use asap_types::ir::operator::arguments::{AtModifier, Reduction, Source, TimeShift};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, Predicate, ScalarExpr, TimeRangeKind};
use asap_types::stages::MemoGroupId;
use asap_types::summary_type::SummaryUpdate;

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
