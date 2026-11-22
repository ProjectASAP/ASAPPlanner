//! Step 5, compressed-sample chains (design doc
//! `compressed-sample-windows.md`, section 3.1): for each raw-range group
//! `TimeRange(W) ← TimeShift* ← Scan` of a PromQL metric, a
//! `SummaryTumblingWindowChain[CompressedSamples, w]` group over a scan of
//! the metric's time, value and series identity, and a
//! `DecodeSamplesInRange` over it as a candidate of the range group: one per
//! ladder width reading the chain's open window, and one per width that
//! divides the readers' grid reading sealed windows only.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::rules::candidates::{add_candidate_if_output_matches, add_group_computing_itself};
use crate::Pass2RuleError;
use asap_types::ir::operator::arguments::{Reduction, Source, TimeShift};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema, PROMQL_SERIES_IDENTITY};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, TimeRangeKind};
use asap_types::stages::{Memo, MemoGroupId, OriginOfCandidateForMemoGroup};
use asap_types::summary_type::SummaryUpdate;
use asap_types::workload::greatest_common_divisor;

/// The widths a chain whose decodes read the open window is offered at: the
/// multi-level window ladder, each at most the metric's longest range.
const LADDER_WIDTHS_MS: [u64; 3] = [60_000, 300_000, 900_000];

/// A raw-range group and what its decodes need.
struct RawRange {
    group: MemoGroupId,
    range: Duration,
    kind: TimeRangeKind,
    /// Top down, plain offsets only.
    shifts: Vec<TimeShift>,
    /// The scan of time, value and identity the chain is built over, with
    /// the range's scan's source and matchers.
    chain_input: Operator<MemoGroupId>,
    /// The label columns the range's scan carries besides those.
    labels: Vec<Field>,
}

pub(crate) fn add_compressed_sample_chains(
    memo: &mut Memo,
    evaluation_period_ms_per_query: &[BTreeMap<usize, Option<u64>>],
) -> Result<(), Pass2RuleError> {
    let ranges: Vec<RawRange> = (0..memo.groups().len())
        .filter_map(|index| raw_range(memo, MemoGroupId(index as u32)))
        .collect();
    // The longest range read from each chain input: a ladder width longer
    // than it would hold windows nobody reads whole.
    let range_ms = |range: &RawRange| u64::try_from(range.range.as_millis()).unwrap_or(u64::MAX);
    let longest_range_ms: Vec<u64> = ranges
        .iter()
        .map(|range| {
            ranges
                .iter()
                .filter(|other| other.chain_input == range.chain_input)
                .map(range_ms)
                .max()
                .unwrap_or(0)
        })
        .collect();
    for (range, longest_ms) in ranges.iter().zip(longest_range_ms) {
        let ladder = LADDER_WIDTHS_MS
            .into_iter()
            .filter(|&width| width <= longest_ms);
        let aligned = aligned_widths(
            range,
            &evaluation_period_ms_per_query[range.group.0 as usize],
        );
        let rows = memo.find_or_add_group(range.chain_input.clone())?;
        add_candidate_if_output_matches(
            memo,
            rows,
            range.chain_input.clone(),
            None,
            OriginOfCandidateForMemoGroup::CompressedSampleWindows,
        )?;
        let reads = ladder
            .map(|width| (width, true))
            .chain(aligned.into_iter().map(|width| (width, false)));
        for (width, reads_unsealed_window) in reads {
            let chain = Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                child: rows,
                summary_type: FieldDataType::CompressedSamples,
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::PerEntity,
                filter: None,
                tumbling_window_width_ms: width,
            });
            let mut top = memo.find_or_add_group(chain.clone())?;
            add_candidate_if_output_matches(
                memo,
                top,
                chain,
                None,
                OriginOfCandidateForMemoGroup::CompressedSampleWindows,
            )?;
            for shift in range.shifts.iter().rev() {
                top = add_group_computing_itself(
                    memo,
                    Operator::NonASAP(NonASAPOp::TimeShift {
                        shift: *shift,
                        child: top,
                    }),
                    OriginOfCandidateForMemoGroup::CompressedSampleWindows,
                )?;
            }
            add_candidate_if_output_matches(
                memo,
                range.group,
                Operator::ASAP(ASAPOp::DecodeSamplesInRange {
                    child: top,
                    range: range.range,
                    kind: range.kind,
                    labels: range.labels.clone(),
                    reads_unsealed_window,
                }),
                None,
                OriginOfCandidateForMemoGroup::CompressedSampleWindows,
            )?;
        }
    }
    Ok(())
}

/// `group` as a [`RawRange`]: a `TimeRange` with a positive range over
/// plain offsets (no `@`) over a PromQL metric's scan holding the series
/// identity and a `Float64` `value`. `None` for any other group: a SQL
/// table, whose rows need not form series, is not offered.
fn raw_range(memo: &Memo, group: MemoGroupId) -> Option<RawRange> {
    let expression = |group: MemoGroupId| &memo.groups()[group.0 as usize].expression;
    let Operator::NonASAP(NonASAPOp::TimeRange { range, kind, child }) = expression(group) else {
        return None;
    };
    if range.is_zero() {
        return None;
    }
    let (mut shifts, mut rows) = (Vec::new(), *child);
    while let Operator::NonASAP(NonASAPOp::TimeShift { shift, child }) = expression(rows) {
        if shift.at.is_some() {
            return None;
        }
        shifts.push(*shift);
        rows = *child;
    }
    let Operator::NonASAP(NonASAPOp::Scan {
        source: source @ Source::TimeSeries { .. },
        predicates,
        schema,
    }) = expression(rows)
    else {
        return None;
    };
    // Matchers read label columns, so a filtered scan keeps its columns
    // and its chain carries them (no label to derive); an unfiltered one is
    // narrowed to time, value and identity, shared by every projection.
    let (chain_schema, labels) = match predicates.is_empty() {
        true => identity_keyed(schema)?,
        false => {
            identity_keyed(schema)?;
            (schema.clone(), Vec::new())
        }
    };
    Some(RawRange {
        group,
        range: *range,
        kind: *kind,
        shifts,
        chain_input: Operator::NonASAP(NonASAPOp::Scan {
            source: source.clone(),
            predicates: predicates.clone(),
            schema: chain_schema,
        }),
        labels,
    })
}

/// The schema of time, value and series identity of a series scan's
/// `schema` (keyed by identity and time), and its other columns, the
/// labels. `None` without a time index, a `Float64` `value` or an identity.
fn identity_keyed(schema: &Schema) -> Option<(Schema, Vec<Field>)> {
    let time = schema.fields.get(schema.time_index?)?;
    let value = schema.fields.iter().find(|field| {
        field.name == "value" && field.dtype == FieldDataType::Plain(DataType::Float64)
    })?;
    let identity = schema
        .fields
        .iter()
        .find(|field| field.name == PROMQL_SERIES_IDENTITY)?;
    let labels = schema
        .fields
        .iter()
        .filter(|field| ![&time.name, &value.name, &identity.name].contains(&&field.name))
        .cloned()
        .collect();
    let mut keyed = Schema::with_time_index(
        vec![time.clone(), value.clone(), identity.clone()],
        0,
        vec![vec![2, 0]],
    );
    keyed.closed = schema.closed;
    Some((keyed, labels))
}

/// The widths whose windows end on every evaluation of `range`'s readers:
/// the gcd closure of `gcd(period, W, offsets)` over the queries with a
/// known period, as `add_chain_groups` offers its chains.
fn aligned_widths(range: &RawRange, periods_ms: &BTreeMap<usize, Option<u64>>) -> BTreeSet<u64> {
    let range_ms = u64::try_from(range.range.as_millis()).unwrap_or(u64::MAX);
    let divisor = range.shifts.iter().fold(range_ms, |gcd, shift| {
        greatest_common_divisor(gcd, shift.offset_ms.unsigned_abs())
    });
    let mut closure = BTreeSet::new();
    for width in periods_ms
        .values()
        .filter_map(|&period_ms| Some(greatest_common_divisor(period_ms?, divisor)))
        .filter(|&width| width > 0)
    {
        let gcds: Vec<u64> = closure
            .iter()
            .map(|&known| greatest_common_divisor(known, width))
            .collect();
        closure.insert(width);
        closure.extend(gcds);
    }
    closure
}

/// Whether `group` is a compressed-samples chain group.
pub(crate) fn is_compressed_samples_chain(memo: &Memo, group: MemoGroupId) -> bool {
    matches!(
        memo.groups()[group.0 as usize].expression,
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            summary_type: FieldDataType::CompressedSamples,
            ..
        })
    )
}
