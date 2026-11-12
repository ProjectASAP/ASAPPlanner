//! What a deployment runtime keeps for an installed physical ASAP DAG: its
//! materialized views ([`PhysicalASAPDAG::materialized_views`]) and how far
//! back it keeps each raw-data source for raw-data scans
//! ([`PhysicalASAPDAG::raw_data_retention`]), derived from the DAG and the
//! `retention_ms` Stage 3 sets on its materialized nodes and query-time
//! `Scan`s, which these functions check.
//!
//! A **materialized view** is a node whose output is materialized across
//! evaluations: its lifetime is `AcrossEvaluations`, at ingestion time or
//! at query time, and it carries its `retention_ms`. A node materialized for
//! `OneEvaluation` is not a view: its output is discarded after one
//! evaluation.
//!
//! ## The time-indexed store
//!
//! A runtime keeps every view, and the raw data of every raw-data `Scan`,
//! in one time-indexed, append-only store: a `SummaryTumblingWindowChain`'s
//! rows by the end of their window, any other view's state by the interval
//! of data it summarizes, raw rows by their time. Each is read only through
//! the `TimeRange` and `TimeShift` nodes above it, which select rows by time:
//! over a chain, `TimeRange(W)` over shifts summing to `o` selects, at
//! evaluation time `T`, the windows ending in `(T - o - W, T - o]`. A node's
//! `retention_ms` is how far back from the newest evaluation time its rows
//! are kept; it covers what those `TimeRange`s and `TimeShift`s above it
//! read back (`o + W`), which [`PhysicalASAPDAG::materialized_views`] and
//! [`PhysicalASAPDAG::raw_data_retention`] check, and older rows are dropped.
//!
//! ## Refreshing a view
//!
//! The views are in dependency order: every view a refresh reads is listed
//! before it. A runtime refreshes a view once all of its inputs are
//! complete: each raw-data window's samples have arrived (sealed) and each
//! view it reads is refreshed. It compiles the DAG with the executor for
//! that view's node (for a view refreshed at ingestion, together with the
//! rest of its refresh group, below), with each view it reads that the run
//! does not refresh as a materialized view slot and raw data read by
//! `Source` at each `Scan`. It refreshes the view in an ingestion run
//! ([`MaterializedViewRefresh::AtIngestion`]) or in the query run of the
//! evaluation that first reads it ([`MaterializedViewRefresh::OnFirstRead`])
//! and appends the result to the store. An ingestion run over
//! `(window_start_ms, window_end_ms]` refreshes the windows ending at
//! `window_end_ms`: a chain builds its window ending there, each
//! `TimeRange(w)` keeps `(window_end_ms - w, window_end_ms]` whatever the
//! shifts below it, and each view it reads is read for the window ending
//! there. A view refreshed in an ingestion run therefore reads windows that
//! all end at one offset before the evaluation; a DAG with one that does
//! not is refused
//! ([`InstallableDagError::IngestionBuildReadsWindowsOfDifferentOffsets`]).
//! A query run binds the views it reaches from the roots without passing
//! another view; a view only other views read is read through them.
//!
//! ## Refresh groups
//!
//! Views refreshed in an ingestion run are refreshed by **refresh group**
//! ([`PhysicalASAPDAG::ingestion_refresh_groups`]): the views whose windows
//! have one width `W` and end at one offset `p` modulo `W` (their
//! [`RefreshWindow`]; a view with an unbounded window is a group of its
//! own). A chain's window is one of its tumbling windows. A runtime
//! refreshes a group with one ingestion run per window `(e - W, e]`, `e ≡ p
//! (mod W)`, whose roots are the views of the group; a view of the group a
//! root reads is computed in the run, and views of other groups are
//! materialized view slots. A node several roots share runs once per run,
//! so a `Scan` below two chains of equal width reads each window's raw data
//! once. Stage 3's cost assumes this grouping; a runtime that splits a group
//! computes the same states at a higher cost than priced.
//!
//! Each view's `coverage` says what its stored state is: a stable id of its
//! definition (what it computes) and its selection (which rows went in,
//! with time relative to the evaluation time). Two views with the same
//! definition id and the same concrete selection (relative time made
//! absolute at an evaluation) hold the same state, so a runtime may store
//! it once.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::physical_asap_dag::{PhysicalASAPDAGValidationError, PhysicalASAPNodeId, ViewCoverage};
use super::time_window::TimeWindow;
use crate::ir::operator::arguments::Source;

/// One materialized view of a DAG: what its stored state is, what it is
/// refreshed from, when, and for how long it is retained.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializedView {
    /// The node whose output is materialized.
    pub node_id: PhysicalASAPNodeId,
    /// Which run refreshes it.
    pub(crate) refresh: MaterializedViewRefresh,
    /// What the stored state is: the node's aggregate coverage.
    pub(crate) coverage: ViewCoverage,
    /// What its refresh reads.
    pub inputs: MaterializedViewInputs,
    /// The node's `retention_ms`: how far back from the newest evaluation
    /// time its stored rows are kept. At least how far back the
    /// `TimeRange`s and `TimeShift`s above it read.
    pub(crate) retention_ms: u64,
}

impl MaterializedView {
    /// Which run refreshes it.
    pub fn refresh(&self) -> MaterializedViewRefresh {
        self.refresh
    }

    /// How far back from the newest evaluation time its stored rows are
    /// kept.
    pub fn retention_ms(&self) -> u64 {
        self.retention_ms
    }
}

#[cfg(any(test, feature = "test-support"))]
impl MaterializedView {
    /// What the stored state is.
    pub fn coverage(&self) -> &ViewCoverage {
        &self.coverage
    }
}

/// What a view's refresh reads, each bound as a source of the refresh;
/// the raw data it scans is read by `Source` through the windows below it
/// ([`PhysicalASAPDAG::refresh_window`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializedViewInputs {
    /// The views it reads, by node id, in node order; each is listed before
    /// it in [`PhysicalASAPDAG::materialized_views`].
    pub materialized_views: Vec<PhysicalASAPNodeId>,
}

/// Which run refreshes a materialized view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterializedViewRefresh {
    /// Refreshed at ingestion time (materialized at ingestion time): the runtime runs the
    /// node in an ingestion run once its inputs are complete (its windows'
    /// samples have arrived and the views it reads are refreshed), and
    /// keeps the result.
    AtIngestion,
    /// Refreshed at query time and materialized across evaluations
    /// (query-time `AcrossEvaluations`): when an evaluation reads this
    /// view for a selection it has not been refreshed for yet, the runtime
    /// runs the node in a query run over its inputs, keeps the result, and
    /// binds it like a view refreshed at ingestion.
    OnFirstRead,
}

/// The windows an ingestion run refreshes for a view
/// ([`PhysicalASAPDAG::refresh_window`]): those `width_ms` wide that end
/// `phase_ms` (`< width_ms`) after a multiple of `width_ms`, relative to the
/// evaluation time. Views refreshed at ingestion with one refresh window
/// are refreshed together, in one ingestion run per window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RefreshWindow {
    /// Width of each window.
    pub width_ms: u64,
    /// Where each window ends, modulo `width_ms`.
    pub phase_ms: u64,
}

impl RefreshWindow {
    /// The refresh window of `window`: `(width_ms, end_offset_ms mod
    /// width_ms)`; `None` for an empty window.
    pub fn of_window(window: TimeWindow) -> Option<Self> {
        let phase_ms = window.end_offset_ms.checked_rem(window.width_ms)?;
        Some(RefreshWindow {
            width_ms: window.width_ms,
            phase_ms,
        })
    }
}

/// The views refreshed at ingestion that share one refresh window
/// ([`PhysicalASAPDAG::ingestion_refresh_groups`]). A runtime refreshes
/// them with one ingestion run per window `(e - W, e]`, whose roots are its
/// views; a view of the group a root reads is computed in the run.
#[derive(Debug, Clone, PartialEq)]
pub struct IngestionRefreshGroup {
    /// The refresh window of every view of the group; `None` for a view
    /// whose window is unbounded or empty, which is a group of its own.
    pub refresh_window: Option<RefreshWindow>,
    /// The views, in dependency order.
    pub views: Vec<MaterializedView>,
}

/// How far back from the evaluation time a runtime keeps a raw-data source
/// for the DAG's raw-data scans ([`PhysicalASAPDAG::raw_data_retention`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDataRetention {
    /// The source.
    pub(crate) source: Source,
    /// The longest `retention_ms` of a raw-data `Scan` of it; `None` when
    /// one reads it unbounded (every row the source holds).
    pub(crate) lookback_ms: Option<u64>,
}

#[cfg(any(test, feature = "test-support"))]
impl RawDataRetention {
    pub fn new(source: Source, lookback_ms: Option<u64>) -> Self {
        Self {
            source,
            lookback_ms,
        }
    }

    /// The source.
    pub fn source(&self) -> &Source {
        &self.source
    }

    /// The longest `retention_ms` of a raw-data `Scan` of it; `None` when
    /// one reads it unbounded.
    pub fn lookback_ms(&self) -> Option<u64> {
        self.lookback_ms
    }
}

/// Why a runtime cannot install a DAG: a materialized view or a raw-data
/// window it cannot derive.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum InstallableDagError {
    /// Bad input: the DAG is not a valid physical ASAP DAG.
    #[error("invalid physical ASAP DAG: {0}")]
    InvalidDag(#[from] PhysicalASAPDAGValidationError),
    /// Unsatisfiable request: the DAG reads raw data through `TimeRange`
    /// `time_range_node_id` in a shape whose window is not a fixed offset
    /// from the evaluation time (`reason`: a subquery or a `TimeShift`
    /// above it, or an `@` anchor below it), or that ends after the
    /// evaluation (a negative offset below it), which no ingestion run can
    /// refresh in time. Not supported yet.
    #[error("raw-data window of node {time_range_node_id} is unsupported: {reason}")]
    UnsupportedRawDataWindow {
        time_range_node_id: PhysicalASAPNodeId,
        reason: &'static str,
    },
    /// Unsatisfiable request: view `node_id`'s refresh reads a source
    /// `Scan` other than through a raw-data `TimeRange` (e.g. a whole SQL
    /// table), which a runtime cannot feed it.
    #[error("materialized view {node_id} reads a source outside a raw-data window")]
    ViewReadsUnwindowedSource { node_id: PhysicalASAPNodeId },
    /// Unsatisfiable request: view `node_id` is not a summary state with a
    /// coverage (`reason`), so a runtime cannot say what it stores.
    #[error("materialized view {node_id} has no aggregate coverage: {reason}")]
    ViewWithoutCoverage {
        node_id: PhysicalASAPNodeId,
        reason: String,
    },
    /// Unsatisfiable request: view `node_id` is refreshed in an ingestion
    /// run but reads windows that end at different offsets before the
    /// evaluation (`end_offsets_ms`): its raw-data windows and the windows
    /// the views it reads summarize. An ingestion run refreshes the windows
    /// ending at its `window_end_ms` (each `TimeRange(w)` keeps
    /// `(window_end_ms - w, window_end_ms]`, and each view it reads is
    /// read for the window ending there), so no one run refreshes them
    /// all; a runtime cannot build this plan.
    #[error(
        "materialized view {node_id} is built in an ingestion run from windows ending at \
         different offsets {end_offsets_ms:?} ms before the evaluation, which no one \
         ingestion run builds"
    )]
    IngestionBuildReadsWindowsOfDifferentOffsets {
        node_id: PhysicalASAPNodeId,
        end_offsets_ms: Vec<u64>,
    },
    /// Bad input (a plan file): view `node_id` has no `retention_ms`.
    #[error("materialized view {node_id} has no retention_ms")]
    ViewWithoutRetention { node_id: PhysicalASAPNodeId },
    /// Bad input (a plan file): view `node_id` is retained for less than the
    /// `TimeRange`s and `TimeShift`s above it read back, so a later
    /// evaluation would miss its rows.
    #[error("materialized view {node_id} is retained for less than its consumers read back")]
    ViewRetentionTooShort { node_id: PhysicalASAPNodeId },
    /// Bad input (a plan file): raw-data `Scan` `node_id` reads a bounded
    /// window but has no `retention_ms`.
    #[error("query-time scan {node_id} reads a bounded window but has no retention_ms")]
    ScanWithoutRetention { node_id: PhysicalASAPNodeId },
    /// Bad input (a plan file): raw-data `Scan` `node_id` keeps its rows
    /// for less than its consumers' window reaches back.
    #[error("query-time scan {node_id} is retained for less than its window reaches back")]
    ScanRetentionTooShort { node_id: PhysicalASAPNodeId },
}

mod raw_data;
mod views;

pub(crate) use raw_data::is_raw_data_scan;
