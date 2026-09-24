//! Execution facts used to reject operators that cannot finish on their inputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Boundedness {
    /// The source or operator promises a finite result for this run.
    Bounded,
    Unbounded,
    /// No finite-input guarantee has been supplied.
    Unknown,
}
impl Boundedness {
    pub fn from_inputs(inputs: &[PlanProperties]) -> Self {
        if inputs.iter().any(|p| p.boundedness == Self::Unbounded) {
            Self::Unbounded
        } else if inputs.is_empty() || inputs.iter().any(|p| p.boundedness == Self::Unknown) {
            Self::Unknown
        } else {
            Self::Bounded
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Emission {
    Incremental,
    /// Produces its result only after all inputs end, even if accumulation is incremental.
    AfterInput,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlanProperties {
    pub boundedness: Boundedness,
    pub emission: Emission,
}
