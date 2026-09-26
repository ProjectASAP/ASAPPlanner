use super::*;
/// Immutable in-memory raw data. The connector owns the resident input; each
/// cursor clones only the next requested batch, not the entire data set.
pub struct MemorySource {
    schema: Schema,
    batches: Vec<Batch>,
}
impl MemorySource {
    pub fn new(schema: Schema, batches: Vec<Batch>) -> Result<Self, Error> {
        crate::values::validate_schema(&schema)?;
        if schema
            .fields
            .iter()
            .any(|f| !matches!(f.dtype, SummaryFamilyType::Plain(_)))
        {
            return Err(Error::Invalid(
                "raw source cannot contain summary states".into(),
            ));
        }
        if batches.iter().any(|batch| batch.schema() != &schema) {
            return Err(Error::Invalid("memory source batch schema mismatch".into()));
        }
        Ok(Self { schema, batches })
    }
}
impl RawSource for MemorySource {
    fn boundedness(&self) -> crate::plan::Boundedness {
        crate::plan::Boundedness::Bounded
    }
    fn schema(&self) -> Schema {
        self.schema.clone()
    }
    fn scan(&self, context: RunContext) -> Result<OutputStream<'_, Batch>, Error> {
        Ok(stream::iter(self.batches.iter())
            .map(move |batch| {
                if context.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                let _allocation = context.reserve(batch.bytes())?;
                Ok(batch.clone())
            })
            .boxed_local())
    }
}
