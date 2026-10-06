//! Query results are either an operator result or a standalone scalar expression.
//! The root discriminator is not an operator and never creates a dag node.
use super::{OperatorNode, ScalarExpr};
use serde::{Deserialize, Serialize};
use std::rc::Rc;
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum QueryRoot<C = Rc<OperatorNode>> {
    Operator(C),
    Scalar(ScalarExpr<C>),
}
impl From<Rc<OperatorNode>> for QueryRoot {
    fn from(node: Rc<OperatorNode>) -> Self {
        Self::Operator(node)
    }
}
impl QueryRoot {
    pub fn validate_structure(&self) -> Result<(), crate::ir::SchemaDerivationError> {
        match self {
            Self::Operator(node) => node.validate_structure(),
            Self::Scalar(expr) => {
                expr.scalar_type(&crate::pre_asap::Schema::default())?;
                for node in expr.operator_refs() {
                    node.validate_structure()?;
                }
                Ok(())
            }
        }
    }

    pub fn as_operator(&self) -> Option<&Rc<OperatorNode>> {
        match self {
            Self::Operator(node) => Some(node),
            Self::Scalar(_) => None,
        }
    }
}
