//! A `SummaryMerge` over windows that list the same groups in the same row
//! order, as the windows of one tumbling-window chain do: row `r` of every
//! window merges into output group `r`, so no row's group labels are
//! hashed, no input batch is copied, and each sketch merges into its
//! group's state in place.
//!
//! Example: `sum by (job) (rate(m[1h]))` evaluated every 60 s over a chain
//! of 1-minute windows of 1,000 series reads 60 windows of 1,000 rows each,
//! every one in the series' key order. Row 17 of each window is the same
//! series, so its 60 counter states merge into output row 17. A window in
//! which a series appeared or ended lists other rows; it merges through the
//! group table, as the hash merge does, and the answer is the same.
//!
//! Contract: the output batch is the one the hash merge (`merge_summary`)
//! returns for the same input, bit for bit for exact states, and with the
//! same estimates for sketches (KLL past its first compaction: within its
//! rank-error bound, as its compaction coin is random).
use super::*;

/// How one input window's rows find their output groups.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // Called from `merge_summary` by the implementation.
pub(super) enum WindowMerge {
    /// Row `r` holds the group of the first window's row `r`, and the first
    /// window holds each group once: row `r` merges into output group `r`.
    ByPosition,
    /// Its rows differ from the first window's (a series appeared, ended or
    /// moved), or the first window repeats a group: each row's group is
    /// looked up in the group table.
    ByGroupTable,
}

/// How each of `windows` merges, by comparing its `groups` columns with
/// the first window's: e.g. `[ByPosition, ByPosition, ByGroupTable]` when
/// a series appeared in the third window. Every window is `ByGroupTable`
/// when the first one repeats a group (a rollup of series to their job).
#[allow(dead_code)] // Called from `merge_summary` by the implementation.
pub(super) fn plan_window_merges(
    windows: &[&Batch],
    groups: &[usize],
) -> Result<Vec<WindowMerge>, Error> {
    let _ = (windows, groups);
    todo!("merge fast path: compare each window's group columns with the first window's")
}

/// Merge the state column `state_column` of `windows` per group of their
/// `groups` columns, each window as [`plan_window_merges`] says, into the
/// batch the hash merge returns for the same input: the groups' labels in
/// key order, then their merged states.
#[allow(dead_code)] // Called from `merge_summary` by the implementation.
pub(super) async fn merge_by_position(
    windows: &[&Batch],
    state_column: usize,
    groups: &[usize],
    input_schema: &SchemaRef,
    output: &SchemaRef,
    context: &RunContext,
) -> Result<Batch, Error> {
    let _ = (windows, state_column, groups, input_schema, output, context);
    todo!("merge fast path: merge each window's row r into output group r")
}

#[cfg(test)]
mod tests;
