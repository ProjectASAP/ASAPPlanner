//! Data access. A deployment implements [`TableProvider`] for raw data and for
//! materialized views; the executor's `Scan` owns the Planner's time-range,
//! predicate and projection semantics over what a provider returns.
use crate::{
    expressions::CompiledExpression,
    plan::{NodeId, PhysicalOperator},
    runtime::{Input, OutputStream, RunContext, Scope},
    values::{Batch, SchemaRef},
    Error,
};
use futures::{stream, StreamExt};
use planner_types::ir::operator::Source;
use planner_types::ir::schema::DataType;
use planner_types::ir::ScalarExpr;
use std::collections::BTreeMap;
use std::sync::Arc;

/// A table a deployment binds to an input slot: raw data (a SQL table, a
/// time series as `(ts, value, *labels)`) or a materialized view's stored
/// states. Metadata must be stable for the lifetime of the binding. Each scan
/// opens an independent cursor; connectors must honor cancellation and bound
/// their own I/O buffers. Dropping a cursor must release its resources. A
/// connector error is never an empty successful scan.
pub trait TableProvider {
    /// Contains every column of the slots it is bound to (matched by field
    /// name, with the same type and nullability); may have more.
    fn schema(&self) -> SchemaRef;
    /// Declare a finite snapshot/window explicitly; execution scope alone does not bound a cursor.
    fn boundedness(&self) -> crate::plan::Boundedness {
        crate::plan::Boundedness::Unknown
    }
    /// Rows for one run. For a table slot `request` is a hint: the executor
    /// re-applies its time range, predicates and projection, so a provider
    /// may ignore any field. A view's provider selects the panes with
    /// `request.time_range_ms` (see [`ScanRequest::time_range_ms`]).
    /// Batches are in [`Self::schema`], or in its `projection` columns when
    /// the provider applies it.
    fn scan(
        &self,
        request: &ScanRequest,
        context: RunContext,
    ) -> Result<OutputStream<'_, Batch>, Error>;
}

/// What a slot reads in one run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScanRequest<'a> {
    /// `(start, end]` in milliseconds on the slot's time column. In a query
    /// run at evaluation time `T` it is the smallest range covering the range windows above
    /// the slot: a `TimeRange(w)` over `TimeShift`s summing to `s` reads
    /// `(T - s - w, T - s]`. A table slot in an ingestion run reads the
    /// ingestion window. `None` when a table slot is read outside a window,
    /// or a view slot's window is not derivable from the DAG (it is only for
    /// a view reading one `TimeRange` directly, as an expanded pane does) or
    /// the run is an ingestion run.
    pub time_range_ms: Option<(i64, i64)>,
    /// The slot's columns, as indices into the provider's schema, in the
    /// slot's order.
    pub projection: Option<&'a [usize]>,
    /// A table slot's `Scan` predicates, over the slot's schema (after
    /// `projection`: column `i` is provider column `projection[i]`). They
    /// may read `now()`, the run's evaluation time, and read no other
    /// operator (`ScalarExpr<()>`). A hint only: the executor re-applies
    /// every predicate.
    pub predicates: &'a [ScalarExpr<()>],
}

/// Explicit source identities; no implicit network discovery or fallback.
#[derive(Default)]
pub struct DataSources {
    sources: Vec<(Source, Arc<dyn TableProvider>)>,
}
impl DataSources {
    pub fn register(
        &mut self,
        identity: Source,
        source: Arc<dyn TableProvider>,
    ) -> Result<(), Error> {
        if self.sources.iter().any(|(key, _)| key == &identity) {
            return Err(Error::Invalid("duplicate data source".into()));
        }
        crate::values::validate_schema(&source.schema())?;
        self.sources.push((identity, source));
        Ok(())
    }
    /// The provider registered for `identity`.
    pub(crate) fn provider(&self, identity: &Source) -> Option<&Arc<dyn TableProvider>> {
        self.sources
            .iter()
            .find(|(key, _)| key == identity)
            .map(|(_, provider)| provider)
    }
}

/// The data a compiled DAG's input slots read in one instantiation: a table
/// slot by its `Scan`'s `Source`, a view slot by its node id.
///
/// Transitional (removed by #969): a runtime binds the raw-window rows
/// `RawDataSampleSelector` or `TableRowSelector` produced at a `TimeRange`
/// node, cut out as a slot, through `views` too, with a provider over the
/// given batches (e.g. [`MemorySource`]).
#[derive(Default)]
pub struct BindingDataSources {
    /// Source → provider, as a catalog: one provider per table or metric,
    /// however many `Scan`s read it.
    pub tables: DataSources,
    /// View slot (node id) → provider of the view's stored states.
    pub views: BTreeMap<NodeId, Arc<dyn TableProvider>>,
}

/// The time range a slot reads at time `T`: `(T - start_before_ms, T -
/// end_before_ms]`, the union of the range windows above it (each
/// `TimeRange(w)` over shifts summing to `s` reads `(T - s - w, T - s]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(any(test, feature = "test-support"), derive(serde::Serialize))]
pub(crate) struct SlotWindow {
    pub(crate) start_before_ms: i64,
    pub(crate) end_before_ms: i64,
}
impl SlotWindow {
    /// The window of a `TimeRange` of `width_ms` over shifts summing to `offset_ms`.
    pub(crate) fn new(offset_ms: i64, width_ms: i64) -> Result<Self, Error> {
        Ok(Self {
            start_before_ms: offset_ms
                .checked_add(width_ms)
                .ok_or_else(|| Error::Invalid("window overflows Int64".into()))?,
            end_before_ms: offset_ms,
        })
    }
    /// The smallest window holding both.
    pub(crate) fn union(self, other: Self) -> Self {
        Self {
            start_before_ms: self.start_before_ms.max(other.start_before_ms),
            end_before_ms: self.end_before_ms.min(other.end_before_ms),
        }
    }
    /// `(start, end]` at time `t_ms`.
    fn at(self, t_ms: i64) -> Result<(i64, i64), Error> {
        let overflow = || Error::Invalid("window overflows Int64".into());
        Ok((
            t_ms.checked_sub(self.start_before_ms)
                .ok_or_else(overflow)?,
            t_ms.checked_sub(self.end_before_ms).ok_or_else(overflow)?,
        ))
    }
}

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

/// Each of `output`'s columns' index in `provider`'s schema, matched by
/// name; bad input when one is missing or has another type or nullability.
fn projection(provider: &SchemaRef, output: &SchemaRef) -> Result<Vec<usize>, Error> {
    output
        .fields
        .iter()
        .map(|field| {
            let index = provider
                .fields
                .iter()
                .position(|candidate| candidate.name == field.name)
                .ok_or_else(|| {
                    Error::Invalid(format!("provider schema lacks column {}", field.name))
                })?;
            let candidate = &provider.fields[index];
            if candidate.dtype != field.dtype || candidate.nullable != field.nullable {
                return Err(Error::Invalid(format!(
                    "provider column {} differs in type from the slot's schema",
                    field.name
                )));
            }
            Ok(index)
        })
        .collect()
}

/// An input slot bound to its provider: reads the provider with this run's
/// [`ScanRequest`] and returns the slot's rows in the slot's schema.
pub(crate) struct Scan {
    provider: Arc<dyn TableProvider>,
    output: SchemaRef,
    /// Each output column's index in the provider's schema.
    projection: Vec<usize>,
    /// A table slot's predicates, compiled over `output`; none for a view.
    predicates: Vec<CompiledExpression>,
    /// `predicates` as the provider receives them.
    predicate_hints: Vec<ScalarExpr<()>>,
    window: Option<SlotWindow>,
    /// A table slot re-applies its requested time range; a view's
    /// provider selects its panes, which the executor cannot check.
    reapplies_time_range: bool,
}
impl Scan {
    /// A table slot over `provider`, which must contain `output`'s columns
    /// and declare `Bounded`.
    pub(crate) fn table(
        provider: Arc<dyn TableProvider>,
        output: SchemaRef,
        predicates: Vec<CompiledExpression>,
        window: Option<SlotWindow>,
    ) -> Result<Self, Error> {
        let mut scan = Self::view(provider, output, window)?;
        scan.predicate_hints = predicates
            .iter()
            .map(|predicate| predicate.expression().clone())
            .collect();
        scan.predicates = predicates;
        scan.reapplies_time_range = true;
        Ok(scan)
    }
    /// A view slot over `provider`, which must contain `output`'s columns
    /// and declare `Bounded`.
    pub(crate) fn view(
        provider: Arc<dyn TableProvider>,
        output: SchemaRef,
        window: Option<SlotWindow>,
    ) -> Result<Self, Error> {
        let projection = projection(&provider.schema(), &output)?;
        if provider.boundedness() != crate::plan::Boundedness::Bounded {
            return Err(Error::Invalid("provider does not declare Bounded".into()));
        }
        Ok(Self {
            provider,
            output,
            projection,
            predicates: vec![],
            predicate_hints: vec![],
            window,
            reapplies_time_range: false,
        })
    }
    /// The time range this run reads (see [`ScanRequest::time_range_ms`]).
    fn time_range_ms(&self, context: &RunContext) -> Result<Option<(i64, i64)>, Error> {
        let Some(window) = self.window else {
            return Ok(None);
        };
        match context.scope {
            Scope::Query {
                evaluation_time_ms, ..
            } => window.at(evaluation_time_ms).map(Some),
            Scope::Ingestion {
                window_start_ms,
                window_end_ms,
                ..
            } => Ok(self
                .reapplies_time_range
                .then_some((window_start_ms, window_end_ms))),
        }
    }
    /// `batch` in the slot's schema: projected when it is in the provider's
    /// schema, relabeled when the provider already projected it.
    fn to_output(&self, batch: Batch, provider: &SchemaRef) -> Result<Batch, Error> {
        if batch.schema() == &self.output {
            return Ok(batch);
        }
        if batch.schema() == provider {
            return batch.project(self.output.clone(), &self.projection);
        }
        let projected =
            batch.schema().fields.len() == self.output.fields.len()
                && batch.schema().fields.iter().zip(&self.output.fields).all(
                    |(actual, expected)| {
                        actual.name == expected.name
                            && actual.dtype == expected.dtype
                            && actual.nullable == expected.nullable
                    },
                );
        if !projected {
            return Err(Error::Invalid(
                "provider returned a batch in neither its schema nor the slot's".into(),
            ));
        }
        let columns: Vec<usize> = (0..self.output.fields.len()).collect();
        batch.project(self.output.clone(), &columns)
    }
}
impl PhysicalOperator<Batch, SchemaRef> for Scan {
    fn properties(&self, _: &[crate::plan::PlanProperties]) -> crate::plan::PlanProperties {
        crate::plan::PlanProperties {
            boundedness: self.provider.boundedness(),
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
        // `now()` in the predicates is this run's evaluation time.
        let predicates: Vec<std::borrow::Cow<'_, CompiledExpression>> = self
            .predicates
            .iter()
            .map(|predicate| match predicate.reads_current_timestamp() {
                true => std::borrow::Cow::Owned(predicate.at_evaluation_time(&context)),
                false => std::borrow::Cow::Borrowed(predicate),
            })
            .collect();
        let time_range_ms = self.time_range_ms(&context)?;
        let time_filter = match time_range_ms {
            Some(range) if self.reapplies_time_range => Some((
                self.output
                    .time_index
                    .ok_or_else(|| Error::Invalid("windowed Scan lacks a time column".into()))?,
                range,
            )),
            _ => None,
        };
        let provider_schema = self.provider.schema();
        // Opening is lazy: validation and construction of a run perform no I/O.
        let opening = context.clone();
        let stream = stream::once(async move {
            if opening.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let request = ScanRequest {
                time_range_ms,
                projection: Some(&self.projection),
                predicates: &self.predicate_hints,
            };
            self.provider.scan(&request, opening)
        });
        use futures::TryStreamExt;
        Ok(stream
            .try_flatten()
            .map(move |batch| {
                if context.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                let batch = self.to_output(batch?, &provider_schema)?;
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
                // The requested time range, whether or not the provider applied it.
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
                    batch = batch.filter(&keep)?;
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
                    batch = batch.filter(keep)?;
                }
                Ok(batch)
            })
            .boxed_local())
    }
}

mod memory;
pub use memory::MemorySource;
