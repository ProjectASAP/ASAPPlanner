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
pub struct SummaryCoverage {
    /// Observation stream identity, including its time axis.
    pub source: String,
    /// Must equal the producing `SummaryAgg.input`.
    pub input: SummaryUpdate,
    /// Must equal the producing `SummaryAgg.reduction`.
    pub reduction: Reduction,
    /// Union of joint regions; never the Cartesian product of independent bounds.
    pub regions: Vec<CoverageRegion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageRegion {
    /// Half-open bounds on one canonical time axis, in milliseconds.
    pub start_ms: i64,
    pub end_ms: i64,
    /// Conjunction of non-null equality predicates; empty means unrestricted.
    pub population: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CoverageError {
    #[error("coverage requires an explicit source identity")]
    MissingIdentity,
    #[error("coverage interval must have start < end")]
    InvalidInterval,
    #[error("population dimension names cannot be empty")]
    InvalidPopulation,
    #[error("summary source, input or reduction differs")]
    IncompatibleInput,
    #[error("coverage overlap is not proven absent")]
    PossibleOverlap,
    #[error("coverage merge requires at least one input")]
    EmptyMerge,
    #[error("summary coverage requires state output")]
    NotState,
    #[error("coverage input/reduction disagrees with summary producer")]
    ProducerMismatch,
}

impl SummaryCoverage {
    pub fn validate(&self) -> Result<(), CoverageError> {
        if self.source.is_empty() {
            return Err(CoverageError::MissingIdentity);
        }
        for (index, region) in self.regions.iter().enumerate() {
            if region.start_ms >= region.end_ms {
                return Err(CoverageError::InvalidInterval);
            }
            if region.population.keys().any(String::is_empty) {
                return Err(CoverageError::InvalidPopulation);
            }
            if self.regions[..index]
                .iter()
                .any(|other| region.may_overlap(other))
            {
                return Err(CoverageError::PossibleOverlap);
            }
        }
        Ok(())
    }

    /// Every observation in a region is assumed to contribute once to the state.
    /// Compose once-per-observation summaries only when their joint regions are
    /// provably disjoint. Family merge capability and accuracy are separate checks.
    pub fn merge_disjoint(inputs: &[Self]) -> Result<Self, CoverageError> {
        let first = inputs.first().ok_or(CoverageError::EmptyMerge)?;
        let mut merged = first.clone();
        merged.regions.clear();
        for input in inputs {
            input.validate()?;
            if input.source != first.source
                || input.input != first.input
                || input.reduction != first.reduction
            {
                return Err(CoverageError::IncompatibleInput);
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
        let mut normalized: Vec<CoverageRegion> = Vec::new();
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
impl CoverageRegion {
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
