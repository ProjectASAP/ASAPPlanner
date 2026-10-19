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

    /// The family and parameters of every state.
    pub fn family(&self) -> &FieldDataType {
        &self.family
    }

    /// The number of groups.
    pub fn group_count(&self) -> usize {
        match &self.states {
            GroupStates::Exact { states, .. } => states.len(),
            GroupStates::Sketches(states) => states.len(),
            GroupStates::Keyed { states, .. } => states.len(),
        }
    }

    /// Add a group with an empty state; returns its number.
    pub fn add_group(&mut self) -> Result<u32, Error> {
        let group = u32::try_from(self.group_count())
            .map_err(|_| Error::Invalid("more than u32::MAX groups".into()))?;
        match &mut self.states {
            GroupStates::Exact { empty, states } => states.push(empty.clone()),
            GroupStates::Sketches(states) => states.push(Self::sketch_updater(&self.family)?),
            GroupStates::Keyed { empty, states } => states.push(empty.clone()),
        }
        if let Some(latest) = &mut self.latest_sample_ms {
            latest.push(None);
        }
        Ok(group)
    }

    /// Add one sample to `group`'s state: `value` observed at
    /// `timestamp_ms` (read only by counters). An operator error when the
    /// state cannot take the value (not finite, outside a DDSketch's domain,
    /// a counter sample not after the group's previous one) or the family is
    /// keyed; the state is then unchanged.
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
        let index = group as usize;
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
                if !value.is_finite() {
                    return Err(operator_error("accumulator input must be finite"));
                }
                states[index].update(*value, timestamp_ms);
            }
            GroupStates::Sketches(states) => states[index]
                .update_value(value, timestamp_ms)
                .map_err(operator_error)?,
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
    /// Count-Min or Count Sketch with a heap). An operator error when the
    /// family is not keyed or the kernel refuses the weight.
    pub fn update_item(&mut self, group: u32, items: &[Value], weight: f64) -> Result<(), Error> {
        let GroupStates::Keyed { states, .. } = &mut self.states else {
            return Err(operator_error("only a keyed summary takes items"));
        };
        states[group as usize].update(items, weight)
    }

    /// Merge `other`, an updater of the same family whose groups have the
    /// same numbers (another shard of one pane), into this one, group by
    /// group; a group only `other` has is added. An operator error when the
    /// families differ or a kernel refuses the merge.
    pub fn merge(&mut self, other: &Self) -> Result<(), Error> {
        if self.family != other.family {
            return Err(operator_error("cannot merge different summary families"));
        }
        while self.group_count() < other.group_count() {
            self.add_group()?;
        }
        match (&mut self.states, &other.states) {
            (GroupStates::Exact { states, .. }, GroupStates::Exact { states: theirs, .. }) => {
                for (state, theirs) in states.iter_mut().zip(theirs) {
                    *state = state.merge(theirs).map_err(operator_error)?;
                }
            }
            (GroupStates::Sketches(states), GroupStates::Sketches(theirs)) => {
                for (state, theirs) in states.iter_mut().zip(theirs) {
                    state
                        .merge_accumulator(theirs.snapshot_accumulator().as_ref())
                        .map_err(operator_error)?;
                }
            }
            (GroupStates::Keyed { states, .. }, GroupStates::Keyed { states: theirs, .. }) => {
                for (state, theirs) in states.iter_mut().zip(theirs) {
                    let merged = state.merge_with(theirs).map_err(operator_error)?;
                    *state = merged
                        .as_any()
                        .downcast_ref::<WeightedFrequency>()
                        .cloned()
                        .ok_or_else(|| operator_error("a merge changed the state's type"))?;
                }
            }
            _ => unreachable!("one family has one kind of states"),
        }
        if let (Some(latest), Some(theirs)) = (&mut self.latest_sample_ms, &other.latest_sample_ms)
        {
            for (latest, theirs) in latest.iter_mut().zip(theirs) {
                *latest = (*latest).max(*theirs);
            }
        }
        Ok(())
    }

    /// Memory the open states hold, in bytes.
    pub fn memory_bytes(&self) -> usize {
        match &self.states {
            GroupStates::Exact { states, .. } => {
                states.capacity() * std::mem::size_of::<ScalarState>()
            }
            GroupStates::Sketches(states) => {
                states.iter().map(|state| state.memory_usage_bytes()).sum()
            }
            GroupStates::Keyed { states, .. } => {
                states.iter().map(|state| state.approx_memory_bytes()).sum()
            }
        }
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
