//! Joint time/population coverage for summary composition, independent of schema.
//! Equality predicates are a deliberately narrow proof vocabulary. Unsupported
//! predicates cannot be declared disjoint merely by giving them different names.
use crate::ir::operator::Source;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ops::Range;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummaryCoverage {
    /// Observation data source, as named by `Scan`: a table or a time series.
    /// Region time bounds refer to its time column.
    pub source: Source,
    /// Union of joint regions; never the Cartesian product of independent bounds.
    pub regions: Vec<CoverageRegion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageRegion {
    /// Half-open bounds on the source's time column, in milliseconds. `None`
    /// means no time restriction, e.g. a source without a time column.
    pub time_ms: Option<CoverageTime>,
    /// Conjunction of non-null equality predicates; empty means unrestricted.
    pub population: BTreeMap<String, String>,
}

/// Half-open time bounds in milliseconds and what they are measured from.
/// Absolute and evaluation-relative bounds are incomparable: without an
/// evaluation time, no overlap between them can be ruled out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "CoverageTimeWire", into = "CoverageTimeWire")]
pub enum CoverageTime {
    /// Timestamps on the source's time column.
    Absolute(Range<i64>),
    /// Offsets from the evaluation time. Tumbling pane `i` of width `w`
    /// covers `-(i + 1) * w..-i * w`.
    RelativeToEvaluation(Range<i64>),
}

impl CoverageTime {
    pub fn range(&self) -> &Range<i64> {
        match self {
            Self::Absolute(range) | Self::RelativeToEvaluation(range) => range,
        }
    }

    fn is_relative(&self) -> bool {
        matches!(self, Self::RelativeToEvaluation(_))
    }
}

impl From<Range<i64>> for CoverageTime {
    fn from(range: Range<i64>) -> Self {
        Self::Absolute(range)
    }
}

/// Absolute time keeps the plain `{start, end}` encoding, so coverage written
/// before relative time existed still deserializes.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum CoverageTimeWire {
    Absolute(Range<i64>),
    Relative(RelativeTimeWire),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RelativeTimeWire {
    relative_to_evaluation: Range<i64>,
}

impl From<CoverageTimeWire> for CoverageTime {
    fn from(wire: CoverageTimeWire) -> Self {
        match wire {
            CoverageTimeWire::Absolute(range) => Self::Absolute(range),
            CoverageTimeWire::Relative(relative) => {
                Self::RelativeToEvaluation(relative.relative_to_evaluation)
            }
        }
    }
}

impl From<CoverageTime> for CoverageTimeWire {
    fn from(time: CoverageTime) -> Self {
        match time {
            CoverageTime::Absolute(range) => Self::Absolute(range),
            CoverageTime::RelativeToEvaluation(range) => Self::Relative(RelativeTimeWire {
                relative_to_evaluation: range,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CoverageError {
    #[error("coverage interval must have start < end")]
    InvalidInterval,
    #[error("population dimension names cannot be empty")]
    InvalidPopulation,
    #[error("summary coverage sources differ")]
    SourceMismatch,
    #[error(
        "coverage mixes absolute and evaluation-relative time, so disjointness cannot be proven"
    )]
    MixedTimeAnchors,
    #[error("coverage overlap is not proven absent")]
    PossibleOverlap,
    #[error("coverage merge requires at least one input")]
    EmptyMerge,
    #[error("summary coverage requires state output")]
    NotState,
    #[error("summary node requires coverage")]
    Missing,
    #[error("summary merge requires known coverage on every input")]
    UnknownInput,
    #[error("retained merge coverage disagrees with input union")]
    MergeOutputMismatch,
}

impl SummaryCoverage {
    pub fn validate(&self) -> Result<(), CoverageError> {
        let mut anchors = self
            .regions
            .iter()
            .filter_map(|region| region.time_ms.as_ref().map(CoverageTime::is_relative));
        if let Some(first) = anchors.next() {
            if anchors.any(|relative| relative != first) {
                return Err(CoverageError::MixedTimeAnchors);
            }
        }
        for (index, region) in self.regions.iter().enumerate() {
            if region
                .time_ms
                .as_ref()
                .is_some_and(|time| time.range().is_empty())
            {
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
    /// provably disjoint. Update/reduction compatibility, family merge capability
    /// and accuracy are checked by `SummaryMerge`, not here.
    pub fn merge_disjoint(inputs: &[Self]) -> Result<Self, CoverageError> {
        let first = inputs.first().ok_or(CoverageError::EmptyMerge)?;
        let mut merged = first.clone();
        merged.regions.clear();
        for input in inputs {
            input.validate()?;
            if input.source != first.source {
                return Err(CoverageError::SourceMismatch);
            }
            merged.regions.extend(input.regions.iter().cloned());
        }
        merged.validate()?;
        // Coalesce adjacent intervals only for identical population predicates;
        // `validate` has already ensured every bounded region shares one anchor.
        merged.regions.sort_by(|a, b| {
            a.population.cmp(&b.population).then(
                a.time_ms
                    .as_ref()
                    .map(|t| t.range().start)
                    .cmp(&b.time_ms.as_ref().map(|t| t.range().start)),
            )
        });
        let mut normalized: Vec<CoverageRegion> = Vec::new();
        for region in merged.regions {
            if let Some(last) = normalized.last_mut() {
                if let (
                    Some(
                        CoverageTime::Absolute(last_time)
                        | CoverageTime::RelativeToEvaluation(last_time),
                    ),
                    Some(time),
                ) = (&mut last.time_ms, &region.time_ms)
                {
                    if last.population == region.population && last_time.end == time.range().start {
                        last_time.end = time.range().end;
                        continue;
                    }
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
        let time_overlaps = match (&self.time_ms, &other.time_ms) {
            (Some(a), Some(b)) if a.is_relative() == b.is_relative() => {
                let (a, b) = (a.range(), b.range());
                a.start < b.end && b.start < a.end
            }
            _ => true,
        };
        time_overlaps
            && !self.population.iter().any(|(dimension, value)| {
                other
                    .population
                    .get(dimension)
                    .is_some_and(|other| other != value)
            })
    }
}
