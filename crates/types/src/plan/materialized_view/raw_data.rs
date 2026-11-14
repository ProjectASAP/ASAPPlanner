//! How far back a runtime keeps each raw-data source for the DAG's raw-data
//! scans ([`PhysicalASAPDAG::raw_data_retention`]), which nodes keep rows
//! ([`PhysicalASAPDAGNode::retains_rows`]), and the shape checks of every
//! raw-data window ([`PhysicalASAPDAG::check_raw_data_windows`]).

use super::views::ViewIndex;
use super::{InstallableDagError, RawDataRetention};
use crate::ir::operator::asap::ASAPOp;
use crate::ir::operator::node::Operator;
use crate::ir::operator::non_asap::NonASAPOp;
use crate::ir::properties::execution_data_state::ExecutionTiming;
use crate::plan::physical_asap_dag::{PhysicalASAPDAG, PhysicalASAPDAGNode, PhysicalASAPNodeId};
use crate::stages::Lifetime;

impl PhysicalASAPDAG {
    /// For each raw-data source some raw-data `Scan` (at query time, or
    /// materialized at ingestion time) reads, in the order of its first such
    /// `Scan`, the longest `retention_ms` among those
    /// scans, `None` when one reads it unbounded (a consumer reads it
    /// outside a `TimeRange` or chain): the raw data a runtime keeps for the
    /// DAG's queries, the raw-data counterpart of each view's retention. A
    /// source read only at ingestion time is absent. Checks that each scan
    /// read through windows has a retention covering what the `TimeRange`s
    /// and `TimeShift`s above it read back.
    pub fn raw_data_retention(&self) -> Result<Vec<RawDataRetention>, InstallableDagError> {
        let index = ViewIndex::new(self);
        let mut retention: Vec<RawDataRetention> = Vec::new();
        for node in self.nodes.iter().filter(|node| is_raw_data_scan(node)) {
            let Operator::NonASAP(NonASAPOp::Scan { source, .. }) = &node.payload else {
                continue;
            };
            let lookback_ms = match (index.read_back_ms(node.id), node.retention_ms) {
                (None, _) => None,
                (Some(read_back_ms), Some(kept_ms)) if kept_ms >= read_back_ms => Some(kept_ms),
                (Some(_), Some(_)) => {
                    return Err(InstallableDagError::ScanRetentionTooShort { node_id: node.id })
                }
                (Some(_), None) => {
                    return Err(InstallableDagError::ScanWithoutRetention { node_id: node.id })
                }
            };
            match retention
                .iter_mut()
                .find(|retained| &retained.source == source)
            {
                Some(retained) => {
                    retained.lookback_ms =
                        retained.lookback_ms.zip(lookback_ms).map(|(a, b)| a.max(b))
                }
                None => retention.push(RawDataRetention {
                    source: source.clone(),
                    lookback_ms,
                }),
            }
        }
        Ok(retention)
    }

    /// Checks that every raw-data window the DAG reads (each `TimeRange`,
    /// and each `SummaryTumblingWindowChain`'s tumbling window, reading a
    /// `Scan` without passing another `TimeRange`) has a supported shape
    /// ([`InstallableDagError::UnsupportedRawDataWindow`]). O(nodes + edges)
    /// plus the depth below each window.
    pub fn check_raw_data_windows(&self) -> Result<(), InstallableDagError> {
        ViewIndex::new(self).check_raw_data_windows()
    }
}

/// Whether `node` is a `Scan` whose raw data is kept for its consumers to
/// read back: one run at query time, or materialized across evaluations
/// at ingestion time. It carries how long its source's rows are retained
/// for it, and is no view: raw data is stored by its `Source`.
pub(crate) fn is_raw_data_scan(node: &PhysicalASAPDAGNode) -> bool {
    matches!(node.payload, Operator::NonASAP(NonASAPOp::Scan { .. }))
        && (node.output_state.timing == ExecutionTiming::QueryTime
            || matches!(node.lifetime, Lifetime::AcrossEvaluations(_)))
}

impl PhysicalASAPDAGNode {
    /// Whether it keeps time-indexed rows for its readers to read back, so
    /// carries `retention_ms`: materialized across evaluations (a view, or
    /// raw data kept at ingestion), or a raw-data `Scan` at query time.
    ///
    /// Example: a query-time `Scan` of `m` read by `TimeRange(5m)` keeps 5
    /// minutes of `m`; a `OneEvaluation` merge keeps nothing.
    pub fn retains_rows(&self) -> bool {
        matches!(self.lifetime, Lifetime::AcrossEvaluations(_)) || is_raw_data_scan(self)
    }

    /// Whether it is a materialized view: materialized across evaluations
    /// and not raw data kept for its source (a `Scan`), so it keeps what
    /// its `retention_ms` covers.
    ///
    /// Example: a stored chain or `TimeShift` is a view; a `Scan` kept at
    /// ingestion time is not.
    pub fn is_view(&self) -> bool {
        matches!(self.lifetime, Lifetime::AcrossEvaluations(_)) && !is_raw_data_scan(self)
    }

    /// Whether it keeps raw rows stored at ingestion time across
    /// evaluations: a `Scan`, or a `TimeShift` without `@` whose input
    /// (`input`, its first producer's operator) is a `Scan`. Its readers
    /// read the rows it retains; an ingestion-time reader reads one batch
    /// of them (`asap_cost::reads_one_batch_of`).
    pub fn keeps_raw_rows_at_ingestion<C>(&self, input: Option<&Operator<C>>) -> bool {
        self.output_state.timing == ExecutionTiming::IngestionTime
            && matches!(self.lifetime, Lifetime::AcrossEvaluations(_))
            && match &self.payload {
                Operator::NonASAP(NonASAPOp::Scan { .. }) => true,
                Operator::NonASAP(NonASAPOp::TimeShift { shift, .. }) => {
                    shift.at.is_none()
                        && matches!(input, Some(Operator::NonASAP(NonASAPOp::Scan { .. })))
                }
                _ => false,
            }
    }
}

impl ViewIndex<'_> {
    /// See [`PhysicalASAPDAG::check_raw_data_windows`].
    pub(super) fn check_raw_data_windows(&self) -> Result<(), InstallableDagError> {
        for node in &self.dag.nodes {
            if !self.reading_raw_data.contains(&node.id) {
                continue;
            }
            match &node.payload {
                Operator::NonASAP(NonASAPOp::TimeRange { .. }) => {
                    self.check_raw_window_at(node.id)?;
                }
                Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. }) => {
                    self.check_raw_window_below(node.id)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Checks the shape of the raw-data window read through the `TimeRange`
    /// `time_range` ([`Self::check_raw_window_below`]).
    pub(super) fn check_raw_window_at(
        &self,
        time_range: PhysicalASAPNodeId,
    ) -> Result<(), InstallableDagError> {
        let Some(NonASAPOp::TimeRange { .. }) = self.relational(time_range) else {
            return Err(InstallableDagError::UnsupportedRawDataWindow {
                time_range_node_id: time_range,
                reason: "not a TimeRange",
            });
        };
        self.check_raw_window_below(time_range)
    }

    /// Checks that `time_range` (a `TimeRange`, or a
    /// `SummaryTumblingWindowChain`'s tumbling window) reads the `Scan`
    /// below it through `TimeShift`s without `@` summing to a non-negative
    /// offset, or says why its shape is unsupported. A `TimeRange` run at
    /// ingestion time may have no `TimeShift` or subquery above it; a query
    /// run evaluates one, and a chain's windows are selected by the
    /// `TimeRange`s above it.
    pub(super) fn check_raw_window_below(
        &self,
        time_range: PhysicalASAPNodeId,
    ) -> Result<(), InstallableDagError> {
        let unsupported = |reason| InstallableDagError::UnsupportedRawDataWindow {
            time_range_node_id: time_range,
            reason,
        };
        let node = self.nodes[&time_range];
        if self.re_timed_from_above.contains(&time_range)
            && node.output_state.timing == ExecutionTiming::IngestionTime
            && matches!(node.payload, Operator::NonASAP(NonASAPOp::TimeRange { .. }))
        {
            return Err(unsupported("a subquery or a TimeShift above the range"));
        }
        let mut end_offset_ms = 0i64;
        let mut current = time_range;
        loop {
            current = self.only_input(current).ok_or(unsupported(
                "an operator with several inputs below the range",
            ))?;
            match self.relational(current) {
                Some(NonASAPOp::TimeShift { shift, .. }) if shift.at.is_some() => {
                    return Err(unsupported("an @ anchor below the range"))
                }
                Some(NonASAPOp::TimeShift { shift, .. }) => {
                    end_offset_ms = end_offset_ms
                        .checked_add(shift.offset_ms)
                        .ok_or(unsupported("window offset overflows"))?;
                }
                Some(NonASAPOp::Scan { .. }) if end_offset_ms < 0 => {
                    return Err(unsupported("a negative offset below the range"))
                }
                Some(NonASAPOp::Scan { .. }) => return Ok(()),
                _ => {
                    return Err(unsupported(
                        "an operator other than TimeShift below the range",
                    ))
                }
            }
        }
    }
}
