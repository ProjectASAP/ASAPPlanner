//! Data access. A deployment implements [`DataStore`]: raw data by the
//! `Source` a `Scan` names, and stored materialized views by their
//! [`MaterializedView`]. The executor's `Scan` owns the Planner's time-range
//! and predicate semantics over the raw rows a store returns.
use crate::{
    expressions::CompiledExpression,
    plan::PhysicalOperator,
    runtime::{Input, OutputStream, RunContext, Scope},
    values::{Batch, SchemaRef},
    Error,
};
use futures::{stream, StreamExt};
use planner_types::ir::materialized_view::MaterializedView;
use planner_types::ir::operator::Source;
use planner_types::ir::physical_export::ScanWindow;
use planner_types::ir::schema::DataType;
use planner_types::ir::ScalarExpr;
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

/// What a `Scan` reads, and how.
enum ScanInput {
    /// A raw data slot: re-applies its time range and predicates.
    RawData {
        source: Source,
        /// Compiled over the slot's schema.
        predicates: Vec<CompiledExpression>,
        /// `predicates` as the store receives them.
        predicate_hints: Vec<ScalarExpr<()>>,
        /// The rows its readers read.
        window: ScanWindow,
    },
    /// A materialized view slot: exact, never re-filtered; its states are
    /// the store's.
    MaterializedView {
        view: MaterializedView,
        /// The data its state summarizes, back from the evaluation time;
        /// always `Within`.
        window: ScanWindow,
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
        window: ScanWindow,
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
                window,
            },
        }
    }
    /// A materialized view slot reading `view`, whose state summarizes
    /// `window`; bad input when the window is unbounded.
    pub(crate) fn materialized_view(
        store: Arc<dyn DataStore>,
        view: MaterializedView,
        output: SchemaRef,
        window: ScanWindow,
    ) -> Result<Self, Error> {
        if window == ScanWindow::Unbounded {
            return Err(Error::Invalid(format!(
                "materialized view {} summarizes an unbounded window",
                view.node_id
            )));
        }
        Ok(Self {
            store,
            output,
            input: ScanInput::MaterializedView { view, window },
        })
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
                window,
            } => self.start_raw_data(source, predicates, predicate_hints, *window, context),
            ScanInput::MaterializedView { view, window } => {
                self.start_materialized_view(view, *window, context)
            }
        }
    }
}
impl Scan {
    fn start_materialized_view<'a>(
        &'a self,
        view: &'a MaterializedView,
        window: ScanWindow,
        context: RunContext,
    ) -> Result<OutputStream<'a, Batch>, Error> {
        // A query run reads the window back from its evaluation time; an
        // ingestion run builds the windows ending at its end.
        let at_ms = match context.scope {
            Scope::Query { evaluation_time_ms } => evaluation_time_ms,
            Scope::Ingestion { window_end_ms, .. } => match window {
                ScanWindow::Within { end_offset_ms, .. } => {
                    window_end_ms.saturating_add(end_offset_ms)
                }
                ScanWindow::Unbounded => window_end_ms,
            },
        };
        let time_range_ms = window
            .bounds_ms(at_ms)
            .ok_or_else(|| Error::Invalid("unbounded materialized view window".into()))?;
        let opening = context.clone();
        let stream = stream::once(async move {
            if opening.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let request = MaterializedViewRequest {
                time_range_ms,
                schema: &self.output,
            };
            self.store.read_materialized_view(view, &request, opening)
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
        window: ScanWindow,
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
            Scope::Query { evaluation_time_ms } => window.bounds_ms(evaluation_time_ms),
            // A slot read through windows reads the rows of the window the
            // run builds.
            Scope::Ingestion {
                window_start_ms,
                window_end_ms,
            } => (window != ScanWindow::Unbounded).then_some((window_start_ms, window_end_ms)),
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
                    let times = batch
                        .column(time)
                        .as_any()
                        .downcast_ref::<arrow::array::TimestampMillisecondArray>()
                        .ok_or_else(|| {
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
