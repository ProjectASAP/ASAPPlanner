//! The windows an in-memory ingestion-time view keeps resident under the
//! data plane's window cache, per read the selection may choose
//! (`docs/design_docs/proposals/cost-terms-from-calibration.md`, section
//! 1): `plan_cost` charges a view the most any one of its readers keeps
//! resident ([`asap_cost::plan::resident_windows::view_resident_windows`]);
//! here each read of one of its executions (an edge out of it, or a query
//! output it answers) carries the windows its readers keep, so the most
//! over the selected reads is what `plan_cost` charges the plan.

use std::collections::HashMap;

use asap_cost::plan::resident_windows::{
    reader_resident_windows, written_resident_windows, ChainReader, ReadTimes, WindowRead,
};
use asap_cost::{AnswerCache, CachedViewWindows, ViewWindowCache};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::{NonASAPOp, Operator};
use asap_types::plan::materialized_view::RefreshWindow;
use asap_types::stages::{ExecutionContext, PhysicalASAPMemoCandidates, PhysicalCandidateId};
use asap_types::workload::QueryWorkloadEntry;

use crate::retention::edge_readers_need;
use crate::selectable::{Reader, SelectableExecutions};

/// One read of a cached view's execution and the windows it keeps
/// resident, mean and most.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ResidentRead {
    /// The execution read, by position in
    /// `SelectableExecutions::executions`.
    pub(crate) execution: usize,
    pub(crate) reader: Reader,
    pub(crate) mean_windows: f64,
    pub(crate) most_windows: u64,
}

/// Each read of an execution of a view in `cached` (by candidate) under a
/// window `cache` (a query's steps as `answer_cache` has the data plane
/// compute them), with the windows it keeps resident: the windows
/// written within the idle time of those the read needs retained
/// (`retained_ms_of_read`, its need as $h_v$ retains it), and for an edge,
/// the most any reader above it keeps
/// ([`reader_resident_windows`], each query reaching a query-time reader on
/// the edge consumer's grid one reader, as `plan_cost` takes each query
/// reaching it). A read the cost model does not follow keeps every window
/// it needs retained.
pub(crate) fn resident_reads(
    physical: &PhysicalASAPMemoCandidates,
    selectable: &SelectableExecutions,
    entries: &[QueryWorkloadEntry],
    (cache, answer_cache): (ViewWindowCache, Option<AnswerCache>),
    cached: &HashMap<PhysicalCandidateId, CachedViewWindows>,
    need_ms_per_edge: &[Option<u64>],
    retained_ms_of_read: impl Fn(PhysicalCandidateId, u64) -> u64,
) -> Vec<ResidentRead> {
    let idle_ms = cache.idle_ms;
    let executions = selectable.executions();
    let memo = &physical.logical_candidates.memo;
    let width_of = |v: PhysicalCandidateId| RefreshWindow::computed_from(physical, v);
    let mut of_reader: HashMap<(u64, ChainReader), Option<(f64, u64)>> = HashMap::new();
    let mut reads = Vec::new();
    for (index, reader, need_ms) in selectable.reads_back(need_ms_per_edge) {
        let v = executions[index].candidate;
        let (Some(windows), Some(need_ms)) = (cached.get(&v), need_ms) else {
            continue;
        };
        let width_ms = windows.width_ms.max(1);
        let retained = retained_ms_of_read(v, need_ms).div_ceil(width_ms).max(1);
        let (mut mean, mut most) = written_resident_windows(width_ms, retained, idle_ms);
        if let Reader::Edge(position) = reader {
            let edge = selectable.edges()[position];
            let physical_edge = physical.edge(edge.edge);
            let grid = match selectable.context(executions[edge.consumer].context) {
                ExecutionContext::Evaluation { grid } => Some(grid),
                _ => None,
            };
            let chain_readers: Vec<ChainReader> = match edge_readers_need(physical, edge.edge) {
                None => vec![ChainReader {
                    read: WindowRead::Every,
                    times: ReadTimes::Refresh { width_ms },
                }],
                Some(windows_read) => windows_read
                    .into_iter()
                    .flat_map(|(above, window)| {
                        let read = match physical.operator(above) {
                            Operator::NonASAP(NonASAPOp::TimeRangeCover { .. })
                                if above == physical_edge.consumer_candidate_id =>
                            {
                                let level_widths_ms: Option<Vec<u64>> = (0..physical
                                    .input_count(above))
                                    .map(|i| {
                                        physical.admissible_edges_into(above, i).iter().find_map(
                                            |&level| {
                                                width_of(physical.edge(level).producer_candidate_id)
                                                    .map(|window| window.width_ms)
                                            },
                                        )
                                    })
                                    .collect();
                                match level_widths_ms {
                                    Some(level_widths_ms) => WindowRead::Cover {
                                        range_ms: window.length_ms,
                                        level_widths_ms,
                                        level: physical_edge.input_index_of_consumer,
                                    },
                                    None => WindowRead::Every,
                                }
                            }
                            Operator::NonASAP(NonASAPOp::TimeRangeCover { .. }) => {
                                WindowRead::Every
                            }
                            _ => WindowRead::Range {
                                range_ms: window.length_ms,
                                offset_ms: window.ends_before_evaluation_ms,
                            },
                        };
                        let properties = physical.physical_candidates[above.0].physical_properties;
                        let times: Vec<ReadTimes> = match properties.timing() {
                            ExecutionTiming::IngestionTime => vec![ReadTimes::Refresh {
                                width_ms: width_of(above)
                                    .map_or(width_ms, |window| window.width_ms),
                            }],
                            ExecutionTiming::QueryTime => {
                                let group = physical.physical_candidates[above.0]
                                    .logical_candidate_id
                                    .group;
                                memo.groups()[group.0 as usize]
                                    .reached_by_queries()
                                    .iter()
                                    .filter_map(|&query| entries.get(query))
                                    .filter(|entry| {
                                        grid.is_none_or(|grid| entry.evaluation_grid() == grid)
                                    })
                                    .map(|entry| ReadTimes::of_query(entry, answer_cache))
                                    .collect()
                            }
                        };
                        times.into_iter().map(move |times| ChainReader {
                            read: read.clone(),
                            times,
                        })
                    })
                    .collect(),
            };
            for chain_reader in chain_readers {
                let resident = *of_reader
                    .entry((width_ms, chain_reader))
                    .or_insert_with_key(|(width_ms, chain_reader)| {
                        reader_resident_windows(*width_ms, idle_ms, chain_reader)
                    });
                let (reader_mean, reader_most) = resident.unwrap_or((retained as f64, retained));
                mean = mean.max(reader_mean);
                most = most.max(reader_most);
            }
        }
        reads.push(ResidentRead {
            execution: index,
            reader,
            mean_windows: mean,
            most_windows: most,
        });
    }
    reads
}
