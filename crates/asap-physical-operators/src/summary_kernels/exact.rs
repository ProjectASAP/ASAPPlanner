//! Exact summary state identified by Planner family, independent of keyed layout.
use super::increase::IncreaseAccumulator;
use crate::Statistic;
use crate::{AggregateCore, KeyByLabelValues, Measurement};
use planner_types::post_asap::{ExactKind, ExactParams, SummaryFamilyType};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize)]
enum ScalarState {
    Sum(f64),
    Count(u64),
    Min(Option<f64>),
    Max(Option<f64>),
    Counter(Option<IncreaseAccumulator>),
}

/// Both the family and population layout survive persistence. Sharing counter
/// arithmetic never authorizes a Rate state to answer an Increase readout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExactAccumulator {
    family: SummaryFamilyType,
    scalar: ScalarState,
    keyed: Option<HashMap<KeyByLabelValues, ScalarState>>,
}

/// Planned readout of an exact summary. `lookback_ms` is the logical PromQL
/// counter window; the evaluation range is resolved from it at run time.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ExactReadout {
    pub statistic: Statistic,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lookback_ms: Option<i64>,
}

impl ExactAccumulator {
    /// Read one population. An empty MIN/MAX population reads as `None`.
    /// `range_ms` extrapolates a counter Rate/Increase to that evaluation range.
    pub fn readout(
        &self,
        statistic: Statistic,
        range_ms: Option<(i64, i64)>,
        key: Option<&KeyByLabelValues>,
    ) -> Result<Option<f64>, Error> {
        if statistic != self.statistic() {
            return Err("readout differs from Planner exact family".into());
        }
        let state = match (&self.keyed, key) {
            (Some(states), Some(key)) => states.get(key).ok_or("unknown exact population")?,
            (None, None) => &self.scalar,
            _ => return Err("readout population differs from installed layout".into()),
        };
        match state {
            ScalarState::Sum(sum) => Ok(Some(*sum)),
            ScalarState::Count(count) => Ok(Some(*count as f64)),
            ScalarState::Min(value) | ScalarState::Max(value) => Ok(*value),
            ScalarState::Counter(Some(counter)) => counter
                .extrapolated_value(range_ms, statistic == Statistic::Rate)
                .map(Some),
            ScalarState::Counter(None) => Err("empty counter population".into()),
        }
    }

    /// Exact integer count of an unkeyed Count state.
    pub fn count(&self) -> Option<u64> {
        match (&self.keyed, &self.scalar) {
            (None, ScalarState::Count(count)) => Some(*count),
            _ => None,
        }
    }

    /// Accumulate into run-local scratch state. Persistent input states remain
    /// immutable; a failed merge discards this scratch state.
    pub(crate) fn merge_from(&mut self, other: &Self) -> Result<(), Error> {
        if self.family != other.family || self.is_keyed() != other.is_keyed() {
            return Err("cannot merge different Planner families or layouts".into());
        }
        if let (Some(target), Some(source)) = (&mut self.keyed, &other.keyed) {
            for (key, state) in source {
                let combined = match target.get(key) {
                    Some(old) => merge_scalar(old, state)?,
                    None => state.clone(),
                };
                target.insert(key.clone(), combined);
            }
        } else {
            self.scalar = merge_scalar(&self.scalar, &other.scalar)?;
        }
        Ok(())
    }

    pub fn new(family: SummaryFamilyType, keyed: bool) -> Result<Self, String> {
        use ExactKind as K;
        use ExactParams as P;
        let scalar = match &family {
            SummaryFamilyType::ExactAggregate(K::Sum, P::Sum) => ScalarState::Sum(0.0),
            SummaryFamilyType::ExactAggregate(K::Count, P::Count) => ScalarState::Count(0),
            SummaryFamilyType::ExactAggregate(K::Min, P::Min) => ScalarState::Min(None),
            SummaryFamilyType::ExactAggregate(K::Max, P::Max) => ScalarState::Max(None),
            SummaryFamilyType::ExactAggregate(K::Rate, P::Rate)
            | SummaryFamilyType::ExactAggregate(K::Increase, P::Increase) => {
                ScalarState::Counter(None)
            }
            _ => return Err(format!("unsupported exact Planner family: {family:?}")),
        };
        Ok(Self {
            family,
            scalar,
            keyed: keyed.then(HashMap::new),
        })
    }

    pub fn family(&self) -> &SummaryFamilyType {
        &self.family
    }
    pub(crate) fn insufficient_counter_samples(
        &self,
        statistic: Statistic,
        key: &Option<KeyByLabelValues>,
    ) -> bool {
        if statistic != self.statistic() {
            return false;
        }
        let state = match (&self.keyed, key) {
            (Some(states), Some(key)) => states.get(key),
            (None, None) => Some(&self.scalar),
            _ => None,
        };
        match state {
            Some(ScalarState::Counter(None)) => true,
            Some(ScalarState::Counter(Some(counter))) => {
                counter.sample_count < 2
                    || counter.last_seen_timestamp == counter.starting_timestamp
            }
            _ => false,
        }
    }
    pub fn is_keyed(&self) -> bool {
        self.keyed.is_some()
    }

    pub fn update(&mut self, key: Option<&KeyByLabelValues>, value: f64, timestamp: i64) {
        let state = match (&mut self.keyed, key) {
            (Some(states), Some(key)) => states
                .entry(key.clone())
                .or_insert_with(|| self.scalar.clone()),
            (None, None) => &mut self.scalar,
            _ => panic!("exact update population layout differs from installed DAG"),
        };
        match state {
            ScalarState::Sum(sum) => *sum += value,
            ScalarState::Count(count) => {
                *count = count.checked_add(1).expect("exact count overflow")
            }
            ScalarState::Min(current) => {
                *current = Some(current.map_or(value, |old| old.min(value)))
            }
            ScalarState::Max(current) => {
                *current = Some(current.map_or(value, |old| old.max(value)))
            }
            ScalarState::Counter(current) => match current {
                Some(counter) => counter.update(Measurement::new(value), timestamp),
                None => {
                    *current = Some(IncreaseAccumulator::new(
                        Measurement::new(value),
                        timestamp,
                        Measurement::new(value),
                        timestamp,
                    ))
                }
            },
        }
    }

    fn statistic(&self) -> Statistic {
        match self.family {
            SummaryFamilyType::ExactAggregate(ExactKind::Sum, _) => Statistic::Sum,
            SummaryFamilyType::ExactAggregate(ExactKind::Count, _) => Statistic::Count,
            SummaryFamilyType::ExactAggregate(ExactKind::Min, _) => Statistic::Min,
            SummaryFamilyType::ExactAggregate(ExactKind::Max, _) => Statistic::Max,
            SummaryFamilyType::ExactAggregate(ExactKind::Rate, _) => Statistic::Rate,
            SummaryFamilyType::ExactAggregate(ExactKind::Increase, _) => Statistic::Increase,
            _ => unreachable!("validated exact family"),
        }
    }
}

fn merge_scalar(left: &ScalarState, right: &ScalarState) -> Result<ScalarState, Error> {
    Ok(match (left, right) {
        (ScalarState::Sum(a), ScalarState::Sum(b)) => ScalarState::Sum(a + b),
        (ScalarState::Count(a), ScalarState::Count(b)) => {
            ScalarState::Count(a.checked_add(*b).ok_or("exact count overflow")?)
        }
        (ScalarState::Min(a), ScalarState::Min(b)) => {
            ScalarState::Min(a.iter().chain(b).copied().reduce(f64::min))
        }
        (ScalarState::Max(a), ScalarState::Max(b)) => {
            ScalarState::Max(a.iter().chain(b).copied().reduce(f64::max))
        }
        (ScalarState::Counter(a), ScalarState::Counter(b)) => ScalarState::Counter(match (a, b) {
            (Some(a), Some(b)) => Some(IncreaseAccumulator::merge_pair(a, b)),
            (a, b) => a.clone().or_else(|| b.clone()),
        }),
        _ => return Err("exact scalar state families differ".into()),
    })
}

impl AggregateCore for ExactAccumulator {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn merge_with(&self, other: &dyn AggregateCore) -> Result<Box<dyn AggregateCore>, Error> {
        let other = other
            .as_any()
            .downcast_ref::<Self>()
            .ok_or("merge requires Planner exact state")?;
        let mut merged = self.clone();
        merged.merge_from(other)?;
        Ok(Box::new(merged))
    }
    fn approx_memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.keyed.as_ref().map_or(0, |m| {
                m.keys()
                    .map(|k| {
                        std::mem::size_of::<ScalarState>()
                            + k.labels.iter().map(String::len).sum::<usize>()
                    })
                    .sum::<usize>()
            })
    }
}
