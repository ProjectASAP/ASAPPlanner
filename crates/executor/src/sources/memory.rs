use super::*;
use planner_types::plan::physical_asap_dag::PhysicalASAPNodeId;
use std::collections::BTreeMap;

/// An in-memory [`DataStore`]: raw tables by `Source`, and each
/// materialized view's rows by its node id, time-indexed and append-only.
/// A run's output for a view is appended with the time range its rows
/// cover: a chain's window `(window_end - w, window_end]`, or the range of
/// data another view's state summarizes. Appending drops the rows older
/// than the view's `retention_ms` before the newest appended end. A read
/// returns the rows whose time ranges tile the requested range exactly, and
/// fails when one is missing.
///
/// A raw table is bound whole (`with_raw_data`, `test-support`): every read returns
/// every row; or bound for appending ([`Self::with_appended_raw_data`]): a
/// runtime appends each batch of samples with the `(start, end]` it covers
/// as it arrives, a read returns the batches overlapping the requested
/// time range, and appending drops the batches older than the table's
/// lookback. Either way the executor re-applies the raw data request.
///
/// The store owns the resident batches (a run is not charged for their
/// states again); each cursor clones only the next batch, not the entire
/// data set.
#[derive(Default)]
pub struct InMemoryDataStore {
    raw_data: Vec<RawTable>,
    /// Each view's appended rows, oldest first.
    materialized_views: BTreeMap<PhysicalASAPNodeId, Vec<AppendedRows>>,
}

/// Rows appended for a view or a raw table bound for appending: the
/// `(start, end]` they cover and their batches.
type AppendedRows = ((i64, i64), Vec<Batch>);

/// The rows of one raw table.
struct RawTable {
    source: Source,
    schema: SchemaRef,
    rows: RawRows,
}

enum RawRows {
    /// Bound whole: every read returns every row. Only the Planner's own
    /// tests bind a table whole (`InMemoryDataStore::with_raw_data`).
    #[cfg(any(test, feature = "test-support"))]
    Whole(Vec<Batch>),
    /// Appended as samples arrive, oldest first; appending drops the rows
    /// ending `lookback_ms` or more before the newest end (`None`: kept
    /// forever).
    Appended {
        lookback_ms: Option<u64>,
        rows: Vec<AppendedRows>,
    },
}

impl InMemoryDataStore {
    /// Bind `batches`, all in `schema`, as the rows of `source`. Bad input
    /// when `source` is already bound or a batch is in another schema. A
    /// fixture for the Planner's own tests (`test-support`); a runtime binds
    /// raw data for appending ([`Self::with_appended_raw_data`]).
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_raw_data(
        mut self,
        source: Source,
        schema: SchemaRef,
        batches: Vec<Batch>,
    ) -> Result<Self, Error> {
        if batches.iter().any(|batch| batch.schema() != &schema) {
            return Err(Error::Invalid("raw data batch schema mismatch".into()));
        }
        held_by_store(&batches);
        self.bind_raw_data(source, schema, RawRows::Whole(batches))?;
        Ok(self)
    }

    /// Bind `source`, in `schema`, for appending its rows as they arrive
    /// ([`Self::append_raw_data`]), keeping each batch until the newest
    /// appended end is `lookback_ms` past its end (`None`: forever). Bad
    /// input when `source` is already bound. Example: a runtime keeping
    /// the last 5 minutes of a time series every query-time `Scan` of it
    /// reads binds it with `Some(300_000)`.
    pub fn with_appended_raw_data(
        mut self,
        source: Source,
        schema: SchemaRef,
        lookback_ms: Option<u64>,
    ) -> Result<Self, Error> {
        let rows = RawRows::Appended {
            lookback_ms,
            rows: Vec::new(),
        };
        self.bind_raw_data(source, schema, rows)?;
        Ok(self)
    }

    /// Append `batches`, all in the table's schema, as the rows of `source`
    /// covering `time_range_ms` (`(start, end]`), then drop its batches
    /// ending `lookback_ms` or more before `end` (the newest are always
    /// kept). Bad input when `source` is not bound for appending, a batch
    /// is in another schema, or the range is empty or starts before the end
    /// of the rows appended last (the store is append-only).
    pub fn append_raw_data(
        &mut self,
        source: &Source,
        time_range_ms: (i64, i64),
        batches: Vec<Batch>,
    ) -> Result<(), Error> {
        let table = self
            .raw_data
            .iter_mut()
            .find(|table| &table.source == source)
            .ok_or_else(|| Error::Invalid(format!("unbound raw data: {source:?}")))?;
        let (lookback_ms, rows) = match &mut table.rows {
            RawRows::Appended { lookback_ms, rows } => (lookback_ms, rows),
            #[cfg(any(test, feature = "test-support"))]
            RawRows::Whole(_) => {
                return Err(Error::Invalid(format!(
                    "raw data {source:?} is bound whole, not for appending"
                )))
            }
        };
        if batches.iter().any(|batch| batch.schema() != &table.schema) {
            return Err(Error::Invalid("raw data batch schema mismatch".into()));
        }
        let (start, end) = time_range_ms;
        if start >= end
            || rows
                .last()
                .is_some_and(|((_, last_end), _)| start < *last_end)
        {
            return Err(Error::Invalid(format!(
                "raw data {source:?}: appended rows ({start}, {end}] are empty or not after the stored ones"
            )));
        }
        held_by_store(&batches);
        rows.push((time_range_ms, batches));
        if let Some(lookback_ms) = *lookback_ms {
            drop_rows_ending_before(rows, end, lookback_ms);
        }
        Ok(())
    }

    /// Bytes of the raw rows held.
    pub fn raw_data_bytes(&self) -> usize {
        self.raw_data
            .iter()
            .map(|table| match &table.rows {
                #[cfg(any(test, feature = "test-support"))]
                RawRows::Whole(batches) => batches_bytes(batches),
                RawRows::Appended { rows, .. } => rows
                    .iter()
                    .map(|(_, batches)| batches_bytes(batches))
                    .sum::<usize>(),
            })
            .sum()
    }

    /// Bytes of the materialized views' stored rows.
    pub fn materialized_view_bytes(&self) -> usize {
        self.materialized_views
            .values()
            .flatten()
            .map(|(_, batches)| batches_bytes(batches))
            .sum()
    }

    /// Bind `source` to `rows`, all in `schema`; bad input when it is
    /// already bound.
    fn bind_raw_data(
        &mut self,
        source: Source,
        schema: SchemaRef,
        rows: RawRows,
    ) -> Result<(), Error> {
        crate::values::validate_schema(&schema)?;
        if self.raw_data.iter().any(|table| table.source == source) {
            return Err(Error::Invalid(format!(
                "raw data {source:?} is bound twice"
            )));
        }
        self.raw_data.push(RawTable {
            source,
            schema,
            rows,
        });
        Ok(())
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
        drop_rows_ending_before(rows, end, view.retention_ms());
        Ok(())
    }
}

/// Drops the rows of `rows` ending `retention_ms` or more before `end`,
/// keeping those ending at `end`: retention 15 at end 30 drops `(0, 10]`
/// and keeps `(10, 20]`.
fn drop_rows_ending_before(rows: &mut Vec<AppendedRows>, end: i64, retention_ms: u64) {
    let oldest_kept_end = end.saturating_sub(i64::try_from(retention_ms).unwrap_or(i64::MAX));
    rows.retain(|((_, row_end), _)| *row_end > oldest_kept_end || *row_end == end);
}

fn batches_bytes(batches: &[Batch]) -> usize {
    batches.iter().map(Batch::memory_bytes).sum()
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
        let table = self
            .raw_data
            .iter()
            .find(|table| &table.source == source)
            .ok_or_else(|| Error::Invalid(format!("unbound raw data: {source:?}")))?;
        let projection = projection(&table.schema, request.schema)?;
        let output = request.schema.clone();
        // An appended table returns the batches overlapping the requested
        // range; the executor re-applies it row by row.
        let batches: Box<dyn Iterator<Item = &Batch>> = match (&table.rows, request.time_range_ms) {
            #[cfg(any(test, feature = "test-support"))]
            (RawRows::Whole(batches), _) => Box::new(batches.iter()),
            (RawRows::Appended { rows, .. }, None) => {
                Box::new(rows.iter().flat_map(|(_, batches)| batches))
            }
            (RawRows::Appended { rows, .. }, Some((start, end))) => Box::new(
                rows.iter()
                    .filter(move |((row_start, row_end), _)| *row_end > start && *row_start < end)
                    .flat_map(|(_, batches)| batches),
            ),
        };
        Ok(stream_batches(
            batches.map(move |batch| batch.project(output.clone(), &projection)),
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
