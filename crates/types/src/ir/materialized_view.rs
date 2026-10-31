//! What a deployment runtime keeps for an installed physical ASAP DAG: its
//! materialized views ([`PhysicalASAPDAG::materialized_views`]) and how far
//! back it keeps each raw-data source for query-time scans
//! ([`PhysicalASAPDAG::raw_data_retention`]), derived from the DAG and the
//! `retention_ms` installation sets on its nodes
//! ([`PhysicalASAPDAG::set_retention`]).
//!
//! A **materialized view** is a node whose output is kept across
//! evaluations: its materialization is `MatAtIngestion` or
//! `MatAtQueryKeptAcrossEvaluations`, and it carries its `retention_ms`. A
//! `MatAtQuery` node is not a view: its output is discarded after one
//! evaluation. A query-time `Scan` carries how long its source's rows are
//! kept for it (`None` when it reads them unbounded).
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
//! `Source` at each `Scan`. It
//! refreshes the view in an ingestion run
//! ([`MaterializedViewRefresh::AtIngestion`]) or in the query run of the
//! evaluation that first reads it
//! ([`MaterializedViewRefresh::OnFirstRead`]), keeps the result, and binds
//! it the same way wherever the DAG reads that node. An ingestion run over
//! `(window_start_ms, window_end_ms]` refreshes the windows ending at
//! `window_end_ms`: each `TimeRange(w)` keeps `(window_end_ms - w,
//! window_end_ms]`, whatever the shifts below it, so a runtime refreshes a
//! window `(start, end]` of `raw_data_windows` ([`RawDataWindow::bounds_ms`]
//! at the evaluation it is refreshed for) in the run over `(start, end]`. A
//! view refreshed in an ingestion run therefore reads windows that all end
//! at one offset before the evaluation; a DAG with one that does not is
//! refused ([`InstallableDagError::IngestionBuildReadsWindowsOfDifferentOffsets`]).
//! A query run binds the views it reaches from the roots without passing
//! another view; a view only other views read is read through them.
//!
//! ## Refresh groups
//!
//! Views refreshed in an ingestion run are refreshed by **refresh group**
//! ([`PhysicalASAPDAG::ingestion_refresh_groups`]): the views whose windows
//! have one width `W` and end at one offset `p` modulo `W` (their
//! [`RefreshWindow`]; a view with an unbounded window is a group of its
//! own). A runtime refreshes a group with one ingestion run per window
//! `(e - W, e]`, `e ≡ p (mod W)`. The run's roots are the views of the group
//! whose state for the window ending at `e` is not stored yet: in steady
//! state the newest pane of each chain, since older panes are the same
//! states, stored by earlier runs. A view of the group that a root reads is
//! computed in the run; views of other groups are materialized view slots.
//! A node several roots share runs once per run, so a `Scan` below a KLL
//! pane chain and a DDSketch pane chain of equal pane width reads each
//! pane's raw data once. Stage 3's cost assumes this grouping; a runtime
//! that splits a group computes the same states at a higher cost than
//! priced.
//!
//! Each view's `coverage` says what its stored state is: a stable id of its
//! definition (what it computes) and its selection (which rows went in,
//! with time relative to the evaluation time). Two views with the same
//! definition id and the same concrete selection (relative time made
//! absolute at an evaluation) hold the same state, so a runtime may store
//! it once.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::physical_export::{
    offset_below, PhysicalASAPDAG, PhysicalASAPDAGNode, PhysicalASAPDAGValidationError,
    PhysicalASAPNodeId, ScanWindow,
};
use crate::ir::operator::asap::ASAPOp;
use crate::ir::operator::node::{Operator, OperatorNode};
use crate::ir::operator::non_asap::NonASAPOp;
use crate::ir::operator::operator_properties::Source;
use crate::ir::properties::execution_data_state::ExecutionTiming;
use crate::ir::properties::summary_coverage::{CoverageError, SelectionBox, SummaryCoverage};
use crate::physical::materialization::Materialization;

/// One materialized view of a DAG: what its stored state is, what it is
/// refreshed from, when, and for how long it is kept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializedView {
    /// The node whose output is kept.
    pub node_id: PhysicalASAPNodeId,
    /// Which run refreshes it.
    pub(crate) refresh: MaterializedViewRefresh,
    /// What the stored state is: the node's summary coverage.
    pub(crate) coverage: ViewCoverage,
    /// What its refresh reads.
    pub inputs: MaterializedViewInputs,
    /// The node's `retention_ms`: how far back from the newest evaluation
    /// time its output must stay available once refreshed. At least how
    /// far back the data it summarizes reaches (its raw-data windows', and
    /// the views it reads, `end_offset_ms + width_ms`).
    pub(crate) retention_ms: u64,
}

#[cfg(any(test, feature = "test-support"))]
impl MaterializedView {
    /// Which run refreshes it.
    pub fn refresh(&self) -> MaterializedViewRefresh {
        self.refresh
    }

    /// How far back from the newest evaluation time its output stays
    /// available.
    pub fn retention_ms(&self) -> u64 {
        self.retention_ms
    }

    /// What the stored state is.
    pub fn coverage(&self) -> &ViewCoverage {
        &self.coverage
    }
}

/// A view's summary coverage
/// ([`SummaryCoverage`], `docs/design_docs/proposals/asap-primitive-schema.md`
/// §4.2): what it computes and which rows went in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewCoverage {
    /// `SummaryCoverage::definition_id`: equal on every node, plan and
    /// process that computes the same thing, whatever its selection; a
    /// rollup to a coarser grouping computes something else and has its
    /// own.
    pub(crate) definition_id: String,
    /// The rows that went in: a union of boxes of value sets, value ranges
    /// and time relative to the evaluation time. A merge's is the union of
    /// its inputs'.
    pub(crate) selection: Vec<SelectionBox>,
}

#[cfg(any(test, feature = "test-support"))]
impl ViewCoverage {
    /// `SummaryCoverage::definition_id`.
    pub fn definition_id(&self) -> &str {
        &self.definition_id
    }

    /// The rows that went in.
    pub fn selection(&self) -> &[SelectionBox] {
        &self.selection
    }
}

/// What a view's refresh reads, each bound as a source of the refresh.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializedViewInputs {
    /// The windows of raw data it scans directly, one per `TimeRange` it
    /// reaches, in node order.
    pub(crate) raw_data_windows: Vec<RawDataWindow>,
    /// The views it reads, by node id, in node order; each is listed before
    /// it in [`PhysicalASAPDAG::materialized_views`].
    pub materialized_views: Vec<PhysicalASAPNodeId>,
}

#[cfg(any(test, feature = "test-support"))]
impl MaterializedViewInputs {
    /// The windows of raw data it scans directly.
    pub fn raw_data_windows(&self) -> &[RawDataWindow] {
        &self.raw_data_windows
    }
}

/// A window of a raw-data source a view's refresh scans: at evaluation time
/// `T`, the rows of `source` in `(T - end_offset_ms - width_ms, T -
/// end_offset_ms]`. Read as `TimeRange(width) <- TimeShift(offset)* <-
/// Scan`, with no `TimeShift`, `TimeRange` or subquery above the
/// `TimeRange`, so the window depends on `T` alone. For an instant selector
/// `width_ms` is its lookback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDataWindow {
    /// The source the `Scan` reads.
    pub(crate) source: Source,
    /// How long before the evaluation time the window ends: the sum of the
    /// offset `TimeShift`s between the `TimeRange` and the `Scan`.
    pub(crate) end_offset_ms: i64,
    /// Width of the window, from the `TimeRange`.
    pub(crate) width_ms: i64,
}

#[cfg(any(test, feature = "test-support"))]
impl RawDataWindow {
    /// The source the `Scan` reads.
    pub fn source(&self) -> &Source {
        &self.source
    }

    /// How long before the evaluation time the window ends.
    pub fn end_offset_ms(&self) -> i64 {
        self.end_offset_ms
    }

    /// Width of the window.
    pub fn width_ms(&self) -> i64 {
        self.width_ms
    }

    /// The window of `source` ending `end_offset_ms` before the evaluation
    /// time, `width_ms` wide.
    pub fn new(source: Source, end_offset_ms: i64, width_ms: i64) -> Self {
        Self {
            source,
            end_offset_ms,
            width_ms,
        }
    }
}

impl RawDataWindow {
    /// How far back from the evaluation time the window reaches:
    /// `end_offset_ms + width_ms`.
    pub(crate) fn lookback_ms(&self) -> u64 {
        u64::try_from(self.end_offset_ms.saturating_add(self.width_ms)).unwrap_or(0)
    }

    /// `(start, end]` of the window at evaluation time `evaluation_time_ms`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn bounds_ms(&self, evaluation_time_ms: i64) -> (i64, i64) {
        let end = evaluation_time_ms - self.end_offset_ms;
        (end - self.width_ms, end)
    }
}

/// Which run refreshes a materialized view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterializedViewRefresh {
    /// Refreshed at ingestion time (`MatAtIngestion`): the runtime runs the
    /// node in an ingestion run once its inputs are complete (its windows'
    /// samples have arrived and the views it reads are refreshed), and
    /// keeps the result.
    AtIngestion,
    /// Refreshed at query time and kept across evaluations
    /// (`MatAtQueryKeptAcrossEvaluations`): when an evaluation reads this
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
    /// The refresh window of the window `(T - end_offset_ms - width_ms, T -
    /// end_offset_ms]`: `(width_ms, end_offset_ms mod width_ms)`; `None` for
    /// an empty window (`width_ms <= 0`).
    pub fn of_window(end_offset_ms: i64, width_ms: i64) -> Option<Self> {
        let phase_ms = end_offset_ms
            .checked_rem_euclid(width_ms)
            .filter(|_| width_ms > 0)?;
        Some(RefreshWindow {
            width_ms: u64::try_from(width_ms).ok()?,
            phase_ms: u64::try_from(phase_ms).ok()?,
        })
    }
}

/// The views refreshed at ingestion that share one refresh window
/// ([`PhysicalASAPDAG::ingestion_refresh_groups`]). A runtime refreshes
/// them with one ingestion run per window `(e - W, e]`; each run's roots are
/// the views whose state for the window ending at `e` is not stored yet (in
/// steady state the newest pane of each chain), and a view of the group a
/// root reads is computed in the run.
#[derive(Debug, Clone, PartialEq)]
pub struct IngestionRefreshGroup {
    /// The refresh window of every view of the group; `None` for a view
    /// whose window is unbounded or empty, which is a group of its own.
    pub refresh_window: Option<RefreshWindow>,
    /// The views, in dependency order.
    pub views: Vec<MaterializedView>,
}

/// How far back from the evaluation time a runtime keeps a raw-data source
/// for the DAG's query-time scans ([`PhysicalASAPDAG::raw_data_retention`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDataRetention {
    /// The source.
    pub(crate) source: Source,
    /// The longest `retention_ms` of a query-time `Scan` of it; `None` when
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

    /// The longest `retention_ms` of a query-time `Scan` of it; `None` when
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
    #[error("materialized view {node_id} has no summary coverage: {reason}")]
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
        end_offsets_ms: Vec<i64>,
    },
    /// Bad input (a plan file): view `node_id` has no `retention_ms`.
    #[error("materialized view {node_id} has no retention_ms")]
    ViewWithoutRetention { node_id: PhysicalASAPNodeId },
    /// Bad input (a plan file): view `node_id` is kept for less than the
    /// data it summarizes reaches back, so a later evaluation would miss
    /// it.
    #[error("materialized view {node_id} is kept for less than its data reaches back")]
    ViewRetentionTooShort { node_id: PhysicalASAPNodeId },
    /// Bad input (a plan file): query-time `Scan` `node_id` reads a bounded
    /// window but has no `retention_ms`.
    #[error("query-time scan {node_id} reads a bounded window but has no retention_ms")]
    ScanWithoutRetention { node_id: PhysicalASAPNodeId },
    /// Bad input (a plan file): query-time `Scan` `node_id` keeps its rows
    /// for less than its readers' window reaches back.
    #[error("query-time scan {node_id} is kept for less than its window reaches back")]
    ScanRetentionTooShort { node_id: PhysicalASAPNodeId },
    /// The caller's deadline passed while deriving the views or retention.
    #[error("deadline exceeded deriving materialized views")]
    DeadlineExceeded,
}

fn check_deadline(deadline: Option<Instant>) -> Result<(), InstallableDagError> {
    match deadline {
        Some(limit) if Instant::now() >= limit => Err(InstallableDagError::DeadlineExceeded),
        _ => Ok(()),
    }
}

impl PhysicalASAPDAG {
    /// Every materialized view, in dependency order (each after the views
    /// it reads), with its refresh mode, coverage, inputs and the node's
    /// `retention_ms`. Validates the DAG and checks that every raw-data
    /// window it reads has a supported shape and that each view's retention
    /// covers how far back its data reaches. O(views · nodes) for the
    /// coverages, plus O(nodes + edges).
    pub fn materialized_views(&self) -> Result<Vec<MaterializedView>, InstallableDagError> {
        if let Some(node) = self
            .nodes
            .iter()
            .find(|node| node.materialization.is_materialized_view() && node.retention_ms.is_none())
        {
            return Err(InstallableDagError::ViewWithoutRetention { node_id: node.id });
        }
        self.validate()?;
        let views: Vec<MaterializedView> = self
            .views_with_node_retention(None)?
            .into_iter()
            .map(|(view, _)| view)
            .collect();
        let mut reach_ms_by_node = HashMap::new();
        for view in &views {
            let reach_ms = reach_ms_of(&view.inputs, &reach_ms_by_node);
            if view.retention_ms < reach_ms {
                return Err(InstallableDagError::ViewRetentionTooShort {
                    node_id: view.node_id,
                });
            }
            reach_ms_by_node.insert(view.node_id, reach_ms);
        }
        Ok(views)
    }

    /// The views refreshed at ingestion, by refresh group: the views with
    /// one [`RefreshWindow`] ([`Self::refresh_window`]) form one group,
    /// which a runtime refreshes with one ingestion run per window
    /// ([`IngestionRefreshGroup`]). Each group lists its views in dependency
    /// order and comes after the groups whose views it reads. Errs as
    /// [`Self::materialized_views`].
    pub fn ingestion_refresh_groups(
        &self,
    ) -> Result<Vec<IngestionRefreshGroup>, InstallableDagError> {
        Ok(self.ingestion_refresh_groups_of(&self.materialized_views()?))
    }

    /// [`Self::ingestion_refresh_groups`] of `views`, this DAG's
    /// materialized views ([`Self::materialized_views`]), for a caller that
    /// already derived them.
    pub fn ingestion_refresh_groups_of(
        &self,
        views: &[MaterializedView],
    ) -> Vec<IngestionRefreshGroup> {
        let views: Vec<MaterializedView> = views
            .iter()
            .filter(|view| view.refresh == MaterializedViewRefresh::AtIngestion)
            .cloned()
            .collect();
        let refresh_windows = self.refresh_windows(views.iter().map(|view| view.node_id));
        let mut groups: Vec<IngestionRefreshGroup> = Vec::new();
        let mut group_by_window: HashMap<RefreshWindow, usize> = HashMap::new();
        let mut group_by_view: HashMap<PhysicalASAPNodeId, usize> = HashMap::new();
        for view in views {
            let refresh_window = refresh_windows[&view.node_id];
            let group = match refresh_window.and_then(|window| group_by_window.get(&window)) {
                Some(&group) => group,
                None => {
                    groups.push(IngestionRefreshGroup {
                        refresh_window,
                        views: Vec::new(),
                    });
                    if let Some(window) = refresh_window {
                        group_by_window.insert(window, groups.len() - 1);
                    }
                    groups.len() - 1
                }
            };
            group_by_view.insert(view.node_id, group);
            groups[group].views.push(view);
        }
        // The other groups each group reads views of.
        let reads: Vec<Vec<usize>> = groups
            .iter()
            .enumerate()
            .map(|(group, of)| {
                of.views
                    .iter()
                    .flat_map(|view| &view.inputs.materialized_views)
                    .filter_map(|read| group_by_view.get(read).copied())
                    .filter(|&read| read != group)
                    .collect()
            })
            .collect();
        // Each group after those it reads: a depth-first postorder. A view's
        // window holds the windows of the views it reads, all ending where
        // it ends, so two groups never read each other.
        fn visit(group: usize, reads: &[Vec<usize>], placed: &mut [bool], order: &mut Vec<usize>) {
            if std::mem::replace(&mut placed[group], true) {
                return;
            }
            for &read in &reads[group] {
                visit(read, reads, placed, order);
            }
            order.push(group);
        }
        let mut placed = vec![false; groups.len()];
        let mut order = Vec::with_capacity(groups.len());
        for group in 0..groups.len() {
            visit(group, &reads, &mut placed, &mut order);
        }
        let mut groups: Vec<Option<IngestionRefreshGroup>> = groups.into_iter().map(Some).collect();
        order
            .into_iter()
            .filter_map(|group| groups[group].take())
            .collect()
    }

    /// The refresh window of `node` ([`RefreshWindow::of_window`]): for a
    /// `PaneChain` (a DAG with its chains unexpanded), its newest pane's,
    /// `(pane_width, s mod pane_width)` with `s` the sum of the offsets of
    /// the `TimeShift`s below it; for any other node, that of its window
    /// ([`Self::view_windows`], which counts a chain below it as all its
    /// panes). A chain and each of its expanded panes have the same one.
    /// `None` when the window is unbounded or empty. O(nodes + edges).
    pub fn refresh_window(&self, node: PhysicalASAPNodeId) -> Option<RefreshWindow> {
        self.refresh_windows([node])[&node]
    }

    /// [`Self::refresh_window`] of each of `nodes`, by id, with the DAG
    /// indexed once for all of them.
    pub fn refresh_windows(
        &self,
        nodes: impl IntoIterator<Item = PhysicalASAPNodeId>,
    ) -> HashMap<PhysicalASAPNodeId, Option<RefreshWindow>> {
        let payloads = self.payloads_by_id();
        let (chains, others): (Vec<_>, Vec<_>) = nodes.into_iter().partition(|id| {
            matches!(
                payloads.get(id),
                Some(Operator::ASAP(ASAPOp::PaneChain { .. }))
            )
        });
        let mut windows: HashMap<PhysicalASAPNodeId, Option<RefreshWindow>> = self
            .view_windows(others)
            .into_iter()
            .map(|(id, window)| {
                let refresh_window = match window {
                    ScanWindow::Within {
                        end_offset_ms,
                        width_ms,
                    } => RefreshWindow::of_window(end_offset_ms, width_ms),
                    ScanWindow::Unbounded => None,
                };
                (id, refresh_window)
            })
            .collect();
        for chain in chains {
            let Some(Operator::ASAP(ASAPOp::PaneChain {
                child,
                pane_width_ms,
                ..
            })) = payloads.get(&chain)
            else {
                continue;
            };
            // The newest pane ends at the offsets of the shifts below the chain.
            let end_offset_ms = offset_below(*child, |id| payloads.get(&id).copied());
            let width_ms = i64::try_from(*pane_width_ms).unwrap_or(0);
            windows.insert(
                chain,
                end_offset_ms.and_then(|offset| RefreshWindow::of_window(offset, width_ms)),
            );
        }
        windows
    }

    /// Sets `retention_ms` on every materialized view and every query-time
    /// `Scan` of an installed DAG (pane chains expanded), and returns the
    /// views ([`Self::materialized_views`]); `Err` when `deadline` passes.
    ///
    /// A view takes `pane_retention_ms_by_node`'s value (Stage 3's retention
    /// of a pane's chain: its offset plus its window plus the longest
    /// range-evaluation span of its readers); otherwise how far back its
    /// data reaches, plus, for a view built from other views, its readers'
    /// longest range-evaluation span (`reader_span_ms_by_node`). Then each
    /// view is kept at least as long as every view built from it needs it:
    /// as long as that view's own retention when it is refreshed on first
    /// read (a range request may refresh it for its oldest step), else as
    /// far back as that view's data reaches. This is each node's own
    /// requirement: a runtime that stores identical outputs of several
    /// nodes once keeps them for the largest requirement among those nodes.
    ///
    /// A query-time `Scan` keeps its rows for its readers' window
    /// ([`Self::scan_window`]'s `lookback_ms`) plus their longest
    /// range-evaluation span, and at least the retention of every view
    /// refreshed on first read from it (which may be refreshed for its
    /// oldest step); `None` when its window is unbounded. In an installed
    /// DAG a chain kept across evaluations is its expanded panes, so its
    /// scan keeps the reach of all of them: reusing a pane refreshed at an
    /// earlier evaluation is optional, and a restart or an older range step
    /// refreshes older panes from raw data. (Stage 3 prices a kept chain on
    /// the unexpanded DAG as reading its newest pane.)
    pub fn set_retention(
        &mut self,
        pane_retention_ms_by_node: &HashMap<PhysicalASAPNodeId, u64>,
        reader_span_ms_by_node: &HashMap<PhysicalASAPNodeId, u64>,
        deadline: Option<Instant>,
    ) -> Result<Vec<MaterializedView>, InstallableDagError> {
        let (mut views, scans_read): (Vec<MaterializedView>, Vec<Vec<PhysicalASAPNodeId>>) = self
            .views_with_node_retention(deadline)?
            .into_iter()
            .unzip();
        let mut reach_ms_by_node = HashMap::new();
        let mut position_of = HashMap::new();
        for (position, view) in views.iter_mut().enumerate() {
            let reach_ms = reach_ms_of(&view.inputs, &reach_ms_by_node);
            let span_ms = match view.inputs.materialized_views.is_empty() {
                true => 0,
                false => reader_span_ms_by_node
                    .get(&view.node_id)
                    .copied()
                    .unwrap_or(0),
            };
            view.retention_ms = pane_retention_ms_by_node
                .get(&view.node_id)
                .copied()
                .unwrap_or(reach_ms + span_ms);
            reach_ms_by_node.insert(view.node_id, reach_ms);
            position_of.insert(view.node_id, position);
        }
        // Views before their inputs: an input stays while every view built
        // from it may still be refreshed.
        for index in (0..views.len()).rev() {
            check_deadline(deadline)?;
            let view = &views[index];
            let needed_ms = match view.refresh {
                MaterializedViewRefresh::OnFirstRead => view.retention_ms,
                MaterializedViewRefresh::AtIngestion => reach_ms_by_node[&view.node_id],
            };
            for input in view.inputs.materialized_views.clone() {
                let kept = &mut views[position_of[&input]].retention_ms;
                *kept = (*kept).max(needed_ms);
            }
        }
        let mut retention_ms_by_node: HashMap<PhysicalASAPNodeId, Option<u64>> = views
            .iter()
            .map(|view| (view.node_id, Some(view.retention_ms)))
            .collect();
        // How long each scan's rows are needed by the views refreshed on
        // first read from it.
        let mut view_need_ms_by_scan: HashMap<PhysicalASAPNodeId, u64> = HashMap::new();
        for (view, scans) in views.iter().zip(&scans_read) {
            if view.refresh == MaterializedViewRefresh::OnFirstRead {
                for scan in scans {
                    let need_ms = view_need_ms_by_scan.entry(*scan).or_insert(0);
                    *need_ms = (*need_ms).max(view.retention_ms);
                }
            }
        }
        let scan_windows = self.query_time_scan_windows();
        for node in self.nodes.iter().filter(|node| is_query_time_scan(node)) {
            check_deadline(deadline)?;
            let window_ms = scan_windows[&node.id].lookback_ms();
            let retention_ms = window_ms.map(|window_ms| {
                let span_ms = reader_span_ms_by_node.get(&node.id).copied().unwrap_or(0);
                let views_ms = view_need_ms_by_scan.get(&node.id).copied().unwrap_or(0);
                (window_ms + span_ms).max(views_ms)
            });
            retention_ms_by_node.insert(node.id, retention_ms);
        }
        for node in &mut self.nodes {
            if let Some(&retention_ms) = retention_ms_by_node.get(&node.id) {
                node.retention_ms = retention_ms;
            }
        }
        Ok(views)
    }

    /// For each raw-data source some query-time `Scan` reads, in the order
    /// of its first such `Scan`, the longest `retention_ms` among those
    /// scans, `None` when one reads it unbounded: the raw data a runtime
    /// keeps for the DAG's queries, the raw-data counterpart of each view's
    /// retention. A source read only at ingestion time is absent. Checks
    /// that each scan reading a bounded window ([`Self::scan_window`]) has a
    /// retention covering it.
    pub fn raw_data_retention(&self) -> Result<Vec<RawDataRetention>, InstallableDagError> {
        let scan_windows = self.query_time_scan_windows();
        let mut retention: Vec<RawDataRetention> = Vec::new();
        for node in self.nodes.iter().filter(|node| is_query_time_scan(node)) {
            let Operator::NonASAP(NonASAPOp::Scan { source, .. }) = &node.payload else {
                continue;
            };
            let window_ms = scan_windows[&node.id].lookback_ms();
            let lookback_ms = match (window_ms, node.retention_ms) {
                (None, _) => None,
                (Some(window_ms), Some(kept_ms)) if kept_ms >= window_ms => Some(kept_ms),
                (Some(_), Some(_)) => {
                    return Err(InstallableDagError::ScanRetentionTooShort { node_id: node.id })
                }
                (Some(_), None) => {
                    return Err(InstallableDagError::ScanWithoutRetention { node_id: node.id })
                }
            };
            match retention.iter_mut().find(|kept| &kept.source == source) {
                Some(kept) => {
                    kept.lookback_ms = kept.lookback_ms.zip(lookback_ms).map(|(a, b)| a.max(b))
                }
                None => retention.push(RawDataRetention {
                    source: source.clone(),
                    lookback_ms,
                }),
            }
        }
        Ok(retention)
    }

    /// [`Self::scan_window`] of each query-time `Scan`, with no chain kept,
    /// by id.
    fn query_time_scan_windows(&self) -> HashMap<PhysicalASAPNodeId, ScanWindow> {
        let scans = self
            .nodes
            .iter()
            .filter(|node| is_query_time_scan(node))
            .map(|node| node.id);
        self.scan_windows(scans, &HashSet::new())
    }

    /// Checks that every raw-data window the DAG reads (each `TimeRange`,
    /// and each `PaneChain`'s pane, reading a `Scan` without passing another
    /// `TimeRange`) has a supported shape
    /// ([`InstallableDagError::UnsupportedRawDataWindow`]). O(nodes + edges)
    /// plus the depth below each window.
    pub fn check_raw_data_windows(&self) -> Result<(), InstallableDagError> {
        ViewIndex::new(self).check_raw_data_windows()
    }

    /// The views, each with the node's `retention_ms` (0 when unset) and the
    /// `Scan`s its refresh reads through its raw-data windows.
    fn views_with_node_retention(
        &self,
        deadline: Option<Instant>,
    ) -> Result<Vec<(MaterializedView, Vec<PhysicalASAPNodeId>)>, InstallableDagError> {
        check_deadline(deadline)?;
        let index = ViewIndex::new(self);
        index.check_raw_data_windows()?;
        let views: HashSet<PhysicalASAPNodeId> = self
            .nodes
            .iter()
            .filter(|node| node.materialization.is_materialized_view())
            .map(|node| node.id)
            .collect();
        // The IR of each node, which coverage is derived from.
        let ir = if views.is_empty() {
            HashMap::new()
        } else {
            self.operator_nodes()?
        };
        let mut derived = Vec::new();
        let mut coverage_by_node: HashMap<PhysicalASAPNodeId, SummaryCoverage> = HashMap::new();
        // The window each view summarizes ([`PhysicalASAPDAG::view_window`]):
        // its raw-data windows and those of the views it reads, unioned.
        let mut window_by_view: HashMap<PhysicalASAPNodeId, ScanWindow> = HashMap::new();
        for &id in index.children_first.iter().filter(|id| views.contains(id)) {
            check_deadline(deadline)?;
            let node = index.nodes[&id];
            let (inputs, scans) = index.inputs_of_view(id, &views)?;
            let read_windows: Vec<ScanWindow> = inputs
                .materialized_views
                .iter()
                .filter_map(|read| window_by_view.get(read).copied())
                .collect();
            if let Some(window) = inputs
                .raw_data_windows
                .iter()
                .map(|window| ScanWindow::Within {
                    end_offset_ms: window.end_offset_ms,
                    width_ms: window.width_ms,
                })
                .chain(read_windows.iter().copied())
                .reduce(ScanWindow::union)
            {
                window_by_view.insert(id, window);
            }
            if matches!(node.materialization, Materialization::MatAtIngestion(_)) {
                // An ingestion run reads each view for the window ending at
                // its end, so a view read must end where the raw windows do.
                let mut end_offsets_ms: Vec<i64> = inputs
                    .raw_data_windows
                    .iter()
                    .map(|window| window.end_offset_ms)
                    .chain(read_windows.iter().filter_map(|window| match window {
                        ScanWindow::Within { end_offset_ms, .. } => Some(*end_offset_ms),
                        ScanWindow::Unbounded => None,
                    }))
                    .collect();
                end_offsets_ms.sort_unstable();
                end_offsets_ms.dedup();
                if end_offsets_ms.len() > 1 {
                    return Err(
                        InstallableDagError::IngestionBuildReadsWindowsOfDifferentOffsets {
                            node_id: id,
                            end_offsets_ms,
                        },
                    );
                }
            }
            let coverage = coverage_of(id, &inputs, &ir, &coverage_by_node).map_err(|error| {
                InstallableDagError::ViewWithoutCoverage {
                    node_id: id,
                    reason: error.to_string(),
                }
            })?;
            coverage_by_node.insert(id, coverage.clone());
            let view = MaterializedView {
                node_id: id,
                refresh: match node.materialization {
                    Materialization::MatAtIngestion(_) => MaterializedViewRefresh::AtIngestion,
                    _ => MaterializedViewRefresh::OnFirstRead,
                },
                coverage: ViewCoverage {
                    definition_id: coverage.definition_id(),
                    selection: coverage.selection,
                },
                inputs,
                retention_ms: node.retention_ms.unwrap_or(0),
            };
            derived.push((view, scans));
        }
        Ok(derived)
    }
}

/// How far back from the evaluation time the data a view refreshed from
/// `inputs` summarizes reaches: the farthest of its raw-data windows and of
/// the views it reads (`reach_ms_by_node`, each already computed).
/// Whether `node` is a `Scan` run at query time, which carries how long its
/// source's rows are kept for it.
pub(crate) fn is_query_time_scan(node: &PhysicalASAPDAGNode) -> bool {
    matches!(node.payload, Operator::NonASAP(NonASAPOp::Scan { .. }))
        && node.output_state.timing == ExecutionTiming::QueryTime
}

fn reach_ms_of(
    inputs: &MaterializedViewInputs,
    reach_ms_by_node: &HashMap<PhysicalASAPNodeId, u64>,
) -> u64 {
    let windows = inputs
        .raw_data_windows
        .iter()
        .map(RawDataWindow::lookback_ms);
    let views = inputs
        .materialized_views
        .iter()
        .map(|id| reach_ms_by_node.get(id).copied().unwrap_or(0));
    windows.chain(views).max().unwrap_or(0)
}

/// The nodes of a DAG by id, each node's inputs in edge order, and two
/// facts every raw-data window check reads, computed in one pass each over
/// the DAG (O(nodes + edges)). `scan_window` counts the windows of a `Scan`
/// for pricing; this walks down from each window to check its shape.
struct ViewIndex<'a> {
    dag: &'a PhysicalASAPDAG,
    nodes: HashMap<PhysicalASAPNodeId, &'a PhysicalASAPDAGNode>,
    inputs: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>>,
    /// Every node after its inputs, from the DAG's node order.
    children_first: Vec<PhysicalASAPNodeId>,
    /// The nodes whose rows come from raw data: a `Scan` is reachable below
    /// them without passing a `TimeRange` or `PaneChain`.
    reading_raw_data: HashSet<PhysicalASAPNodeId>,
    /// The nodes with a `TimeShift`, `TimeRange` or `PromqlSubquery` above
    /// them, which evaluates them at other times than the evaluation time.
    re_timed_from_above: HashSet<PhysicalASAPNodeId>,
}

/// Whether `operator` evaluates its input at other times than its own.
fn re_times_its_input(operator: &Operator<PhysicalASAPNodeId>) -> bool {
    matches!(
        operator,
        Operator::NonASAP(
            NonASAPOp::TimeShift { .. }
                | NonASAPOp::TimeRange { .. }
                | NonASAPOp::PromqlSubquery { .. }
        )
    )
}

impl<'a> ViewIndex<'a> {
    fn new(dag: &'a PhysicalASAPDAG) -> Self {
        let nodes: HashMap<_, _> = dag.nodes.iter().map(|n| (n.id, n)).collect();
        let mut inputs: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>> = HashMap::new();
        for edge in &dag.edges {
            inputs.entry(edge.consumer).or_default().push(edge.producer);
        }
        // Every node after its inputs: an iterative depth-first postorder.
        let mut children_first = Vec::with_capacity(dag.nodes.len());
        let mut visited = HashSet::new();
        for node in &dag.nodes {
            let mut pending = vec![(node.id, false)];
            while let Some((id, inputs_done)) = pending.pop() {
                if inputs_done {
                    children_first.push(id);
                } else if visited.insert(id) {
                    pending.push((id, true));
                    for &input in inputs.get(&id).into_iter().flatten() {
                        pending.push((input, false));
                    }
                }
            }
        }
        let payload = |id: &PhysicalASAPNodeId| nodes.get(id).map(|node| &node.payload);
        let mut reading_raw_data = HashSet::new();
        for id in &children_first {
            let reads = inputs
                .get(id)
                .into_iter()
                .flatten()
                .any(|input| match payload(input) {
                    Some(Operator::NonASAP(NonASAPOp::Scan { .. })) => true,
                    Some(
                        Operator::NonASAP(NonASAPOp::TimeRange { .. })
                        | Operator::ASAP(ASAPOp::PaneChain { .. }),
                    ) => false,
                    _ => reading_raw_data.contains(input),
                });
            if reads {
                reading_raw_data.insert(*id);
            }
        }
        let mut re_timed_from_above = HashSet::new();
        for id in children_first.iter().rev() {
            if re_timed_from_above.contains(id) || payload(id).is_some_and(re_times_its_input) {
                re_timed_from_above.extend(inputs.get(id).into_iter().flatten().copied());
            }
        }
        ViewIndex {
            dag,
            nodes,
            inputs,
            children_first,
            reading_raw_data,
            re_timed_from_above,
        }
    }

    fn relational(&self, id: PhysicalASAPNodeId) -> Option<&'a NonASAPOp<PhysicalASAPNodeId>> {
        match &self.nodes.get(&id)?.payload {
            Operator::NonASAP(op) => Some(op),
            Operator::ASAP(_) => None,
        }
    }

    fn only_input(&self, id: PhysicalASAPNodeId) -> Option<PhysicalASAPNodeId> {
        match self.inputs.get(&id).map(Vec::as_slice) {
            Some([input]) => Some(*input),
            _ => None,
        }
    }

    /// See [`PhysicalASAPDAG::check_raw_data_windows`].
    fn check_raw_data_windows(&self) -> Result<(), InstallableDagError> {
        for node in &self.dag.nodes {
            if !self.reading_raw_data.contains(&node.id) {
                continue;
            }
            match &node.payload {
                Operator::NonASAP(NonASAPOp::TimeRange { .. }) => {
                    self.window_at(node.id)?;
                }
                Operator::ASAP(ASAPOp::PaneChain { pane_width_ms, .. }) => {
                    self.raw_window_below(node.id, u128::from(*pane_width_ms))?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The raw-data window read through the `TimeRange` `time_range` and
    /// the `Scan` it reads, or why its shape is unsupported.
    fn window_at(
        &self,
        time_range: PhysicalASAPNodeId,
    ) -> Result<(RawDataWindow, PhysicalASAPNodeId), InstallableDagError> {
        let Some(NonASAPOp::TimeRange { range, .. }) = self.relational(time_range) else {
            return Err(InstallableDagError::UnsupportedRawDataWindow {
                time_range_node_id: time_range,
                reason: "not a TimeRange",
            });
        };
        self.raw_window_below(time_range, range.as_millis())
    }

    /// The window of width `width_ms` that `time_range` (a `TimeRange`, or
    /// a `PaneChain`'s pane) reads from the `Scan` below it, and that
    /// `Scan`, or why its shape is unsupported. It does not allocate or
    /// compile every pane in a chain.
    fn raw_window_below(
        &self,
        time_range: PhysicalASAPNodeId,
        width_ms: u128,
    ) -> Result<(RawDataWindow, PhysicalASAPNodeId), InstallableDagError> {
        let unsupported = |reason| InstallableDagError::UnsupportedRawDataWindow {
            time_range_node_id: time_range,
            reason,
        };
        if self.re_timed_from_above.contains(&time_range) {
            return Err(unsupported("a subquery or a TimeShift above the range"));
        }
        let width_ms =
            i64::try_from(width_ms).map_err(|_| unsupported("window width overflows"))?;
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
                Some(NonASAPOp::Scan { source, .. }) => {
                    let window = RawDataWindow {
                        source: source.clone(),
                        end_offset_ms,
                        width_ms,
                    };
                    return Ok((window, current));
                }
                _ => {
                    return Err(unsupported(
                        "an operator other than TimeShift below the range",
                    ))
                }
            }
        }
    }

    /// What view `node`'s refresh reads: below it, each view (`views`) and
    /// each raw-data `TimeRange` it reaches without passing another of
    /// them, in node order, and the `Scan` below each `TimeRange`.
    fn inputs_of_view(
        &self,
        node: PhysicalASAPNodeId,
        views: &HashSet<PhysicalASAPNodeId>,
    ) -> Result<(MaterializedViewInputs, Vec<PhysicalASAPNodeId>), InstallableDagError> {
        let mut raw_ranges = Vec::new();
        let mut view_inputs = Vec::new();
        let mut seen = HashSet::new();
        let mut pending: Vec<PhysicalASAPNodeId> = self
            .inputs
            .get(&node)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            if views.contains(&id) {
                view_inputs.push(id);
                continue;
            }
            match self.relational(id) {
                Some(NonASAPOp::TimeRange { .. }) if self.reading_raw_data.contains(&id) => {
                    raw_ranges.push(id);
                    continue;
                }
                Some(NonASAPOp::Scan { .. }) => {
                    return Err(InstallableDagError::ViewReadsUnwindowedSource { node_id: node })
                }
                _ => pending.extend(self.inputs.get(&id).into_iter().flatten().copied()),
            }
        }
        raw_ranges.sort_unstable();
        view_inputs.sort_unstable();
        let (raw_data_windows, scans) = raw_ranges
            .into_iter()
            .map(|range| self.window_at(range))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .unzip();
        let inputs = MaterializedViewInputs {
            raw_data_windows,
            materialized_views: view_inputs,
        };
        Ok((inputs, scans))
    }
}

/// The coverage of view `id` (IR `ir`), refreshed from `inputs`. A merge's
/// is the union of its inputs' selections under their shared definition
/// ([`SummaryCoverage::derive`]). A summary built from exactly one view and
/// nothing else, with no selection of its own (a rollup to a coarser
/// grouping, or across series), takes that input's selection: its
/// definition is its own sub-DAG with the input replaced by the input's
/// definition, so the rollups of equal inputs share one definition.
/// Anything else keeps its own derived coverage.
fn coverage_of(
    id: PhysicalASAPNodeId,
    inputs: &MaterializedViewInputs,
    ir: &HashMap<PhysicalASAPNodeId, Rc<OperatorNode>>,
    coverage_by_node: &HashMap<PhysicalASAPNodeId, SummaryCoverage>,
) -> Result<SummaryCoverage, CoverageError> {
    let node = &ir[&id];
    let own = SummaryCoverage::derive(node)?;
    let [input] = inputs.materialized_views.as_slice() else {
        return Ok(own);
    };
    if !inputs.raw_data_windows.is_empty()
        || matches!(node.asap(), Some(ASAPOp::SummaryMerge { .. }))
        || own.selection != [SelectionBox::default()]
    {
        return Ok(own);
    }
    let input_coverage = &coverage_by_node[input];
    let over_definition = with_replaced(node, &ir[input], &input_coverage.definition);
    Ok(SummaryCoverage {
        definition: SummaryCoverage::derive(&over_definition)?.definition,
        selection: input_coverage.selection.clone(),
    })
}

/// `node` with every use of `old` (by address) replaced by `new`; nodes
/// that do not reach `old` are shared, not copied.
fn with_replaced(
    node: &Rc<OperatorNode>,
    old: &Rc<OperatorNode>,
    new: &Rc<OperatorNode>,
) -> Rc<OperatorNode> {
    fn replace(
        node: &Rc<OperatorNode>,
        old: &Rc<OperatorNode>,
        new: &Rc<OperatorNode>,
        cache: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
    ) -> Rc<OperatorNode> {
        if Rc::ptr_eq(node, old) {
            return Rc::clone(new);
        }
        if let Some(done) = cache.get(&Rc::as_ptr(node)) {
            return Rc::clone(done);
        }
        let children = node.children();
        let replaced: Vec<Rc<OperatorNode>> = children
            .iter()
            .map(|child| replace(child, old, new, cache))
            .collect();
        let result = if children
            .iter()
            .zip(&replaced)
            .all(|(a, b)| Rc::ptr_eq(a, b))
        {
            Rc::clone(node)
        } else {
            let mut next = replaced.into_iter();
            let mut copy = (**node).clone();
            copy.operator = node
                .operator
                .map_children(|_| next.next().expect("one per child"));
            Rc::new(copy)
        };
        cache.insert(Rc::as_ptr(node), Rc::clone(&result));
        result
    }
    replace(node, old, new, &mut HashMap::new())
}
