use super::*;
use ExecutionTiming::{IngestionTime, QueryTime};

/// `new` accepts every (τ, ℓ) except (IngestionTime, OneEvaluation), for
/// both media, and keeps what it was given.
#[test]
fn new_over_every_timing_and_lifetime() {
    for medium in [StorageMedium::Memory, StorageMedium::Disk] {
        for (timing, lifetime, valid) in [
            (QueryTime, Lifetime::None, true),
            (QueryTime, Lifetime::OneEvaluation(medium), true),
            (QueryTime, Lifetime::AcrossEvaluations(medium), true),
            (IngestionTime, Lifetime::None, true),
            (IngestionTime, Lifetime::OneEvaluation(medium), false),
            (IngestionTime, Lifetime::AcrossEvaluations(medium), true),
        ] {
            let properties = PhysicalProperties::new(timing, lifetime);
            match valid {
                true => {
                    let properties = properties.unwrap();
                    assert_eq!(
                        (properties.timing(), properties.lifetime()),
                        (timing, lifetime)
                    );
                    assert_eq!(properties.is_materialized(), lifetime != Lifetime::None);
                }
                false => assert_eq!(
                    properties,
                    Err(PhysicalPropertiesError::OneEvaluationAtIngestion)
                ),
            }
        }
    }
}
