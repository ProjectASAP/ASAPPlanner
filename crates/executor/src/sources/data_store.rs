use crate::{
    runtime::{OutputStream, RunContext},
    values::{Batch, SchemaRef},
    Error,
};
use planner_types::ir::materialized_view::MaterializedView;
use planner_types::ir::operator::Source;
use planner_types::ir::ScalarExpr;

/// A deployment's raw data and stored materialized views, as one run reads
/// them. The deployment passes one handle per run, and the handle presents
/// one consistent state to every read of that run: a refresh that publishes
/// or replaces a view's state during the run is seen by the next run, not
/// by this one. How it keeps that snapshot is the deployment's choice.
///
/// Each read opens an independent cursor; a store must honor cancellation
/// and bound its own I/O buffers, and dropping a cursor must release its
/// resources. A store error is never an empty successful read.
pub trait DataStore {
    /// Rows of the raw table or time series `source` names, in
    /// `request.schema`. `request` is otherwise a hint: the executor
    /// re-applies its time range and predicates, so a store may ignore
    /// them.
    fn read_raw_data(
        &self,
        source: &Source,
        request: &RawDataRequest,
        context: RunContext,
    ) -> Result<OutputStream<'_, Batch>, Error>;

    /// The stored states of `view` whose intervals tile
    /// `request.time_range_ms` exactly, in `request.schema` (group columns
    /// and one state column). Exact: the executor never re-filters them,
    /// so the store never counts an observation twice, never returns
    /// another definition's state, and never treats missing state as
    /// empty; it returns an error instead. The states are the store's: a
    /// run is not charged for them.
    fn read_materialized_view(
        &self,
        view: &MaterializedView,
        request: &MaterializedViewRequest,
        context: RunContext,
    ) -> Result<OutputStream<'_, Batch>, Error>;
}

/// What a raw data slot (a `Scan`) reads in one run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RawDataRequest<'a> {
    /// `(start, end]` in milliseconds, on the slot's time column.
    ///
    /// - Query run at evaluation time `T`: the smallest range covering
    ///   every window the `Scan`'s readers read
    ///   ([`PhysicalASAPDAG::scan_window`](planner_types::ir::physical_export::PhysicalASAPDAG::scan_window));
    ///   `None` when a reader reads the `Scan` outside a window.
    /// - Ingestion run: the run's `(window_start_ms, window_end_ms]` when
    ///   the `Scan` is read through windows, else `None`. Each
    ///   `TimeRange(w)` then keeps `(window_end_ms - w, window_end_ms]`,
    ///   whatever the shifts below it.
    pub time_range_ms: Option<(i64, i64)>,
    /// The slot's columns, the `Scan`'s schema. The store returns rows with
    /// these columns (name, type and nullability), in this order.
    pub schema: &'a SchemaRef,
    /// The `Scan`'s predicates over `schema`. They may read `now()`, the
    /// run's evaluation time, and read no other operator (`ScalarExpr<()>`).
    pub predicates: &'a [ScalarExpr<()>],
}

/// What a materialized view slot reads in one run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MaterializedViewRequest<'a> {
    /// `(start, end]` in milliseconds: the interval of data the view's state
    /// summarizes in this run
    /// ([`PhysicalASAPDAG::view_window`](planner_types::ir::physical_export::PhysicalASAPDAG::view_window)).
    /// In a query run at `T` with view window `(T - o - w, T - o]`, it is
    /// that range; in an ingestion run, `(window_end_ms - w, window_end_ms]`.
    pub time_range_ms: (i64, i64),
    /// The slot's columns, the view node's schema: group columns and one
    /// state column. The store returns rows with these columns, in this
    /// order.
    pub schema: &'a SchemaRef,
}
