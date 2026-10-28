use super::*;
use planner_types::ir::physical_export::PhysicalASAPNodeId;
use std::collections::BTreeMap;

/// An immutable in-memory [`DataStore`], for tests and examples: raw tables
/// by `Source`, and each materialized view's states by its node id. A
/// deployment's store keys views by their coverage and selects their states
/// by time range; this one returns every state bound to the node, and every
/// row of a table (the executor re-applies the raw data request).
///
/// The store owns the resident batches (a run is not charged for their
/// states again); each cursor clones only the next batch, not the entire
/// data set.
#[derive(Default)]
pub struct InMemoryDataStore {
    raw_data: Vec<(Source, SchemaRef, Vec<Batch>)>,
    materialized_views: BTreeMap<PhysicalASAPNodeId, Vec<Batch>>,
}
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
            return Err(Error::Invalid(format!("raw data {source:?} is bound twice")));
        }
        if batches.iter().any(|batch| batch.schema() != &schema) {
            return Err(Error::Invalid("raw data batch schema mismatch".into()));
        }
        held_by_store(&batches);
        self.raw_data.push((source, schema, batches));
        Ok(self)
    }
    /// Bind `batches` as the states of the materialized view at node
    /// `view`. Bad input when the node is already bound.
    pub fn with_materialized_view(
        mut self,
        view: PhysicalASAPNodeId,
        batches: Vec<Batch>,
    ) -> Result<Self, Error> {
        if self.materialized_views.contains_key(&view) {
            return Err(Error::Invalid(format!(
                "materialized view {view} is bound twice"
            )));
        }
        held_by_store(&batches);
        self.materialized_views.insert(view, batches);
        Ok(self)
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
        _: &MaterializedViewRequest,
        context: RunContext,
    ) -> Result<OutputStream<'_, Batch>, Error> {
        let batches = self
            .materialized_views
            .get(&view.node_id)
            .ok_or_else(|| {
                Error::Invalid(format!("unbound materialized view {}", view.node_id))
            })?;
        Ok(stream_batches(batches.iter().cloned().map(Ok), context))
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
                .ok_or_else(|| {
                    Error::Invalid(format!("raw data lacks column {}", field.name))
                })?;
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
