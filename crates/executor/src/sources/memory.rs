use super::*;
/// Immutable in-memory table: raw rows, or a view's stored states. The
/// provider owns the resident batches (a run is not charged for their
/// states again); each cursor clones only the next requested batch, not the
/// entire data set. It ignores the [`ScanRequest`] and returns every batch.
pub struct MemorySource {
    schema: SchemaRef,
    batches: Vec<Batch>,
}
impl MemorySource {
    pub fn new(schema: SchemaRef, batches: Vec<Batch>) -> Result<Self, Error> {
        crate::values::validate_schema(&schema)?;
        if batches.iter().any(|batch| batch.schema() != &schema) {
            return Err(Error::Invalid("memory source batch schema mismatch".into()));
        }
        for batch in &batches {
            batch.mark_states_held_outside_run();
        }
        Ok(Self { schema, batches })
    }
}
impl TableProvider for MemorySource {
    fn boundedness(&self) -> crate::plan::Boundedness {
        crate::plan::Boundedness::Bounded
    }
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
    fn scan(&self, _: &ScanRequest, context: RunContext) -> Result<OutputStream<'_, Batch>, Error> {
        Ok(stream::iter(self.batches.iter())
            .map(move |batch| {
                if context.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                let _allocation = context.reserve(batch.memory_bytes())?;
                Ok(batch.clone())
            })
            .boxed_local())
    }
}
