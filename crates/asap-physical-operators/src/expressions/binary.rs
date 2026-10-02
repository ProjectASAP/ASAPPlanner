//! Execution configuration for a binary kernel, including comparison evaluation mode.
use planner_types::pre_asap::{
    ArithmeticOpKind, CompareOpKind, PromQLVectorSetOpKind, VectorMatch,
};
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum BinaryOpKind {
    Arithmetic(ArithmeticOpKind),
    Compare(CompareOpKind),
    CompareBool(CompareOpKind),
    Set(PromQLVectorSetOpKind),
}
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BinaryOperator {
    pub kind: BinaryOpKind,
    pub vector_match: Option<VectorMatch>,
    pub checked_relative_division: bool,
    pub checked_finite_division: bool,
}
impl BinaryOperator {
    pub fn from_logical(operator: &planner_types::ir::BinaryOperator, return_bool: bool) -> Self {
        use planner_types::pre_asap::BinaryOpKind as L;
        Self {
            kind: match &operator.kind {
                L::Arithmetic(op) => BinaryOpKind::Arithmetic(op.clone()),
                L::Compare(op) if return_bool => BinaryOpKind::CompareBool(op.clone()),
                L::Compare(op) => BinaryOpKind::Compare(op.clone()),
                L::Set(op) => BinaryOpKind::Set(op.clone()),
            },
            vector_match: operator.vector_match.clone(),
            checked_relative_division: operator.checked_relative_division,
            checked_finite_division: operator.checked_finite_division,
        }
    }
}
