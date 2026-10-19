//! Per-sample construction of a column of summary states, one state per
//! group: the code the `SummaryAgg` build runs row by row, exposed so a
//! runtime that ingests sample by sample builds the same states.
use super::exact::ScalarState;
use super::factory::{create_planner_accumulator, AccumulatorUpdater};
use super::weighted_frequency::WeightedFrequency;
use crate::values::{StateColumn, Value};
use crate::{AggregateCore, Error};
use planner_types::ir::scalar::ColumnRef;
use planner_types::ir::schema::{ExactKind, FieldDataType, GroupingStrategy, SummaryUpdate};
use std::sync::Arc;

/// The open summary states of one family, one per group, updated one sample
/// at a time; sealed into an immutable [`StateColumn`].
///
/// Built from the plan's family and parameters with [`Self::new`]. Groups are
/// numbered `0..group_count()` in the order [`Self::add_group`] adds them;
/// the caller keeps its own map from a group's labels to its number.
pub struct StateColumnUpdater {
    /// The family and parameters of every state.
    family: FieldDataType,
    /// The states, by group.
    states: GroupStates,
    /// For a counter (rate, increase), each group's latest sample time:
    /// samples must arrive in strictly increasing time within a group.
    latest_sample_ms: Option<Vec<Option<i64>>>,
    /// Memory the open states hold, kept up to date on every change.
    memory_bytes: usize,
}

/// The open states of a [`StateColumnUpdater`], by kind of family.
enum GroupStates {
    /// An exact accumulator per group, as a fixed-size numeric record.
    Exact {
        /// The state of a group that saw no sample.
        empty: ScalarState,
        /// The states, by group.
        states: Vec<ScalarState>,
    },
    /// A sketch per group, through its kernel's updater.
    Sketches(Vec<Box<dyn AccumulatorUpdater>>),
    /// A keyed frequency sketch (Count-Min or Count Sketch with a heap) per
    /// group.
    Keyed {
        /// An empty state of the family's shape, cloned for each new group.
        empty: WeightedFrequency,
        /// The states, by group.
        states: Vec<WeightedFrequency>,
    },
}

fn operator_error(message: impl ToString) -> Error {
    Error::Operator(message.to_string())
}

impl StateColumnUpdater {
    /// An updater with no group for `family`, the family and parameters of a
    /// `SummaryAgg` the plan builds one state per group of: an exact
    /// accumulator, a value sketch (KLL, DDSketch, HLL, KMV, UnivMon), or a
    /// keyed frequency sketch with a heap. Bad input for any other family,
    /// including a HydraCms family, whose groups share one state.
    pub fn new(family: &FieldDataType) -> Result<Self, Error> {
        crate::values::validate_family(family)?;
        let states = match family {
            FieldDataType::ExactAggregate(..) => GroupStates::Exact {
                empty: ScalarState::empty(family).map_err(Error::Invalid)?,
                states: Vec::new(),
            },
            FieldDataType::Sketch(kind, grouping)
                if grouping == &GroupingStrategy::PerSubpopulationInstance
                    && WeightedFrequency::configuration(kind).is_ok() =>
            {
                let (algorithm, width, depth, capacity) = WeightedFrequency::configuration(kind)?;
                GroupStates::Keyed {
                    empty: WeightedFrequency::new(algorithm, width, depth, capacity)?,
                    states: Vec::new(),
                }
            }
            FieldDataType::Sketch(..) => {
                // Fails for a family with no per-group value kernel.
                Self::sketch_updater(family)?;
                GroupStates::Sketches(Vec::new())
            }
            _ => return Err(Error::Invalid("unsupported summary family".into())),
        };
        let counter = matches!(
            family,
            FieldDataType::ExactAggregate(ExactKind::Rate | ExactKind::Increase, _)
        );
        Ok(Self {
            family: family.clone(),
            states,
            latest_sample_ms: counter.then(Vec::new),
            memory_bytes: 0,
        })
    }

    fn sketch_updater(family: &FieldDataType) -> Result<Box<dyn AccumulatorUpdater>, Error> {
        create_planner_accumulator(
            family,
            &SummaryUpdate::column(ColumnRef::SampleValue),
            &Default::default(),
        )
        .map_err(Error::Invalid)
    }

    /// The number of groups.
    pub fn group_count(&self) -> usize {
        match &self.states {
            GroupStates::Exact { states, .. } => states.len(),
            GroupStates::Sketches(states) => states.len(),
            GroupStates::Keyed { states, .. } => states.len(),
        }
    }

    /// Add groups with empty states until there are `group_count`.
    pub fn ensure_groups(&mut self, group_count: usize) -> Result<(), Error> {
        while self.group_count() < group_count {
            self.add_group()?;
        }
        Ok(())
    }

    /// Add a group with an empty state; returns its number.
    pub fn add_group(&mut self) -> Result<u32, Error> {
        let group = u32::try_from(self.group_count())
            .map_err(|_| Error::Invalid("more than u32::MAX groups".into()))?;
        self.memory_bytes += match &mut self.states {
            GroupStates::Exact { empty, states } => {
                // The buffer's growth: the column holds its capacity.
                let before = states.capacity();
                states.push(empty.clone());
                (states.capacity() - before) * std::mem::size_of::<ScalarState>()
            }
            GroupStates::Sketches(states) => {
                let state = Self::sketch_updater(&self.family)?;
                let bytes = state.memory_usage_bytes();
                states.push(state);
                bytes
            }
            GroupStates::Keyed { empty, states } => {
                states.push(empty.clone());
                empty.approx_memory_bytes()
            }
        };
        if let Some(latest) = &mut self.latest_sample_ms {
            latest.push(None);
        }
        Ok(group)
    }

    /// `group` as an index, or bad input when the updater has no such group.
    fn index(&self, group: u32) -> Result<usize, Error> {
        let index = group as usize;
        if index >= self.group_count() {
            return Err(Error::Invalid(format!(
                "group {group} was not added (the updater has {})",
                self.group_count()
            )));
        }
        Ok(index)
    }

    /// Add one sample to `group`'s state: `value` observed at
    /// `timestamp_ms` (read only by counters). Bad input for a group never
    /// added; an operator error when the state cannot take the value (not
    /// finite, outside a DDSketch's domain, a counter sample not after the
    /// group's previous one) or the family is keyed. The state is then
    /// unchanged.
    pub fn update(&mut self, group: u32, value: f64, timestamp_ms: i64) -> Result<(), Error> {
        self.update_value(group, &Value::Float64(value), timestamp_ms)
    }

    /// [`Self::update`] for a typed value: a value sketch over item values
    /// (UnivMon) also takes Utf8, Int64 and Bool values.
    pub(crate) fn update_value(
        &mut self,
        group: u32,
        value: &Value,
        timestamp_ms: i64,
    ) -> Result<(), Error> {
        let index = self.index(group)?;
        if let Some(latest) = &mut self.latest_sample_ms {
            if latest[index].is_some_and(|prior| timestamp_ms <= prior) {
                return Err(operator_error(
                    "counter samples must have strictly increasing timestamps within each group",
                ));
            }
        }
        match &mut self.states {
            GroupStates::Exact { states, .. } => {
                // The exact kernel's input contract (`AccumulatorUpdater`).
                let Value::Float64(value) = value else {
                    return Err(operator_error("summary update requires Float64"));
                };
                super::factory::finite_input(*value).map_err(operator_error)?;
                states[index].update(*value, timestamp_ms);
            }
            GroupStates::Sketches(states) => {
                let state = &mut states[index];
                let before = state.memory_usage_bytes();
                state
                    .update_value(value, timestamp_ms)
                    .map_err(operator_error)?;
                self.memory_bytes = self.memory_bytes - before + state.memory_usage_bytes();
            }
            GroupStates::Keyed { .. } => {
                return Err(operator_error(
                    "a keyed summary is updated with an item and a weight",
                ))
            }
        }
        if let Some(latest) = &mut self.latest_sample_ms {
            latest[index] = Some(timestamp_ms);
        }
        Ok(())
    }

    /// Add `weight` to the item tuple `items` in `group`'s keyed state (a
    /// Count-Min or Count Sketch with a heap). Bad input for a group never
    /// added; an operator error when the family is not keyed or the kernel
    /// refuses the weight.
    pub fn update_item(&mut self, group: u32, items: &[Value], weight: f64) -> Result<(), Error> {
        let index = self.index(group)?;
        let GroupStates::Keyed { states, .. } = &mut self.states else {
            return Err(operator_error("only a keyed summary takes items"));
        };
        let state = &mut states[index];
        let before = state.approx_memory_bytes();
        state.update(items, weight)?;
        self.memory_bytes = self.memory_bytes - before + state.approx_memory_bytes();
        Ok(())
    }

    /// Merge `other`, an updater of the same family whose groups have the
    /// same numbers (another shard of one pane), into this one, group by
    /// group; a group only `other` has is added.
    ///
    /// For a counter (rate, increase), each group's samples in the two
    /// shards must be disjoint in time: one shard's last sample before the
    /// other's first. Overlapping shards would count the increase of their
    /// overlap twice, so they are refused as bad input, as are different
    /// families. An operator error when a kernel refuses the merge. On an
    /// error this updater may hold some groups already merged.
    pub fn merge(&mut self, other: Self) -> Result<(), Error> {
        if self.family != other.family {
            return Err(Error::Invalid(
                "cannot merge different summary families".into(),
            ));
        }
        self.ensure_groups(other.group_count())?;
        match (&mut self.states, other.states) {
            (GroupStates::Exact { states, .. }, GroupStates::Exact { states: theirs, .. }) => {
                for (state, theirs) in states.iter_mut().zip(&theirs) {
                    if state.overlaps_in_time(theirs) {
                        return Err(Error::Invalid(
                            "counter shards overlap in time: a group's samples in one shard \
                             must all precede its samples in the other"
                                .into(),
                        ));
                    }
                    *state = state.merge(theirs).map_err(operator_error)?;
                }
            }
            (GroupStates::Sketches(states), GroupStates::Sketches(theirs)) => {
                for (state, theirs) in states.iter_mut().zip(theirs) {
                    let before = state.memory_usage_bytes();
                    state
                        .merge_accumulator(theirs.into_accumulator().as_ref())
                        .map_err(operator_error)?;
                    self.memory_bytes = self.memory_bytes - before + state.memory_usage_bytes();
                }
            }
            (GroupStates::Keyed { states, .. }, GroupStates::Keyed { states: theirs, .. }) => {
                for (state, theirs) in states.iter_mut().zip(&theirs) {
                    let before = state.approx_memory_bytes();
                    let mut merged = state.merge_with(theirs).map_err(operator_error)?;
                    std::mem::swap(
                        state,
                        merged
                            .as_any_mut()
                            .downcast_mut::<WeightedFrequency>()
                            .ok_or_else(|| operator_error("a merge changed the state's type"))?,
                    );
                    self.memory_bytes = self.memory_bytes - before + state.approx_memory_bytes();
                }
            }
            _ => unreachable!("one family has one kind of states"),
        }
        if let (Some(latest), Some(theirs)) = (&mut self.latest_sample_ms, other.latest_sample_ms) {
            for (latest, theirs) in latest.iter_mut().zip(theirs) {
                *latest = (*latest).max(theirs);
            }
        }
        Ok(())
    }

    /// Memory the open states hold, in bytes.
    pub fn memory_bytes(&self) -> usize {
        self.memory_bytes
    }

    /// The states as an immutable column, state `g` being group `g`'s.
    pub fn seal(self) -> StateColumn {
        match self.states {
            GroupStates::Exact { states, .. } => StateColumn::from_exact(self.family, states),
            GroupStates::Sketches(states) => StateColumn::from_objects(
                self.family,
                states
                    .into_iter()
                    .map(|state| Arc::from(state.into_accumulator()))
                    .collect(),
            ),
            GroupStates::Keyed { states, .. } => StateColumn::from_objects(
                self.family,
                states
                    .into_iter()
                    .map(|state| Arc::new(state) as Arc<dyn AggregateCore>)
                    .collect(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use planner_types::ir::schema::ExactParams;

    // An exact updater charges the buffer it holds (its capacity), the
    // same bytes its sealed column charges.
    #[test]
    fn exact_memory_is_the_buffer_capacity() {
        let family = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
        for groups in [1, 5, 17] {
            let mut updater = StateColumnUpdater::new(&family).unwrap();
            updater.ensure_groups(groups).unwrap();
            let charged = updater.memory_bytes();
            let GroupStates::Exact { states, .. } = &updater.states else {
                unreachable!()
            };
            assert_eq!(
                charged,
                states.capacity() * std::mem::size_of::<ScalarState>()
            );
            assert_eq!(
                charged,
                updater.seal().charged_state_bytes(),
                "{groups} groups"
            );
        }
    }
}
