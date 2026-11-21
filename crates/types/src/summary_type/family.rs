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
                ExactKind::Counter,
                ExactParams::Counter
            )),
            Some(SummaryFamily::Exact(ExactKind::Counter))
        );
        assert_eq!(
            SummaryFamily::of(&FieldDataType::Plain(DataType::Float64)),
            None
        );
    }

    /// `rate` and `increase` share the `Counter` state, written
    /// `{"exact": "Counter"}`. The names each had before are refused, not
    /// read as it: a deployment that listed only `Increase` would otherwise
    /// rule out `rate` too without saying so.
    #[test]
    fn counter_family_is_written_counter_and_old_names_are_refused() {
        let counter = SummaryFamily::Exact(ExactKind::Counter);
        assert_eq!(
            serde_json::to_string(&counter).unwrap(),
            r#"{"exact":"Counter"}"#
        );
        for json in [r#"{"exact": "Rate"}"#, r#"{"exact": "Increase"}"#] {
            assert!(
                serde_json::from_str::<SummaryFamily>(json).is_err(),
                "{json}"
            );
        }
    }
}
