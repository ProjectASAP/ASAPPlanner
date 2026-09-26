//! Raw data access. Connectors provide rows; Scan owns Planner predicate semantics.
use crate::{
    expressions::CompiledExpression,
    plan::PhysicalOperator,
    runtime::{Input, OutputStream, RunContext},
    values::{Batch, Schema, Value},
    Error,
};
use futures::{stream, StreamExt};
use planner_types::{
    post_asap::{SummaryFamilyType, SummaryField, SummarySchema},
    pre_asap::{DataType, QueryExpr, Source},
};
use std::sync::Arc;

/// A bound data source. Metadata must be stable for the lifetime of the binding.
/// Each scan opens an independent cursor. Connectors return raw, unfiltered rows
/// and must honor cancellation and bound their own I/O buffers. Dropping a cursor
/// must release its resources. A connector error is never an empty successful scan.
pub trait RawSource {
    fn schema(&self) -> Schema;
    /// Declare a finite snapshot/window explicitly; execution scope alone does not bound a cursor.
    fn boundedness(&self) -> crate::plan::Boundedness {
        crate::plan::Boundedness::Unknown
    }
    fn scan(&self, context: RunContext) -> Result<OutputStream<'_, Batch>, Error>;
}

/// Explicit source identities; no implicit network discovery or fallback.
#[derive(Default)]
pub struct DataSources {
    sources: Vec<(Source, Arc<dyn RawSource>)>,
}
impl DataSources {
    pub fn register(&mut self, identity: Source, source: Arc<dyn RawSource>) -> Result<(), Error> {
        if self.sources.iter().any(|(key, _)| key == &identity) {
            return Err(Error::Invalid("duplicate data source".into()));
        }
        crate::values::validate_schema(&source.schema())?;
        self.sources.push((identity, source));
        Ok(())
    }
    pub fn bind(&self, expression: &QueryExpr) -> Result<Scan, Error> {
        let QueryExpr::Scan {
            source,
            predicates,
            schema,
        } = expression
        else {
            return Err(Error::Invalid(
                "raw Scan requires a Planner Scan leaf".into(),
            ));
        };
        let output = Arc::new(SummarySchema {
            fields: schema
                .columns
                .iter()
                .map(|column| SummaryField {
                    name: column.name.clone(),
                    dtype: SummaryFamilyType::Plain(column.dtype.clone()),
                    nullable: column.nullable,
                })
                .collect(),
            time_index: schema.time_index,
        });
        crate::values::validate_schema(&output)?;
        let reader = self
            .sources
            .iter()
            .find(|(key, _)| key == source)
            .map(|(_, reader)| reader.clone())
            .ok_or_else(|| Error::Invalid(format!("unbound raw source: {source:?}")))?;
        if reader.schema() != output {
            return Err(Error::Invalid(
                "raw source differs from Planner Scan schema".into(),
            ));
        }
        let predicates = predicates
            .iter()
            .map(|predicate| {
                let predicate = CompiledExpression::compile(&predicate.0, &output)?;
                if predicate.dtype().0 != DataType::Bool {
                    return Err(Error::Invalid("Scan predicate must be boolean".into()));
                }
                Ok(predicate)
            })
            .collect::<Result<Vec<_>, Error>>()?;
        Ok(Scan {
            reader,
            output,
            predicates,
        })
    }
}

pub struct Scan {
    reader: Arc<dyn RawSource>,
    output: Schema,
    predicates: Vec<CompiledExpression>,
}
impl PhysicalOperator<Batch, Schema> for Scan {
    fn properties(&self, _: &[crate::plan::PlanProperties]) -> crate::plan::PlanProperties {
        crate::plan::PlanProperties {
            boundedness: self.reader.boundedness(),
            emission: crate::plan::Emission::Incremental,
        }
    }

    fn name(&self) -> &str {
        "Scan"
    }
    fn input_schemas(&self) -> Vec<Schema> {
        vec![]
    }
    fn output_schema(&self) -> Schema {
        self.output.clone()
    }
    fn output_bytes(&self, batch: &Batch) -> usize {
        batch.bytes()
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
        // Opening is lazy: validation and construction of a run perform no I/O.
        let opening = context.clone();
        let stream = stream::once(async move {
            if opening.is_cancelled() {
                return Err(Error::Cancelled);
            }
            self.reader.scan(opening)
        });
        use futures::TryStreamExt;
        Ok(stream
            .try_flatten()
            .map(move |batch| {
                if context.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                let batch = batch?;
                if batch.schema() != &self.output {
                    return Err(Error::Invalid(
                        "connector returned a different Scan schema".into(),
                    ));
                }
                if self.predicates.is_empty() {
                    return Ok(batch);
                }
                let _workspace =
                    context.reserve(batch.bytes().checked_mul(2).ok_or(Error::MemoryLimit)?)?;
                let mut rows = Vec::new();
                for row in batch.rows() {
                    if context.is_cancelled() {
                        return Err(Error::Cancelled);
                    }
                    let mut keep = true;
                    for predicate in &self.predicates {
                        match predicate.evaluate(row)? {
                            Value::Bool(true) => {}
                            Value::Bool(false) | Value::Null => {
                                keep = false;
                                break;
                            }
                            _ => {
                                return Err(Error::Invalid("Scan predicate is not boolean".into()))
                            }
                        }
                    }
                    if keep {
                        rows.push(row.clone());
                    }
                }
                Batch::try_new(self.output.clone(), rows)
            })
            .boxed_local())
    }
}

mod memory;
pub use memory::MemorySource;
