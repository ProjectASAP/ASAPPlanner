use super::*;
use planner_types::plan::physical_asap_dag::PhysicalASAPNodeId;
use std::collections::BTreeMap;

/// An in-memory [`DataStore`], for tests and examples: raw tables by
/// `Source`, and each materialized view's rows by its node id, time-indexed
/// and append-only. A run's output for a view is appended with the time
/// range its rows cover: a chain's window `(window_end - w, window_end]`,
/// or the range of data another view's state summarizes. Appending drops the
/// rows older than the view's `retention_ms` before the newest appended
/// end. A read returns the rows whose time ranges tile the requested range
/// exactly, and fails when one is missing. Raw tables return every row (the
/// executor re-applies the raw data request).
///
/// The store owns the resident batches (a run is not charged for their
/// states again); each cursor clones only the next batch, not the entire
/// data set.
#[derive(Default)]
pub struct InMemoryDataStore {
    raw_data: Vec<(Source, SchemaRef, Vec<Batch>)>,
    /// Each view's appended rows, oldest first.
    materialized_views: BTreeMap<PhysicalASAPNodeId, Vec<AppendedRows>>,
}

/// Rows appended for a view: the `(start, end]` they cover and their batches.
type AppendedRows = ((i64, i64), Vec<Batch>);
impl InMemoryDataStore {
    /// Bind `batches`, all in `schema`, as the rows of `source`. Bad input
    /// when `source` is already bound or a batch is in another schema.
    pub fn with_raw_data(
        mut self,
        source: Source,
        schema: SchemaRef,
        batches: Vec<Batch>,
    ) -> Result<Self, Error> {
        crate::values::validate_schema(&schema)?;
        if self.raw_data.iter().any(|(bound, _, _)| bound == &source) {
            return Err(Error::Invalid(format!(
                "raw data {source:?} is bound twice"
            )));
        }
        if batches.iter().any(|batch| batch.schema() != &schema) {
            return Err(Error::Invalid("raw data batch schema mismatch".into()));
        }
        held_by_store(&batches);
        self.raw_data.push((source, schema, batches));
        Ok(self)
    }
    /// Append `batches` as the rows of `view` covering `time_range_ms`
    /// (`(start, end]`), then drop its rows ending `view.retention_ms()` or
    /// more before `end` (the newest rows are always kept). Bad input when
    /// the range is empty or starts before the end of the rows appended
    /// last (the store is append-only).
    pub fn append_materialized_view(
        &mut self,
        view: &MaterializedView,
        time_range_ms: (i64, i64),
        batches: Vec<Batch>,
    ) -> Result<(), Error> {
        let (start, end) = time_range_ms;
        let rows = self.materialized_views.entry(view.node_id).or_default();
        if start >= end
            || rows
                .last()
                .is_some_and(|((_, last_end), _)| start < *last_end)
        {
            return Err(Error::Invalid(format!(
                "materialized view {}: appended rows ({start}, {end}] are empty or not after the stored ones",
                view.node_id
            )));
        }
        held_by_store(&batches);
        rows.push((time_range_ms, batches));
        let oldest_kept_end =
            end.saturating_sub(i64::try_from(view.retention_ms()).unwrap_or(i64::MAX));
        rows.retain(|((_, row_end), _)| *row_end > oldest_kept_end || *row_end == end);
        Ok(())
    }
}

fn held_by_store(batches: &[Batch]) {
    for batch in batches {
        batch.mark_states_held_outside_run();
    }
}

/// `batches`, one at a time, each charged to `context` while it is read.
fn stream_batches<'a>(
    batches: impl Iterator<Item = Result<Batch, Error>> + 'a,
    context: RunContext,
) -> OutputStream<'a, Batch> {
    stream::iter(batches)
        .map(move |batch| {
            if context.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let batch = batch?;
            let _allocation = context.reserve(batch.memory_bytes())?;
            Ok(batch)
        })
        .boxed_local()
}

impl DataStore for InMemoryDataStore {
    fn read_raw_data(
        &self,
        source: &Source,
        request: &RawDataRequest,
        context: RunContext,
    ) -> Result<OutputStream<'_, Batch>, Error> {
        let (_, schema, batches) = self
            .raw_data
            .iter()
            .find(|(bound, _, _)| bound == source)
            .ok_or_else(|| Error::Invalid(format!("unbound raw data: {source:?}")))?;
        let projection = projection(schema, request.schema)?;
        let output = request.schema.clone();
        Ok(stream_batches(
            batches
                .iter()
                .map(move |batch| batch.project(output.clone(), &projection)),
            context,
        ))
    }
    fn read_materialized_view(
        &self,
        view: &MaterializedView,
        request: &MaterializedViewRequest,
        context: RunContext,
    ) -> Result<OutputStream<'_, Batch>, Error> {
        let (start, end) = request.time_range_ms;
        let missing = || {
            Error::Invalid(format!(
                "materialized view {} has no rows tiling ({start}, {end}]",
                view.node_id
            ))
        };
        let rows = self
            .materialized_views
            .get(&view.node_id)
            .ok_or_else(missing)?;
        let selected: Vec<&AppendedRows> = rows
            .iter()
            .filter(|((row_start, row_end), _)| *row_start >= start && *row_end <= end)
            .collect();
        // The selected ranges, oldest first, must tile `(start, end]`.
        let mut covered_to = start;
        for ((row_start, row_end), _) in &selected {
            if *row_start != covered_to {
                return Err(missing());
            }
            covered_to = *row_end;
        }
        if covered_to != end {
            return Err(missing());
        }
        Ok(stream_batches(
            selected
                .into_iter()
                .flat_map(|(_, batches)| batches.iter().cloned().map(Ok)),
            context,
        ))
    }
}

/// Each of `output`'s columns' index in `schema`, matched by name; bad input
/// when one is missing or has another type or nullability.
fn projection(schema: &SchemaRef, output: &SchemaRef) -> Result<Vec<usize>, Error> {
    output
        .fields
        .iter()
        .map(|field| {
            let index = schema
                .fields
                .iter()
                .position(|candidate| candidate.name == field.name)
                .ok_or_else(|| Error::Invalid(format!("raw data lacks column {}", field.name)))?;
            let candidate = &schema.fields[index];
            if candidate.dtype != field.dtype || candidate.nullable != field.nullable {
                return Err(Error::Invalid(format!(
                    "raw data column {} differs in type from the slot's schema",
                    field.name
                )));
            }
            Ok(index)
        })
        .collect()
}
