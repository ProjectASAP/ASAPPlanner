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

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::physical_asap_dag::{
    PhysicalASAPDAG, PhysicalASAPDAGNode, PhysicalASAPDAGValidationError, PhysicalASAPNodeId,
    ViewCoverage,
};
use super::time_window::{self, TimeIndexedRead, TimeWindow};
use crate::ir::operator::arguments::Source;
use crate::ir::operator::asap::ASAPOp;
use crate::ir::operator::node::Operator;
use crate::ir::operator::non_asap::NonASAPOp;
use crate::ir::properties::execution_data_state::ExecutionTiming;
use crate::stages::Lifetime;

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

impl PhysicalASAPDAG {
    /// Every materialized view, in dependency order (each after the views
    /// it reads), with its refresh mode, coverage, inputs and the node's
    /// `retention_ms`. Validates the DAG and checks that every raw-data
    /// window it reads has a supported shape and that each view's retention
    /// covers what the `TimeRange`s and `TimeShift`s above it read back.
    /// O(views · nodes) for the inputs, plus O(nodes + edges).
    pub fn materialized_views(&self) -> Result<Vec<MaterializedView>, InstallableDagError> {
        if let Some(node) = self.nodes.iter().find(|node| {
            matches!(node.lifetime, Lifetime::AcrossEvaluations(_))
                && !is_raw_data_scan(node)
                && node.retention_ms.is_none()
        }) {
            return Err(InstallableDagError::ViewWithoutRetention { node_id: node.id });
        }
        self.validate()?;
        let index = ViewIndex::new(self);
        let views = index.views()?;
        for view in &views {
            // A view read at a time not relative to the evaluation (`@`) is
            // read unbounded, which no retention covers; it is not checked.
            if index
                .read_back_ms(view.node_id)
                .is_some_and(|read_back_ms| view.retention_ms < read_back_ms)
            {
                return Err(InstallableDagError::ViewRetentionTooShort {
                    node_id: view.node_id,
                });
            }
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
    /// `SummaryTumblingWindowChain` of width `w`, `(w, s mod w)` with `s`
    /// the sum of the offsets of the `TimeShift`s below it; for any other
    /// node, that of the smallest window holding the windows below it (each
    /// `TimeRange`'s, and each chain's reached without passing one). `None`
    /// when that window is unbounded (none below it, or an `@` anchor below
    /// a `TimeRange`) or empty. O(nodes + edges).
    pub fn refresh_window(&self, node: PhysicalASAPNodeId) -> Option<RefreshWindow> {
        self.refresh_windows([node])[&node]
    }

    /// [`Self::refresh_window`] of each of `nodes`, by id, with the DAG
    /// indexed once for all of them.
    pub fn refresh_windows(
        &self,
        nodes: impl IntoIterator<Item = PhysicalASAPNodeId>,
    ) -> HashMap<PhysicalASAPNodeId, Option<RefreshWindow>> {
        let index = ViewIndex::new(self);
        nodes
            .into_iter()
            .map(|node| {
                let refresh_window = index
                    .windows_below(node)
                    .and_then(|windows| windows.into_iter().reduce(TimeWindow::union))
                    .and_then(RefreshWindow::of_window);
                (node, refresh_window)
            })
            .collect()
    }

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
}

/// The nodes of a DAG by id, each node's inputs and consumers in edge
/// order, and two facts every raw-data window check reads, computed in one
/// pass each over the DAG (O(nodes + edges)); it walks down from each
/// window to check its shape.
struct ViewIndex<'a> {
    dag: &'a PhysicalASAPDAG,
    nodes: HashMap<PhysicalASAPNodeId, &'a PhysicalASAPDAGNode>,
    inputs: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>>,
    consumers: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>>,
    /// Every node after its inputs, from the DAG's node order.
    children_first: Vec<PhysicalASAPNodeId>,
    /// The nodes whose rows come from raw data: a `Scan` is reachable below
    /// them without passing a `TimeRange` or `SummaryTumblingWindowChain`.
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
        let mut consumers: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>> = HashMap::new();
        for edge in &dag.edges {
            inputs.entry(edge.consumer).or_default().push(edge.producer);
            consumers
                .entry(edge.producer)
                .or_default()
                .push(edge.consumer);
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
                        | Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. }),
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
            consumers,
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

    /// Every materialized view, in dependency order, with the node's
    /// `retention_ms` (0 when unset), after checking the DAG's raw-data
    /// windows.
    fn views(&self) -> Result<Vec<MaterializedView>, InstallableDagError> {
        self.check_raw_data_windows()?;
        let views: HashSet<PhysicalASAPNodeId> = self
            .dag
            .nodes
            .iter()
            .filter(|node| {
                matches!(node.lifetime, Lifetime::AcrossEvaluations(_)) && !is_raw_data_scan(node)
            })
            .map(|node| node.id)
            .collect();
        let mut derived = Vec::new();
        for &id in self.children_first.iter().filter(|id| views.contains(id)) {
            let node = self.nodes[&id];
            let inputs = self.inputs_of_view(id, &views)?;
            let refreshed_at_ingestion = node.output_state.timing == ExecutionTiming::IngestionTime;
            if refreshed_at_ingestion {
                // An ingestion run builds the windows ending at its end, so
                // every window the view reads must end at one offset.
                let mut end_offsets_ms: Vec<u64> = self
                    .windows_below(id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|window| window.end_offset_ms)
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
            let coverage = node.view_coverage.clone().ok_or_else(|| {
                InstallableDagError::ViewWithoutCoverage {
                    node_id: id,
                    reason: "the plan node carries no view_coverage".into(),
                }
            })?;
            derived.push(MaterializedView {
                node_id: id,
                refresh: if refreshed_at_ingestion {
                    MaterializedViewRefresh::AtIngestion
                } else {
                    MaterializedViewRefresh::OnFirstRead
                },
                coverage,
                inputs,
                retention_ms: node.retention_ms.unwrap_or(0),
            });
        }
        Ok(derived)
    }

    /// The windows back from the evaluation time that `node` reads
    /// ([`time_window::windows_below`]): a chain its own tumbling window;
    /// any other node each `TimeRange` below it and each chain below it
    /// reached without passing a `TimeRange`. `None` when one is read
    /// through an `@` anchor or a negative offset.
    fn windows_below(&self, node: PhysicalASAPNodeId) -> Option<Vec<TimeWindow>> {
        time_window::windows_below(
            node,
            |id| self.inputs.get(&id).into_iter().flatten().copied(),
            |id| self.only_input(id),
            |id| &self.nodes[&id].payload,
        )
    }

    /// How far back from the evaluation time the readers of `node` read it
    /// ([`time_window::windows_read_above`]): the most any reads back.
    /// `None`, unbounded, when a root or a reader at a time not relative
    /// to the evaluation (`@`) reads it, or, for a raw-data `Scan`, a reader
    /// other than a window (a `TimeRange` or an ingestion-time chain).
    ///
    /// Example: a `Scan` read by `TimeShift(1m) -> TimeRange(5m)`: 360,000.
    fn read_back_ms(&self, node: PhysicalASAPNodeId) -> Option<u64> {
        let root = |id: PhysicalASAPNodeId| self.dag.roots.contains(&id);
        if root(node) {
            return None;
        }
        let windows = time_window::windows_read_above(
            node,
            |id| self.consumers.get(&id).into_iter().flatten().copied(),
            |id| match TimeIndexedRead::of(
                &self.nodes[&id].payload,
                self.nodes[&id].output_state.timing,
            ) {
                TimeIndexedRead::Through { .. } if root(id) => TimeIndexedRead::Whole,
                read => read,
            },
            is_raw_data_scan(self.nodes[&node]),
        )?;
        Some(
            windows
                .iter()
                .map(|(_, window)| window.read_back_ms())
                .max()
                .unwrap_or(0),
        )
    }

    /// See [`PhysicalASAPDAG::check_raw_data_windows`].
    fn check_raw_data_windows(&self) -> Result<(), InstallableDagError> {
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
    fn check_raw_window_at(
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
    fn check_raw_window_below(
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

    /// What view `node`'s refresh reads: below it, each view (`views`) it
    /// reaches without passing another, in node order; a chain reading raw
    /// data none. Checks the shape of each raw-data window it reads (its
    /// tumbling window over raw data, or each raw-data `TimeRange` it
    /// reaches without passing a view), and that it reads no `Scan` outside
    /// one.
    fn inputs_of_view(
        &self,
        node: PhysicalASAPNodeId,
        views: &HashSet<PhysicalASAPNodeId>,
    ) -> Result<MaterializedViewInputs, InstallableDagError> {
        if let Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. }) =
            &self.nodes[&node].payload
        {
            if self.reading_raw_data.contains(&node) {
                self.check_raw_window_below(node)?;
                return Ok(MaterializedViewInputs {
                    materialized_views: Vec::new(),
                });
            }
        }
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
        for range in raw_ranges {
            self.check_raw_window_at(range)?;
        }
        Ok(MaterializedViewInputs {
            materialized_views: view_inputs,
        })
    }
}
