//! Step 1: rollup from the finest common group, one file per concern:
//! compensating filters and matchers, the finest common group, and the
//! measures that roll up.
mod filters;
mod finest_common_group;
mod measures;

use asap_types::ir::operator::arguments::Source;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::{NonASAPOp, Operator, Predicate, TimeRangeKind};
use asap_types::stages::{Memo, MemoGroupId};
use asap_types::test_support::memo_builders::*;
use std::time::Duration;

/// The instant vector `x` over `scan`: its latest sample per series.
pub(super) fn instant(memo: &mut Memo, scan: MemoGroupId) -> MemoGroupId {
    group(
        memo,
        Operator::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_secs(1),
            kind: TimeRangeKind::Instant,
            child: scan,
        }),
    )
}

/// A scan of the SQL table `flows(src, dst, bytes)` with `predicates`.
pub(super) fn flows(memo: &mut Memo, predicates: Vec<Predicate<MemoGroupId>>) -> MemoGroupId {
    group(
        memo,
        Operator::NonASAP(NonASAPOp::Scan {
            source: Source::Table {
                table_ref: "flows".into(),
            },
            predicates,
            schema: Schema::new(vec![
                Field::plain("src", DataType::Utf8, false),
                Field::plain("dst", DataType::Utf8, false),
                Field::plain("bytes", DataType::Float64, false),
            ]),
        }),
    )
}
