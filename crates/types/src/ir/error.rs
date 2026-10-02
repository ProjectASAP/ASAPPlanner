//! Errors validating operator graphs and scalar expressions.
use crate::pre_asap::ColumnId;
use thiserror::Error;
/// Errors from schema derivation over a operator DAG.
#[derive(Debug, Error)]
pub enum QueryExprError {
    #[error("invalid scalar function signature: {0}")]
    InvalidScalarSignature(String),
    #[error("by-column id {0} out of range (input has {1} columns)")]
    InvalidGroupByColumn(ColumnId, usize),
    #[error("Concat requires at least one child")]
    EmptyConcat,
    #[error("invalid per-series sample column: {0}")]
    InvalidSampleColumn(String),
}
