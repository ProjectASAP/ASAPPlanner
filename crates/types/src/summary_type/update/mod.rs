//! What a summary state is updated with, per row.

use serde::{Deserialize, Serialize};

use crate::ir::scalar::ColumnRef;

/// One summary-state update. `item` identifies a keyed item when the family
/// is keyed; `weight` is the observation/update expression applied to state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SummaryUpdate {
    pub item: Option<SummaryInputExpr>,
    pub weight: SummaryInputExpr,
}

impl SummaryUpdate {
    pub fn column(column: ColumnRef) -> Self {
        Self {
            item: None,
            weight: SummaryInputExpr::Column(column),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntityIdentity {
    PromqlLabelSet { excluding: Vec<ColumnRef> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SummaryInputExpr {
    Constant(f64),
    Column(ColumnRef),
    Tuple(Vec<SummaryInputExpr>),
    EntityIdentity(EntityIdentity),
}

#[cfg(test)]
mod tests;
