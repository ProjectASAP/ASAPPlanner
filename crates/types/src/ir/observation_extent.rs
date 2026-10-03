//! Joint time/population coverage for summary composition, independent of schema.
//! Equality predicates are a deliberately narrow proof vocabulary. Unsupported
//! predicates cannot be declared disjoint merely by giving them different names.
use super::operator_properties::Reduction;
use crate::post_asap::SummaryUpdate;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationExtent {
    pub source: String,
    pub revision: String,
    pub input: SummaryUpdate,
    pub grouping: Reduction,
    pub multiplicity: ObservationMultiplicity,
    /// Union of joint regions; never the Cartesian product of independent bounds.
    pub regions: Vec<ExtentRegion>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObservationMultiplicity {
    OncePerObservation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtentRegion {
    /// Half-open bounds on one canonical time axis, in milliseconds.
    pub start_ms: i64,
    pub end_ms: i64,
    /// Conjunction of non-null equality predicates; empty means unrestricted.
    pub population: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ExtentError {
    #[error("coverage requires explicit source and revision identity")]
    MissingIdentity,
    #[error("coverage interval must have start < end")]
    InvalidInterval,
    #[error("population dimension names cannot be empty")]
    InvalidPopulation,
    #[error("summary input identity, grouping or multiplicity differs")]
    IncompatibleInput,
    #[error("coverage overlap is not proven absent")]
    PossibleOverlap,
    #[error("coverage merge requires at least one input")]
    EmptyMerge,
}

impl ObservationExtent {
    pub fn validate(&self) -> Result<(), ExtentError> {
        if self.source.is_empty() || self.revision.is_empty() {
            return Err(ExtentError::MissingIdentity);
        }
        for (index, region) in self.regions.iter().enumerate() {
            if region.start_ms >= region.end_ms {
                return Err(ExtentError::InvalidInterval);
            }
            if region.population.keys().any(String::is_empty) {
                return Err(ExtentError::InvalidPopulation);
            }
            if self.regions[..index]
                .iter()
                .any(|other| region.may_overlap(other))
            {
                return Err(ExtentError::PossibleOverlap);
            }
        }
        Ok(())
    }

    /// Compose once-per-observation summaries only when their joint regions are
    /// provably disjoint. Family merge capability and accuracy are separate checks.
    pub fn merge_disjoint(inputs: &[Self]) -> Result<Self, ExtentError> {
        let first = inputs.first().ok_or(ExtentError::EmptyMerge)?;
        let mut merged = first.clone();
        merged.regions.clear();
        for input in inputs {
            input.validate()?;
            if input.source != first.source
                || input.revision != first.revision
                || input.input != first.input
                || input.grouping != first.grouping
                || input.multiplicity != first.multiplicity
            {
                return Err(ExtentError::IncompatibleInput);
            }
            merged.regions.extend(input.regions.iter().cloned());
        }
        merged.validate()?;
        // Coalesce adjacent intervals only for identical population predicates.
        merged.regions.sort_by(|a, b| {
            a.population
                .cmp(&b.population)
                .then(a.start_ms.cmp(&b.start_ms))
        });
        let mut normalized: Vec<ExtentRegion> = Vec::new();
        for region in merged.regions {
            if let Some(last) = normalized.last_mut() {
                if last.population == region.population && last.end_ms == region.start_ms {
                    last.end_ms = region.end_ms;
                    continue;
                }
            }
            normalized.push(region);
        }
        merged.regions = normalized;
        Ok(merged)
    }
}
impl ExtentRegion {
    fn may_overlap(&self, other: &Self) -> bool {
        self.start_ms < other.end_ms
            && other.start_ms < self.end_ms
            && !self.population.iter().any(|(dimension, value)| {
                other
                    .population
                    .get(dimension)
                    .is_some_and(|other| other != value)
            })
    }
}
