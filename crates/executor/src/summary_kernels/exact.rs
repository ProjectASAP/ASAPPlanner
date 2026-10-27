//! Exact summary state identified by Planner family, independent of keyed layout.
use super::increase::IncreaseAccumulator;
use crate::AggregateCore;
use planner_types::ir::schema::{ExactKind, ExactParams, FieldDataType as SummaryFamilyType};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

type Error = Box<dyn std::error::Error + Send + Sync>;

/// One population's exact state: a fixed-size numeric record with no heap
/// allocation, so a column of them is one contiguous buffer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum ScalarState {
    Sum {
        sum: f64,
        compensation: f64,
        seen: bool,
    },
    Count(u64),
    Min(Option<f64>),
    Max(Option<f64>),
    Counter(Option<IncreaseAccumulator>),
}

/// Both the family and population layout survive persistence. Sharing counter
/// arithmetic never authorizes a Rate state to answer an Increase evaluation.
///
/// Deserialization validates the payload against its declared family, so
/// deployments can persist this state with any serde format without mirroring
/// its shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "ExactPayload")]
pub struct ExactAccumulator {
    family: SummaryFamilyType,
    scalar: ScalarState,
    keyed: Option<HashMap<Vec<String>, ScalarState>>,
}

#[derive(Deserialize)]
struct ExactPayload {
    family: SummaryFamilyType,
    scalar: ScalarState,
    keyed: Option<HashMap<Vec<String>, ScalarState>>,
}

impl TryFrom<ExactPayload> for ExactAccumulator {
    type Error = String;

    fn try_from(payload: ExactPayload) -> Result<Self, String> {
        let empty = Self::new(payload.family, payload.keyed.is_some())?;
        let same_kind = |state: &ScalarState| {
            std::mem::discriminant(state) == std::mem::discriminant(&empty.scalar)
        };
        if !same_kind(&payload.scalar)
            || payload
                .keyed
                .iter()
                .flat_map(HashMap::values)
                .any(|s| !same_kind(s))
        {
            return Err("exact payload differs from its declared Planner family".into());
        }
        Ok(Self {
            scalar: payload.scalar,
            keyed: payload.keyed,
            ..empty
        })
    }
}

/// Planned evaluation of an exact summary. `lookback_ms` is the logical PromQL
/// counter window, ending `offset_ms` before the evaluation time (PromQL
/// `offset`); the evaluation range is resolved from them at run time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExactEvaluation {
    pub statistic: ExactKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lookback_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub offset_ms: i64,
}

fn is_zero(ms: &i64) -> bool {
    *ms == 0
}

impl ExactAccumulator {
    /// Read one population. An empty MIN/MAX population reads as `None`.
    /// `range_ms` extrapolates a counter Rate/Increase to that evaluation range.
    pub fn evaluation(
        &self,
        statistic: &ExactKind,
        range_ms: Option<(i64, i64)>,
        key: Option<&[String]>,
    ) -> Result<Option<f64>, Error> {
        if statistic != self.statistic() {
            return Err("evaluation differs from Planner exact family".into());
        }
        let state = match (&self.keyed, key) {
            (Some(states), Some(key)) => states.get(key).ok_or("unknown exact population")?,
            (None, None) => &self.scalar,
            _ => return Err("evaluation population differs from installed layout".into()),
        };
        state.evaluation(statistic, range_ms)
    }

    /// The earliest sample time of a counter (Rate/Increase) state, over
    /// all its populations; `None` for an empty or non-counter state.
    /// Counter states merge pairwise in time order, so a merge of panes
    /// orders them by this first.
    pub fn earliest_sample_ms(&self) -> Option<i64> {
        std::iter::once(&self.scalar)
            .chain(self.keyed.iter().flat_map(HashMap::values))
            .filter_map(ScalarState::earliest_sample_ms)
            .min()
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
            SummaryFamilyType::ExactAggregate(K::Sum, P::Sum) => ScalarState::Sum {
                sum: 0.0,
                compensation: 0.0,
                seen: false,
            },
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
    pub(crate) fn insufficient_counter_raw_data_samples(
        &self,
        statistic: &ExactKind,
        key: Option<&[String]>,
    ) -> bool {
        if statistic != self.statistic() {
            return false;
        }
        let state = match (&self.keyed, key) {
            (Some(states), Some(key)) => states.get(key),
            (None, None) => Some(&self.scalar),
            _ => None,
        };
        state.is_some_and(ScalarState::insufficient_counter_raw_data_samples)
    }
    pub fn is_keyed(&self) -> bool {
        self.keyed.is_some()
    }

    pub fn update(&mut self, key: Option<&[String]>, value: f64, timestamp: i64) {
        let state = match (&mut self.keyed, key) {
            (Some(states), Some(key)) => states
                .entry(key.to_vec())
                .or_insert_with(|| self.scalar.clone()),
            (None, None) => &mut self.scalar,
            _ => panic!("exact update population layout differs from installed DAG"),
        };
        state.update(value, timestamp);
    }

    /// The unkeyed state of `family` holding `scalar`, read back from a state
    /// column.
    pub(crate) fn from_scalar(family: SummaryFamilyType, scalar: ScalarState) -> Self {
        Self {
            family,
            scalar,
            keyed: None,
        }
    }

    /// The unkeyed population's state; `None` for a keyed state.
    pub(crate) fn scalar(&self) -> Option<&ScalarState> {
        self.keyed.is_none().then_some(&self.scalar)
    }

    fn statistic(&self) -> &ExactKind {
        statistic_of(&self.family)
    }
}

/// The statistic an exact `family` answers: its [`ExactKind`].
pub(crate) fn statistic_of(family: &SummaryFamilyType) -> &ExactKind {
    match family {
        SummaryFamilyType::ExactAggregate(kind, _) => kind,
        _ => unreachable!("validated exact family"),
    }
}

impl ScalarState {
    /// The empty state of an exact `family`.
    pub(crate) fn empty(family: &SummaryFamilyType) -> Result<Self, String> {
        Ok(ExactAccumulator::new(family.clone(), false)?.scalar)
    }

    /// Add one sample: `value` observed at `timestamp_ms`.
    pub(crate) fn update(&mut self, value: f64, timestamp_ms: i64) {
        match self {
            ScalarState::Sum {
                sum,
                compensation,
                seen,
            } => {
                compensated_add(sum, compensation, value);
                *seen = true;
            }
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
                Some(counter) => counter.update(value, timestamp_ms),
                None => {
                    *current = Some(IncreaseAccumulator::new(
                        value,
                        timestamp_ms,
                        value,
                        timestamp_ms,
                    ))
                }
            },
        }
    }

    /// The population's value for `statistic`; `range_ms` extrapolates a
    /// counter. An empty MIN/MAX reads as `None`.
    pub(crate) fn evaluation(
        &self,
        statistic: &ExactKind,
        range_ms: Option<(i64, i64)>,
    ) -> Result<Option<f64>, Error> {
        match self {
            ScalarState::Sum {
                sum, compensation, ..
            } => Ok(Some(sum + compensation)),
            ScalarState::Count(count) => Ok(Some(*count as f64)),
            ScalarState::Min(value) | ScalarState::Max(value) => Ok(*value),
            ScalarState::Counter(Some(counter)) => counter
                .extrapolated_value(range_ms, *statistic == ExactKind::Rate)
                .map(Some),
            ScalarState::Counter(None) => Err("empty counter population".into()),
        }
    }

    /// Whether a counter has fewer than two ordered samples (PromQL omits it).
    pub(crate) fn insufficient_counter_raw_data_samples(&self) -> bool {
        match self {
            ScalarState::Counter(None) => true,
            ScalarState::Counter(Some(counter)) => {
                counter.sample_count < 2
                    || counter.last_seen_timestamp == counter.starting_timestamp
            }
            _ => false,
        }
    }

    /// The earliest sample time of a counter; `None` otherwise.
    pub(crate) fn earliest_sample_ms(&self) -> Option<i64> {
        match self {
            ScalarState::Counter(Some(counter)) => Some(counter.starting_timestamp),
            _ => None,
        }
    }

    /// Whether two counter states' sample intervals overlap (share any time
    /// from first to last sample, ends included); false for other states
    /// and for an empty counter.
    pub(crate) fn overlaps_in_time(&self, other: &Self) -> bool {
        match (self, other) {
            (ScalarState::Counter(Some(a)), ScalarState::Counter(Some(b))) => {
                a.starting_timestamp <= b.last_seen_timestamp
                    && b.starting_timestamp <= a.last_seen_timestamp
            }
            _ => false,
        }
    }

    /// Whether a SUM saw no value (SQL reads it as NULL).
    pub(crate) fn is_empty_sum(&self) -> bool {
        matches!(self, ScalarState::Sum { seen: false, .. })
    }

    /// The exact count of a COUNT state.
    pub(crate) fn count(&self) -> Option<u64> {
        match self {
            ScalarState::Count(count) => Some(*count),
            _ => None,
        }
    }

    /// The merge of two states of one family.
    pub(crate) fn merge(&self, other: &Self) -> Result<Self, Error> {
        merge_scalar(self, other)
    }
}

// Kahan-Neumaier addition, clearing the correction when the running sum is infinite.
fn compensated_add(sum: &mut f64, compensation: &mut f64, value: f64) {
    let next = *sum + value;
    *compensation = if next.is_infinite() {
        0.0
    } else if sum.abs() >= value.abs() {
        *compensation + ((*sum - next) + value)
    } else {
        *compensation + ((value - next) + *sum)
    };
    *sum = next;
}

fn merge_scalar(left: &ScalarState, right: &ScalarState) -> Result<ScalarState, Error> {
    Ok(match (left, right) {
        (
            ScalarState::Sum {
                sum: a,
                compensation: ac,
                seen: a_seen,
            },
            ScalarState::Sum {
                sum: b,
                compensation: bc,
                seen: b_seen,
            },
        ) => {
            let (mut sum, mut compensation) = (*a, *ac);
            compensated_add(&mut sum, &mut compensation, *b);
            compensated_add(&mut sum, &mut compensation, *bc);
            ScalarState::Sum {
                sum,
                compensation,
                seen: *a_seen || *b_seen,
            }
        }
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
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
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
                            + k.iter().map(String::len).sum::<usize>()
                    })
                    .sum::<usize>()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Serialize)]
    struct Payload {
        family: SummaryFamilyType,
        scalar: ScalarState,
        keyed: Option<HashMap<Vec<String>, ScalarState>>,
    }

    /// The planner prices one exact state at the bytes a column of them
    /// holds per group: one `ScalarState`.
    #[test]
    fn the_planner_prices_an_exact_state_at_its_size() {
        assert_eq!(
            std::mem::size_of::<ScalarState>() as u64,
            planner_types::ir::schema::EXACT_ACCUMULATOR_STATE_BYTES
        );
    }

    fn decode(payload: &Payload) -> Result<ExactAccumulator, rmp_serde::decode::Error> {
        rmp_serde::from_slice(&rmp_serde::to_vec_named(payload).unwrap())
    }

    fn sum() -> SummaryFamilyType {
        SummaryFamilyType::ExactAggregate(ExactKind::Sum, ExactParams::Sum)
    }

    // Stored Sum preserves low-order increments across updates, persistence and pane merge.
    #[test]
    fn stored_sum_is_compensated() {
        let mut state = ExactAccumulator::new(sum(), false).unwrap();
        for value in [1e16, 1.0] {
            state.update(None, value, 0);
        }
        let bytes = rmp_serde::to_vec_named(&state).unwrap();
        let mut restored: ExactAccumulator = rmp_serde::from_slice(&bytes).unwrap();
        let mut negative = ExactAccumulator::new(sum(), false).unwrap();
        negative.update(None, -1e16, 1);
        restored.merge_from(&negative).unwrap();
        assert_eq!(
            restored.evaluation(&ExactKind::Sum, None, None).unwrap(),
            Some(1.0)
        );
    }

    // Infinite running sums retain IEEE behavior without a spurious NaN correction.
    #[test]
    fn compensated_sum_retains_infinity_rules() {
        let mut state = ExactAccumulator::new(sum(), false).unwrap();
        state.update(None, f64::INFINITY, 0);
        state.update(None, 1.0, 0);
        assert_eq!(
            state.evaluation(&ExactKind::Sum, None, None).unwrap(),
            Some(f64::INFINITY)
        );
        state.update(None, f64::NEG_INFINITY, 0);
        assert!(state
            .evaluation(&ExactKind::Sum, None, None)
            .unwrap()
            .unwrap()
            .is_nan());
    }

    // A persisted exact state decodes back to the same family, layout and evaluation.
    #[test]
    fn serialized_state_round_trips() {
        let mut state = ExactAccumulator::new(sum(), true).unwrap();
        let key = ["a".to_string()];
        state.update(Some(&key), 2.5, 0);
        let bytes = rmp_serde::to_vec_named(&state).unwrap();
        let restored: ExactAccumulator = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(restored.family(), &sum());
        assert_eq!(
            restored
                .evaluation(&ExactKind::Sum, None, Some(&key))
                .unwrap(),
            Some(2.5)
        );
    }

    // Decoding rejects population states that differ from the declared family.
    #[test]
    fn decode_rejects_state_of_another_family() {
        let scalar = Payload {
            family: sum(),
            scalar: ScalarState::Count(3),
            keyed: None,
        };
        assert!(decode(&scalar).is_err());
        let key = vec!["a".to_string()];
        let keyed = Payload {
            family: sum(),
            scalar: ScalarState::Sum {
                sum: 0.0,
                compensation: 0.0,
                seen: false,
            },
            keyed: Some(HashMap::from([(key, ScalarState::Max(Some(1.0)))])),
        };
        assert!(decode(&keyed).is_err());
    }

    // Decoding rejects families that have no exact Planner state.
    #[test]
    fn decode_rejects_unsupported_family() {
        let payload = Payload {
            family: SummaryFamilyType::ExactAggregate(ExactKind::Sum, ExactParams::Count),
            scalar: ScalarState::Sum {
                sum: 0.0,
                compensation: 0.0,
                seen: false,
            },
            keyed: None,
        };
        assert!(decode(&payload).is_err());
    }
}
