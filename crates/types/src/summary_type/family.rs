//! A summary state's family without its parameters: the vocabulary
//! deployment exclusions (and a later calibration filter) match on.

use serde::{Deserialize, Serialize};

use super::{ExactKind, SketchAlgorithm};
use crate::ir::schema::FieldDataType;

/// The family of a summary state without its parameters: a sketch
/// algorithm or an exact accumulator kind. JSON: `{"sketch": "Kll"}`,
/// `{"exact": "Sum"}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryFamily {
    Sketch(SketchAlgorithm),
    Exact(ExactKind),
}

impl SummaryFamily {
    /// `Sketch(Kll)` for `FieldDataType::Sketch(Kll)`, `Exact(Sum)` for
    /// `ExactAggregate(Sum, _)`, `None` for any other field type.
    pub fn of(summary_type: &FieldDataType) -> Option<Self> {
        match summary_type {
            FieldDataType::Sketch(algorithm) => Some(Self::Sketch(*algorithm)),
            FieldDataType::ExactAggregate(kind, _) => Some(Self::Exact(*kind)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::schema::DataType;
    use crate::summary_type::ExactParams;

    /// A sketch state is its algorithm, an exact accumulator its kind, and
    /// a plain value has no family.
    #[test]
    fn summary_family_of_a_field_type() {
        assert_eq!(
            SummaryFamily::of(&FieldDataType::Sketch(SketchAlgorithm::Kll)),
            Some(SummaryFamily::Sketch(SketchAlgorithm::Kll))
        );
        assert_eq!(
            SummaryFamily::of(&FieldDataType::ExactAggregate(
                ExactKind::Sum,
                ExactParams::Sum
            )),
            Some(SummaryFamily::Exact(ExactKind::Sum))
        );
        assert_eq!(
            SummaryFamily::of(&FieldDataType::ExactAggregate(
                ExactKind::Rate,
                ExactParams::Rate
            )),
            Some(SummaryFamily::Exact(ExactKind::Rate))
        );
        assert_eq!(
            SummaryFamily::of(&FieldDataType::Plain(DataType::Float64)),
            None
        );
    }
}
