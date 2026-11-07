//! Scoped source contracts the accuracy rules consult.

/// An enforced domain for the true values of one operand of an exact
/// arithmetic operation, in every evaluation: they are finite and lie in
/// `[lower, upper]`. Plan selection uses it to bound a product or quotient
/// of estimates whose errors are not both relative. Sampled min/max
/// statistics are not a proof: the source or execution layer must enforce
/// it.
#[derive(Debug, Clone, PartialEq)]
pub struct OperandDomain {
    pub lower: f64,
    pub upper: f64,
}
