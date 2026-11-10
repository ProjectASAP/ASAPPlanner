//! Column lineage: where each output column's values come from, a source
//! column they are copied from or none ([`ColumnLineage`]). The memo derives
//! it once per group (`GroupProperties::column_lineage`); aggregate
//! coverage names a selection's column by it
//! ([`super::aggregate_coverage`]).

use serde::{Deserialize, Serialize};

use crate::ir::operator::arguments::SourceColumn;

/// Where an output column's values come from. It names a column across
/// groups where a schema field's name cannot: two join sides' columns of
/// one name have different lineage, and a renamed column keeps its lineage.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ColumnLineage {
    /// Copied unchanged from a column of a source.
    SourceColumn(SourceColumn),
    /// Computed by an operator: a measure, an expression, a summary state or
    /// a readout.
    Computed,
}
