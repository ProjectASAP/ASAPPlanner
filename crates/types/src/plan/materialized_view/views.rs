//! The materialized views of a DAG and their refresh groups and windows
//! ([`PhysicalASAPDAG::materialized_views`],
//! [`PhysicalASAPDAG::ingestion_refresh_groups`],
//! [`PhysicalASAPDAG::refresh_window`]), derived over one index of the DAG
//! ([`ViewIndex`]).

use std::collections::{HashMap, HashSet};

use super::{
    is_raw_data_scan, IngestionRefreshGroup, InstallableDagError, MaterializedView,
    MaterializedViewInputs, MaterializedViewRefresh, RefreshWindow,
};
use crate::ir::operator::asap::ASAPOp;
use crate::ir::operator::node::Operator;
use crate::ir::operator::non_asap::NonASAPOp;
use crate::ir::properties::execution_data_state::ExecutionTiming;
use crate::plan::physical_asap_dag::{PhysicalASAPDAG, PhysicalASAPDAGNode, PhysicalASAPNodeId};
use crate::plan::time_window::{self, TimeIndexedRead, TimeWindow};

impl PhysicalASAPDAG {
    /// Every materialized view, in dependency order (each after the views
    /// it reads), with its refresh mode, coverage, inputs and the node's
    /// `retention_ms`. Validates the DAG and checks that every raw-data
    /// window it reads has a supported shape and that each view's retention
    /// covers what the `TimeRange`s and `TimeShift`s above it read back.
    /// O(views · nodes) for the inputs, plus O(nodes + edges).
    pub fn materialized_views(&self) -> Result<Vec<MaterializedView>, InstallableDagError> {
        if let Some(node) = self
            .nodes
            .iter()
            .find(|node| node.is_view() && node.retention_ms.is_none())
        {
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
}

/// The nodes of a DAG by id, each node's inputs and consumers in edge
/// order, and two facts every raw-data window check reads, computed in one
/// pass each over the DAG (O(nodes + edges)); it walks down from each
/// window to check its shape.
pub(super) struct ViewIndex<'a> {
    pub(super) dag: &'a PhysicalASAPDAG,
    pub(super) nodes: HashMap<PhysicalASAPNodeId, &'a PhysicalASAPDAGNode>,
    pub(super) inputs: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>>,
    pub(super) consumers: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>>,
    /// Every node after its inputs, from the DAG's node order.
    pub(super) children_first: Vec<PhysicalASAPNodeId>,
    /// The nodes whose rows come from raw data: a `Scan` is reachable below
    /// them without passing a `TimeRange` or `SummaryTumblingWindowChain`.
    pub(super) reading_raw_data: HashSet<PhysicalASAPNodeId>,
    /// The nodes with a `TimeShift`, `TimeRange` or `PromqlSubquery` above
    /// them, which evaluates them at other times than the evaluation time.
    pub(super) re_timed_from_above: HashSet<PhysicalASAPNodeId>,
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
    pub(super) fn new(dag: &'a PhysicalASAPDAG) -> Self {
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

    pub(super) fn relational(
        &self,
        id: PhysicalASAPNodeId,
    ) -> Option<&'a NonASAPOp<PhysicalASAPNodeId>> {
        match &self.nodes.get(&id)?.payload {
            Operator::NonASAP(op) => Some(op),
            Operator::ASAP(_) => None,
        }
    }

    pub(super) fn only_input(&self, id: PhysicalASAPNodeId) -> Option<PhysicalASAPNodeId> {
        match self.inputs.get(&id).map(Vec::as_slice) {
            Some([input]) => Some(*input),
            _ => None,
        }
    }

    /// Every materialized view, in dependency order, with the node's
    /// `retention_ms` (0 when unset), after checking the DAG's raw-data
    /// windows.
    pub(super) fn views(&self) -> Result<Vec<MaterializedView>, InstallableDagError> {
        self.check_raw_data_windows()?;
        let views: HashSet<PhysicalASAPNodeId> = self
            .dag
            .nodes
            .iter()
            .filter(|node| node.is_view())
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
    pub(super) fn read_back_ms(&self, node: PhysicalASAPNodeId) -> Option<u64> {
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
                self.nodes[&id].lifetime,
            ) {
                // A root's output is read at evaluation time, so a root
                // read through is read whole. A materialized shift that is a
                // root still reads its input's newest output: its readers do
                // not change that, so it is not mapped to `Whole` (the
                // executor refuses its refresh over a chain for now).
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
