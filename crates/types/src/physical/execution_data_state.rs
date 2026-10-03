//! Exact-operator schema helpers for summary planning.

use thiserror::Error;

use crate::ir::schema::Schema;
use crate::ir::SchemaDerivationError;

/// `schema` as a summary-planning node output: fields and time axis kept,
/// unique keys dropped, closed.
pub fn lift_plain(schema: &Schema) -> Schema {
    Schema::lifted(schema.fields.clone(), schema.time_index)
}

/// Why an exact operator's output schema could not be derived.
#[derive(Debug, Error)]
pub enum ExactOperationSchemaError {
    #[error("exact operator input carries summary state, not plain columns")]
    NonPlainInput,
    #[error("schema derivation failed: {0}")]
    Schema(#[from] SchemaDerivationError),
}
