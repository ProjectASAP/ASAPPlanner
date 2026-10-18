//! Typed failures from operator/scalar schema and type derivation.
//!
//! [`SchemaDerivationError`] distinguishes invalid scalar signatures, out-of-range
//! grouping columns, empty concatenations, and invalid sample columns.
//! Structural DAG and execution-timing validation have separate error types.
use crate::ir::schema::ColumnId;
use thiserror::Error;
/// Errors from schema and type derivation over an operator DAG.
#[derive(Debug, Error)]
pub enum SchemaDerivationError {
    #[error("invalid scalar function signature: {0}")]
    InvalidScalarSignature(String),
    #[error("by-column id {0} out of range (input has {1} columns)")]
    InvalidGroupByColumn(ColumnId, usize),
    #[error("Concat requires at least one child")]
    EmptyConcat,
    #[error("invalid per-series sample column: {0}")]
    InvalidSampleColumn(String),
    #[error("invalid summary coverage: {0}")]
    Coverage(#[from] crate::ir::properties::summary_coverage::CoverageError),
}
