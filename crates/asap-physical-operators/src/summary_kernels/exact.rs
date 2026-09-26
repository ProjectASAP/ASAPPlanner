//! Exact summary state identified by Planner family, independent of keyed layout.
use super::increase::IncreaseAccumulator;
use crate::Statistic;
use crate::{
    AggregateCore, AggregationType, AuxStats, KeyByLabelValues, Measurement, SerializableToSink,
};
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

impl ExactAccumulator {
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

    pub fn deserialize_from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let state: Self = rmp_serde::from_slice(bytes)?;
        let expected = Self::new(state.family.clone(), state.is_keyed())?;
        let same_variant = |value: &ScalarState| {
            std::mem::discriminant(value) == std::mem::discriminant(&expected.scalar)
        };
        if !same_variant(&state.scalar)
            || state
                .keyed
                .as_ref()
                .is_some_and(|states| states.values().any(|s| !same_variant(s)))
        {
            return Err("exact payload differs from declared Planner family".into());
        }
        Ok(state)
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
            (Some(a), Some(b)) => Some(
                <IncreaseAccumulator as crate::MergeableAccumulator<_>>::merge_accumulators(vec![
                    a.clone(),
                    b.clone(),
                ])?,
            ),
            (a, b) => a.clone().or_else(|| b.clone()),
        }),
        _ => return Err("exact scalar state families differ".into()),
    })
}

impl SerializableToSink for ExactAccumulator {
    fn serialize_to_json(&self) -> serde_json::Value {
        serde_json::json!({"family": self.family, "scalar": self.scalar, "keyed": self.keyed.as_ref().map(|m|m.iter().collect::<Vec<_>>())})
    }
    fn serialize_to_bytes(&self) -> Vec<u8> {
        rmp_serde::to_vec_named(self).expect("exact state encoding")
    }
}

impl AggregateCore for ExactAccumulator {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }
    fn type_name(&self) -> &'static str {
        "PlannerExactAccumulatorV1"
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn merge_with(&self, other: &dyn AggregateCore) -> Result<Box<dyn AggregateCore>, Error> {
        let other = other
            .as_any()
            .downcast_ref::<Self>()
            .ok_or("merge requires Planner exact state")?;
        if self.family != other.family || self.is_keyed() != other.is_keyed() {
            return Err("cannot merge different Planner families or layouts".into());
        }
        let mut merged = self.clone();
        if let (Some(target), Some(source)) = (&mut merged.keyed, &other.keyed) {
            for (key, state) in source {
                let combined = match target.get(key) {
                    Some(old) => merge_scalar(old, state)?,
                    None => state.clone(),
                };
                target.insert(key.clone(), combined);
            }
        } else {
            merged.scalar = merge_scalar(&self.scalar, &other.scalar)?;
        }
        Ok(Box::new(merged))
    }
    fn get_accumulator_type(&self) -> AggregationType {
        match self.statistic() {
            Statistic::Sum => AggregationType::Sum,
            Statistic::Count => AggregationType::Count,
            Statistic::Min => AggregationType::Min,
            Statistic::Max => AggregationType::Max,
            Statistic::Rate => AggregationType::Rate,
            Statistic::Increase => AggregationType::Increase,
            _ => unreachable!(),
        }
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
    fn aux_stats(&self) -> AuxStats {
        if self.is_keyed() {
            return AuxStats::empty();
        }
        match self.scalar {
            ScalarState::Sum(value) => AuxStats {
                sum: Some(value),
                ..AuxStats::empty()
            },
            ScalarState::Count(value) => AuxStats {
                count: Some(value),
                ..AuxStats::empty()
            },
            ScalarState::Min(value) => AuxStats {
                min: value,
                ..AuxStats::empty()
            },
            ScalarState::Max(value) => AuxStats {
                max: value,
                ..AuxStats::empty()
            },
            ScalarState::Counter(_) => AuxStats::empty(),
        }
    }
    fn get_keys(&self) -> Option<Vec<KeyByLabelValues>> {
        self.keyed.as_ref().map(|m| m.keys().cloned().collect())
    }
    fn query_statistic(
        &self,
        statistic: Statistic,
        key: &Option<KeyByLabelValues>,
        kwargs: &HashMap<String, String>,
    ) -> Result<f64, Error> {
        if statistic != self.statistic() {
            return Err("readout differs from Planner exact family".into());
        }
        let state = match (&self.keyed, key) {
            (Some(states), Some(key)) => states.get(key).ok_or("unknown exact population")?,
            (None, None) => &self.scalar,
            _ => return Err("readout population differs from installed layout".into()),
        };
        match state {
            ScalarState::Sum(sum) => Ok(*sum),
            ScalarState::Count(count) => Ok(*count as f64),
            ScalarState::Min(value) | ScalarState::Max(value) => {
                value.ok_or_else(|| "empty exact population".into())
            }
            ScalarState::Counter(Some(counter)) => {
                counter.query_statistic(statistic, &None, kwargs)
            }
            ScalarState::Counter(None) => Err("empty counter population".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Identity, population isolation, and readout survive the persisted format.
    #[test]
    fn exact_families_roundtrip_and_reject_cross_family_operations() {
        let families = [
            (ExactKind::Sum, ExactParams::Sum, Statistic::Sum, 16.0),
            (ExactKind::Count, ExactParams::Count, Statistic::Count, 3.0),
            (ExactKind::Min, ExactParams::Min, Statistic::Min, 2.0),
            (ExactKind::Max, ExactParams::Max, Statistic::Max, 8.0),
            (ExactKind::Rate, ExactParams::Rate, Statistic::Rate, 3.0),
            (
                ExactKind::Increase,
                ExactParams::Increase,
                Statistic::Increase,
                6.0,
            ),
        ];
        for keyed in [false, true] {
            let key = keyed.then(|| KeyByLabelValues::new_with_labels(vec!["a".into()]));
            let mut states = Vec::new();
            for (kind, params, stat, value) in &families {
                let mut state = ExactAccumulator::new(
                    SummaryFamilyType::ExactAggregate(kind.clone(), params.clone()),
                    keyed,
                )
                .unwrap();
                for (ts, v) in [(1000, 8.0), (2000, 2.0), (3000, 6.0)] {
                    state.update(key.as_ref(), v, ts);
                }
                let restored =
                    ExactAccumulator::deserialize_from_bytes(&state.serialize_to_bytes()).unwrap();
                assert_eq!(restored.family(), state.family());
                assert_eq!(
                    restored
                        .query_statistic(*stat, &key, &HashMap::new())
                        .unwrap(),
                    *value
                );
                for (_, _, wrong, _) in &families {
                    if wrong != stat {
                        assert!(restored
                            .query_statistic(*wrong, &key, &HashMap::new())
                            .is_err());
                    }
                }
                states.push(restored);
            }
            for (i, a) in states.iter().enumerate() {
                for (j, b) in states.iter().enumerate() {
                    assert_eq!(a.merge_with(b).is_ok(), i == j);
                }
            }
        }
    }
}
