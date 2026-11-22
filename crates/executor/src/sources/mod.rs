//! Data access. A deployment implements [`DataStore`]: raw data by the
//! `Source` a `Scan` names, and stored materialized views by their
//! [`MaterializedView`]. The executor's `Scan` owns the Planner's time-range
//! and predicate semantics over the raw rows a store returns.
use crate::values::arrow_columns::timestamp_array;
use crate::{
    expressions::CompiledExpression,
    plan::PhysicalOperator,
    runtime::{Input, OutputStream, RunContext, Scope},
    values::{Batch, SchemaRef},
    Error,
};
use futures::{stream, StreamExt};
use planner_types::ir::operator::tumbling_window_chain::CoverLevel;
use planner_types::ir::operator::Source;
use planner_types::ir::schema::DataType;
use planner_types::ir::ScalarExpr;
use planner_types::plan::materialized_view::MaterializedView;
use planner_types::plan::TimeWindow;
use std::sync::Arc;

mod data_store;
pub use data_store::{DataStore, MaterializedViewRequest, RawDataRequest};
mod memory;
pub use memory::InMemoryDataStore;

/// A `Scan` predicate compiled over the scan's rows `output`; bad input when
/// it is not boolean or the executor cannot evaluate it.
pub(crate) fn compile_scan_predicate(
    predicate: &planner_types::ir::ScalarExpr,
    output: &SchemaRef,
) -> Result<CompiledExpression, Error> {
    let predicate = CompiledExpression::compile(predicate, output)?;
    if predicate.dtype().0 != DataType::Bool {
        return Err(Error::Invalid("Scan predicate must be boolean".into()));
    }
    Ok(predicate)
}

/// How a compressed-samples chain's view is read by the
/// `DecodeSamplesInRange`s above it: the sealed windows overlapping the
/// window they read, aligned to the chain's grid (the store holds whole
/// windows), and, when one of them reads unsealed windows, the open window
/// after them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(any(test, feature = "test-support"), derive(serde::Serialize))]
pub(crate) struct SampleWindowsRead {
    /// The chain's tumbling-window width.
    pub(crate) width_ms: i64,
    /// Whether the open window is read too.
    pub(crate) open_window: bool,
}

/// What a `Scan` reads, and how.
enum ScanInput {
    /// A raw data slot: re-applies its time range and predicates.
    RawData {
        source: Source,
        /// Compiled over the slot's schema.
        predicates: Vec<CompiledExpression>,
        /// `predicates` as the store receives them.
        predicate_hints: Vec<ScalarExpr<()>>,
        /// The rows its consumers read, back from the time the run
        /// evaluates it at; `None` when one reads every row.
        read_window: Option<TimeWindow>,
    },
    /// A materialized view slot: exact, never re-filtered; its states are
    /// the store's.
    MaterializedView {
        /// For a compressed-samples chain read by decodes, how its windows
        /// are selected; `None` for every other view.
        sample_windows: Option<SampleWindowsRead>,
        view: MaterializedView,
        /// The rows a read selects, back from the evaluation time.
        read_window: TimeWindow,
        /// A chain read only as a level of `TimeRangeCover`s: a query run
        /// reads only the union of the windows they take from it.
        cover_reads: Option<Vec<CoverLevel>>,
    },
}

/// An input slot bound to a [`DataStore`]: reads it with this run's request
/// and returns the slot's rows in the slot's schema.
pub(crate) struct Scan {
    store: Arc<dyn DataStore>,
    output: SchemaRef,
    input: ScanInput,
}
impl Scan {
    /// A raw data slot reading `source`.
    pub(crate) fn raw_data(
        store: Arc<dyn DataStore>,
        source: Source,
        output: SchemaRef,
        predicates: Vec<CompiledExpression>,
        read_window: Option<TimeWindow>,
    ) -> Self {
        let predicate_hints = predicates
            .iter()
            .map(|predicate| predicate.expression().clone())
            .collect();
        Self {
            store,
            output,
            input: ScanInput::RawData {
                source,
                predicates,
                predicate_hints,
                read_window,
            },
        }
    }
    /// A materialized view slot reading the rows of `view` that
    /// `read_window` selects; read only as `TimeRangeCover` levels
    /// (`cover_reads`), only the union of the windows they take from it;
    /// a compressed-samples chain read by decodes (`sample_windows`), its
    /// sealed windows overlapping the window and maybe its open window.
    pub(crate) fn materialized_view(
        store: Arc<dyn DataStore>,
        view: MaterializedView,
        output: SchemaRef,
        read_window: TimeWindow,
        cover_reads: Option<Vec<CoverLevel>>,
        sample_windows: Option<SampleWindowsRead>,
    ) -> Self {
        Self {
            store,
            output,
            input: ScanInput::MaterializedView {
                sample_windows,
                view,
                read_window,
                cover_reads,
            },
        }
    }
    /// `batch` in the slot's schema; bad input when its columns differ.
    fn to_output(&self, batch: Batch) -> Result<Batch, Error> {
        if batch.schema() == &self.output {
            return Ok(batch);
        }
        let same_columns =
            batch.schema().fields.len() == self.output.fields.len()
                && batch.schema().fields.iter().zip(&self.output.fields).all(
                    |(actual, expected)| {
                        actual.name == expected.name
                            && actual.dtype == expected.dtype
                            && actual.nullable == expected.nullable
                    },
                );
        if !same_columns {
            return Err(Error::Invalid(
                "data store returned a batch in another schema than the slot's".into(),
            ));
        }
        let columns: Vec<usize> = (0..self.output.fields.len()).collect();
        batch.project(self.output.clone(), &columns)
    }
}
impl PhysicalOperator<Batch, SchemaRef> for Scan {
    fn properties(&self, _: &[crate::plan::PlanProperties]) -> crate::plan::PlanProperties {
        crate::plan::PlanProperties {
            boundedness: crate::plan::Boundedness::Bounded,
            emission: crate::plan::Emission::Incremental,
        }
    }

    fn name(&self) -> &str {
        "Scan"
    }
    fn input_schemas(&self) -> Vec<SchemaRef> {
        vec![]
    }
    fn output_schema(&self) -> SchemaRef {
        self.output.clone()
    }
    fn output_bytes(&self, batch: &Batch) -> usize {
        batch.memory_bytes()
    }
    fn start<'a>(
        &'a self,
        inputs: Vec<Input<'a, Batch>>,
        context: RunContext,
    ) -> Result<OutputStream<'a, Batch>, Error> {
        if !inputs.is_empty() {
            return Err(Error::Invalid("Scan cannot have inputs".into()));
        }
        if context.is_cancelled() {
            return Err(Error::Cancelled);
        }
        match &self.input {
            ScanInput::RawData {
                source,
                predicates,
                predicate_hints,
                read_window,
            } => self.start_raw_data(source, predicates, predicate_hints, *read_window, context),
            ScanInput::MaterializedView {
                view,
                read_window,
                cover_reads,
                sample_windows: Some(read),
            } if cover_reads.is_none() => {
                self.start_sample_windows(view, *read_window, *read, context)
            }
            ScanInput::MaterializedView {
                view,
                read_window,
                cover_reads,
                ..
            } => self.start_materialized_view(view, *read_window, cover_reads.as_deref(), context),
        }
    }
}
impl Scan {
    /// The windows of a compressed-samples chain that the decodes above it
    /// read at evaluation time `T`: the sealed windows overlapping
    /// `read_window`, from the grid point at or before its start to the
    /// newest grid point at or before `T` (later windows are not sealed),
    /// then, with `read.open_window`, the store's open window. The decodes
    /// trim the samples to their own ranges.
    fn start_sample_windows<'a>(
        &'a self,
        view: &'a MaterializedView,
        read_window: TimeWindow,
        read: SampleWindowsRead,
        context: RunContext,
    ) -> Result<OutputStream<'a, Batch>, Error> {
        let Scope::Query { evaluation_time_ms } = context.scope else {
            return Err(Error::Invalid(
                "compressed-samples windows are decoded at query time only".into(),
            ));
        };
        let (start_ms, _) = read_window.bounds_ms(evaluation_time_ms);
        let sealed = (
            start_ms.div_euclid(read.width_ms) * read.width_ms,
            evaluation_time_ms.div_euclid(read.width_ms) * read.width_ms,
        );
        let opening = context.clone();
        let stream = stream::once(async move {
            if opening.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let sealed_windows = if sealed.0 < sealed.1 {
                let request = MaterializedViewRequest {
                    time_range_ms: sealed,
                    schema: &self.output,
                };
                self.store
                    .read_materialized_view(view, &request, opening.clone())?
            } else {
                stream::empty().boxed_local()
            };
            Ok(if read.open_window {
                sealed_windows
                    .chain(
                        stream::once({
                            let opening = opening.clone();
                            async move { self.store.read_open_window(view, &self.output, opening) }
                        })
                        .try_flatten(),
                    )
                    .boxed_local()
            } else {
                sealed_windows
            })
        });
        use futures::TryStreamExt;
        Ok(stream
            .try_flatten()
            .map(move |batch| {
                if context.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                let batch = self.to_output(batch?)?;
                batch.mark_states_held_outside_run();
                Ok(batch)
            })
            .boxed_local())
    }

    fn start_materialized_view<'a>(
        &'a self,
        view: &'a MaterializedView,
        read_window: TimeWindow,
        cover_reads: Option<&[CoverLevel]>,
        context: RunContext,
    ) -> Result<OutputStream<'a, Batch>, Error> {
        // A query run reads the window back from its evaluation time; an
        // ingestion run reads the window ending at its end, whatever the
        // offset.
        let at_ms = match context.scope {
            Scope::Query { evaluation_time_ms } => evaluation_time_ms,
            Scope::Ingestion { window_end_ms, .. } => window_end_ms.saturating_add(
                i64::try_from(read_window.ends_before_evaluation_ms).unwrap_or(i64::MAX),
            ),
        };
        let time_range_ms = read_window.bounds_ms(at_ms);
        // Read only as cover levels: at query time only the union of the
        // windows those covers take from it, one read per run of
        // consecutive windows (a wider level's middle, the narrowest
        // level's two edges). Example: the 15 s level of a 1 h cover over
        // 5 min windows reads two ranges of at most 5 min, not the hour;
        // with a 70 min cover too, also that cover's edges.
        let ranges_ms: Vec<(i64, i64)> = match (&context.scope, cover_reads) {
            (Scope::Query { evaluation_time_ms }, Some(cover_reads)) => {
                let mut window_ends: std::collections::BTreeSet<i64> = Default::default();
                let mut width = 0_i64;
                for read in cover_reads {
                    width = i64::try_from(read.level_widths_ms[read.level]).map_err(|_| {
                        Error::Invalid("a cover level's width exceeds Int64".into())
                    })?;
                    let range = i64::try_from(read.range_ms)
                        .map_err(|_| Error::Invalid("a cover's range exceeds Int64".into()))?;
                    let windows =
                        planner_types::ir::operator::tumbling_window_chain::cover_windows(
                            evaluation_time_ms - range,
                            *evaluation_time_ms,
                            &read.level_widths_ms,
                        )
                        .map_err(|error| Error::Invalid(error.to_string()))?;
                    window_ends.extend(
                        windows
                            .into_iter()
                            .filter(|(level, _)| *level == read.level)
                            .map(|(_, window_end)| window_end),
                    );
                }
                let mut ranges: Vec<(i64, i64)> = Vec::new();
                for window_end in window_ends {
                    match ranges.last_mut() {
                        Some((_, last_end)) if *last_end == window_end - width => {
                            *last_end = window_end
                        }
                        _ => ranges.push((window_end - width, window_end)),
                    }
                }
                ranges
            }
            _ => vec![time_range_ms],
        };
        let opening = context.clone();
        let stream = stream::iter(ranges_ms).then(move |time_range_ms| {
            let opening = opening.clone();
            async move {
                if opening.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                let request = MaterializedViewRequest {
                    time_range_ms,
                    schema: &self.output,
                };
                self.store.read_materialized_view(view, &request, opening)
            }
        });
        use futures::TryStreamExt;
        Ok(stream
            .try_flatten()
            .map(move |batch| {
                if context.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                let batch = self.to_output(batch?)?;
                // A materialized view's states are the store's, read by this run.
                batch.mark_states_held_outside_run();
                Ok(batch)
            })
            .boxed_local())
    }
    fn start_raw_data<'a>(
        &'a self,
        source: &'a Source,
        predicates: &'a [CompiledExpression],
        predicate_hints: &'a [ScalarExpr<()>],
        read_window: Option<TimeWindow>,
        context: RunContext,
    ) -> Result<OutputStream<'a, Batch>, Error> {
        // `now()` in the predicates is this run's evaluation time.
        let predicates: Vec<std::borrow::Cow<'_, CompiledExpression>> = predicates
            .iter()
            .map(|predicate| match predicate.reads_current_timestamp() {
                true => std::borrow::Cow::Owned(predicate.at_evaluation_time(&context)),
                false => std::borrow::Cow::Borrowed(predicate),
            })
            .collect();
        let time_range_ms = match context.scope {
            Scope::Query { evaluation_time_ms } => {
                read_window.map(|window| window.bounds_ms(evaluation_time_ms))
            }
            // A slot read through windows reads the rows of the window the
            // run builds.
            Scope::Ingestion {
                window_start_ms,
                window_end_ms,
            } => read_window.map(|_| (window_start_ms, window_end_ms)),
        };
        let time_filter = match time_range_ms {
            Some(range) => Some((
                self.output
                    .time_index
                    .ok_or_else(|| Error::Invalid("windowed Scan lacks a time column".into()))?,
                range,
            )),
            None => None,
        };
        // Opening is lazy: validation and construction of a run perform no I/O.
        let opening = context.clone();
        let stream = stream::once(async move {
            if opening.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let request = RawDataRequest {
                time_range_ms,
                schema: &self.output,
                predicates: predicate_hints,
            };
            self.store.read_raw_data(source, &request, opening)
        });
        use futures::TryStreamExt;
        Ok(stream
            .try_flatten()
            .map(move |batch| {
                if context.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                let batch = self.to_output(batch?)?;
                if predicates.is_empty() && time_filter.is_none() {
                    return Ok(batch);
                }
                // The filtered copy is at most the batch again; the
                // predicates' temporaries (masks, per-value and intermediate
                // columns) at most once more.
                let _workspace = context.reserve(
                    batch
                        .memory_bytes()
                        .checked_mul(2)
                        .ok_or(Error::MemoryLimit)?,
                )?;
                let mut batch = batch;
                // The requested time range, whether or not the store applied it.
                if let Some((time, (start_ms, end_ms))) = time_filter {
                    let times = timestamp_array(batch.column(time)).ok_or_else(|| {
                        Error::Invalid("Scan time column is not a timestamp".into())
                    })?;
                    let keep: arrow::array::BooleanArray = times
                        .iter()
                        .map(|t| Some(t.is_some_and(|t| t > start_ms && t <= end_ms)))
                        .collect();
                    batch = keep_rows(batch, &keep)?;
                }
                // Each predicate is one column over the rows the earlier ones
                // kept; over dictionary-encoded labels it is evaluated once
                // per distinct label value, not once per row.
                for predicate in &predicates {
                    if batch.num_rows() == 0 {
                        break;
                    }
                    let keep = predicate.evaluate_batch(&batch)?;
                    let keep = keep
                        .as_any()
                        .downcast_ref::<arrow::array::BooleanArray>()
                        .ok_or_else(|| Error::Invalid("Scan predicate is not boolean".into()))?;
                    batch = keep_rows(batch, keep)?;
                }
                Ok(batch)
            })
            .boxed_local())
    }
}

/// The rows of `batch` that `keep` keeps; `batch` itself, uncopied, when it
/// keeps every row.
fn keep_rows(batch: Batch, keep: &arrow::array::BooleanArray) -> Result<Batch, Error> {
    if keep.true_count() == batch.num_rows() {
        return Ok(batch);
    }
    batch.filter(keep)
}
