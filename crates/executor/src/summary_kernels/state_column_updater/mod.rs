//! Per-sample construction of a column of summary states, one state per
//! group: the code the `SummaryAgg` build runs row by row, exposed so a
//! runtime that ingests sample by sample builds the same states.
use super::exact::ScalarState;
use super::factory::{create_hydra_cms, create_planner_accumulator, AccumulatorUpdater};
use super::hydra_cms::{hydra_group_name, hydra_item, hydra_weight, HydraCms, HydraCmsGroup};
use super::weighted_frequency::WeightedFrequency;
use crate::values::{StateColumn, Value};
use crate::{AggregateCore, Error};

use planner_types::ir::scalar::ColumnRef;
use planner_types::ir::schema::FieldDataType;
use planner_types::summary_type::{ExactKind, SketchParams, SummaryUpdate};
use std::sync::Arc;

mod reorder_buffer;
use reorder_buffer::{CounterOrder, ReorderBuffer};

/// The open summary states of one family, one per group, updated one sample
/// at a time; sealed into an immutable [`StateColumn`].
///
/// Built from the plan's family and θ with [`Self::new`]. Groups are
/// numbered `0..group_count()` in the order [`Self::add_group`] (or
/// [`Self::add_group_with_key`]) adds them; the caller keeps its own map from
/// a group's labels to its number.
pub struct StateColumnUpdater {
    /// The family of every state.
    family: FieldDataType,
    /// θ of every state: the `parameters` of the exported build node
    /// (`PhysicalASAPDAGNode::parameters`); `None` for an exact family.
    parameters: Option<SketchParams>,
    /// The states, by group.
    states: GroupStates,
    /// For a counter (rate, increase), the order its samples are applied
    /// in: each group's in increasing time; `None` for any other family.
    counter: Option<CounterOrder>,
    /// Memory the open states hold, kept up to date on every change.
    memory_bytes: usize,
    /// The shape of the keys [`Self::add_group_with_key`] has taken, from
    /// the first: per position, the value type seen (`None` while only NULL
    /// has been). `None` before the first key.
    key_types: Option<Vec<Option<std::mem::Discriminant<Value>>>>,
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
        /// An empty state of the family's shape, cloned for each new group
        /// (boxed: much larger than the other kinds' fields).
        empty: Box<WeightedFrequency>,
        /// The states, by group.
        states: Vec<WeightedFrequency>,
    },
    /// One HydraCms grid shared by every group (a Hydra family).
    Hydra {
        /// The grid all groups count into (boxed: much larger than the other
        /// kinds' fields).
        grid: Box<HydraCms>,
        /// Each group's subpopulation name in the grid, by group.
        names: Vec<String>,
    },
}

fn operator_error(message: impl ToString) -> Error {
    Error::Operator(message.to_string())
}

impl StateColumnUpdater {
    /// An updater with no group for `family` and θ `parameters`, the family
    /// and the exported node's `parameters` of a `SummaryAgg` the plan
    /// builds one state per group of: an exact accumulator (no θ), a value
    /// sketch (KLL, DDSketch, HLL, KMV, UnivMon), a keyed frequency sketch
    /// with a heap, or a HydraCms grid shared by all groups. Bad input for
    /// any other summary type, or θ its kernel does not build.
    pub fn new(family: &FieldDataType, parameters: Option<&SketchParams>) -> Result<Self, Error> {
        crate::values::validate_family(family)?;
        let states = match family {
            FieldDataType::Sketch(planner_types::summary_type::SketchAlgorithm::HydraCms) => {
                GroupStates::Hydra {
                    grid: Box::new(create_hydra_cms(family, parameters).map_err(Error::Invalid)?),
                    names: Vec::new(),
                }
            }
            FieldDataType::ExactAggregate(..) if parameters.is_some() => {
                return Err(Error::Invalid(
                    "an exact summary has no parameters θ".into(),
                ))
            }
            FieldDataType::ExactAggregate(..) => GroupStates::Exact {
                empty: ScalarState::empty(family).map_err(Error::Invalid)?,
                states: Vec::new(),
            },
            FieldDataType::Sketch(
                algorithm @ (planner_types::summary_type::SketchAlgorithm::CmsWithHeap
                | planner_types::summary_type::SketchAlgorithm::CountSketchWithHeap),
            ) => {
                let Some(parameters) = parameters else {
                    return Err(Error::Invalid(format!(
                        "a {algorithm:?} summary needs its parameters θ"
                    )));
                };
                let (algorithm, width, depth, capacity) =
                    WeightedFrequency::configuration(algorithm, parameters)?;
                GroupStates::Keyed {
                    empty: Box::new(WeightedFrequency::new(algorithm, width, depth, capacity)?),
                    states: Vec::new(),
                }
            }
            FieldDataType::Sketch(..) => {
                // Fails for a family with no per-group value kernel.
                Self::sketch_updater(family, parameters)?;
                GroupStates::Sketches(Vec::new())
            }
            _ => return Err(Error::Invalid("unsupported summary family".into())),
        };
        let counter = matches!(
            family,
            FieldDataType::ExactAggregate(ExactKind::Rate | ExactKind::Increase, _)
        );
        // A Hydra grid is allocated up front, shared by all groups.
        let memory_bytes = match &states {
            GroupStates::Hydra { grid, .. } => grid.approx_memory_bytes(),
            _ => 0,
        };
        Ok(Self {
            family: family.clone(),
            parameters: parameters.cloned(),
            states,
            counter: counter.then(CounterOrder::default),
            memory_bytes,
            key_types: None,
        })
    }

    /// This updater taking each group's samples in any order within
    /// `max_out_of_order_ms`: the data's declared bound on how late a
    /// sample arrives after a later sample of its series (`DataArrival`'s
    /// `max_out_of_order_ms`). A counter (rate, increase) holds each sample
    /// until the newest sample seen in its group is more than the bound
    /// past it, then applies the group's held samples in time order:
    /// samples at 10 s, 12 s, 11 s with a 5 s bound build the state of
    /// 10 s, 11 s, 12 s, whatever other groups' samples arrive between
    /// them. A sample further behind is still applied when it follows its
    /// group's last applied sample; otherwise [`Self::update`] refuses it,
    /// as Prometheus refuses a sample outside its out-of-order window.
    /// [`Self::seal`] and [`Self::merge`] apply every held sample first, so
    /// a runtime seals a tumbling window once the bound has passed after
    /// its end. Any other family needs no order and is returned unchanged.
    /// A bound beyond Int64 holds every sample until the seal.
    pub fn with_max_out_of_order_ms(mut self, max_out_of_order_ms: u64) -> Self {
        if let Some(counter) = &mut self.counter {
            let max_out_of_order_ms = i64::try_from(max_out_of_order_ms).unwrap_or(i64::MAX);
            counter.reorder_buffer = Some(ReorderBuffer::new(
                max_out_of_order_ms,
                counter.latest_applied_ms.len(),
            ));
        }
        self
    }

    fn sketch_updater(
        family: &FieldDataType,
        parameters: Option<&SketchParams>,
    ) -> Result<Box<dyn AccumulatorUpdater>, Error> {
        create_planner_accumulator(
            family,
            parameters,
            &SummaryUpdate::column(ColumnRef::SampleValue),
        )
        .map_err(Error::Invalid)
    }

    /// The number of groups.
    pub fn group_count(&self) -> usize {
        match &self.states {
            GroupStates::Exact { states, .. } => states.len(),
            GroupStates::Sketches(states) => states.len(),
            GroupStates::Keyed { states, .. } => states.len(),
            GroupStates::Hydra { names, .. } => names.len(),
        }
    }

    /// Add groups with empty states until there are `group_count`.
    pub fn ensure_groups(&mut self, group_count: usize) -> Result<(), Error> {
        while self.group_count() < group_count {
            self.add_group()?;
        }
        Ok(())
    }

    /// Add the group whose key (its grouping columns' values, in the plan's
    /// order and of the plan's group column types) is `key`, with an empty
    /// state; returns its number. A HydraCms group needs its key: it names
    /// the group's subpopulation in the shared grid, as the batch build names
    /// it. Other families ignore the key, so a runtime can add every group
    /// this way.
    ///
    /// The updater does not know the plan's group columns, so it checks what
    /// it can: bad input for an empty key or a summary value, a key whose
    /// arity differs from the first key's, or a value whose type differs
    /// from the type earlier keys had at that position (NULL fits any).
    pub fn add_group_with_key(&mut self, key: &[Value]) -> Result<u32, Error> {
        self.check_key(key)?;
        let GroupStates::Hydra { names, .. } = &mut self.states else {
            return self.add_group();
        };
        let group = u32::try_from(names.len())
            .map_err(|_| Error::Invalid("more than u32::MAX groups".into()))?;
        let name = hydra_group_name(key)?;
        self.memory_bytes += name.len();
        names.push(name);
        Ok(group)
    }

    /// `key` fits the keys added before (see [`Self::add_group_with_key`]);
    /// records its types.
    fn check_key(&mut self, key: &[Value]) -> Result<(), Error> {
        if key.is_empty() {
            return Err(Error::Invalid("a group key has at least one value".into()));
        }
        if key
            .iter()
            .any(|value| matches!(value, Value::Summary { .. }))
        {
            return Err(Error::Invalid("a summary value is not a group key".into()));
        }
        let types = self.key_types.get_or_insert_with(|| vec![None; key.len()]);
        if types.len() != key.len() {
            return Err(Error::Invalid(format!(
                "group key has {} values; earlier keys have {}",
                key.len(),
                types.len()
            )));
        }
        let mut updated = types.clone();
        for (position, (value, seen)) in key.iter().zip(updated.iter_mut()).enumerate() {
            if matches!(value, Value::Null) {
                continue;
            }
            let kind = std::mem::discriminant(value);
            match seen {
                Some(seen) if *seen != kind => {
                    return Err(Error::Invalid(format!(
                        "group key value {position} has another type than earlier keys"
                    )))
                }
                _ => *seen = Some(kind),
            }
        }
        *types = updated;
        Ok(())
    }

    /// Add a group with an empty state; returns its number. Bad input for a
    /// HydraCms family, whose groups are added with
    /// [`Self::add_group_with_key`].
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
                let state = Self::sketch_updater(&self.family, self.parameters.as_ref())?;
                let bytes = state.memory_usage_bytes();
                states.push(state);
                bytes
            }
            GroupStates::Keyed { empty, states } => {
                states.push(empty.as_ref().clone());
                empty.approx_memory_bytes()
            }
            GroupStates::Hydra { .. } => {
                return Err(Error::Invalid(
                    "a HydraCms group is added with its key (add_group_with_key)".into(),
                ))
            }
        };
        if let Some(counter) = &mut self.counter {
            counter.add_group();
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
    /// finite for a sketch or a counter, outside a DDSketch's domain, a
    /// counter sample not after the group's last applied one, or at the time
    /// of a held one ([`Self::with_max_out_of_order_ms`])) or the family is
    /// keyed. The state is then unchanged.
    pub fn update(&mut self, group: u32, value: f64, timestamp_ms: i64) -> Result<(), Error> {
        self.update_value(group, &Value::Float64(value), timestamp_ms)
    }

    /// [`Self::update`] for a typed value: a sketch also takes Int64 values,
    /// a distinct count (KMV, HLL) Utf8 values, and UnivMon Utf8 and Bool
    /// values.
    pub(crate) fn update_value(
        &mut self,
        group: u32,
        value: &Value,
        timestamp_ms: i64,
    ) -> Result<(), Error> {
        let index = self.index(group)?;
        if self
            .counter
            .as_ref()
            .is_some_and(|counter| counter.reorder_buffer.is_some())
        {
            return self.hold_and_release(group, value, timestamp_ms);
        }
        self.apply(index, value, timestamp_ms)
    }

    /// Update state `index` with one sample, in its group's time order.
    fn apply(&mut self, index: usize, value: &Value, timestamp_ms: i64) -> Result<(), Error> {
        if let Some(counter) = &self.counter {
            if counter.latest_applied_ms[index].is_some_and(|prior| timestamp_ms <= prior) {
                return Err(operator_error(
                    "counter samples must have strictly increasing timestamps within each group",
                ));
            }
        }
        match &mut self.states {
            GroupStates::Exact { states, .. } => {
                // The exact kernel's input rule
                // (`factory::exact_accumulator_input`); a count also counts
                // a non-NULL value of any other type (SQL `COUNT(col)`).
                let FieldDataType::ExactAggregate(kind, _) = &self.family else {
                    unreachable!("exact states belong to an exact family")
                };
                match (&states[index], value) {
                    (_, Value::Float64(value)) => {
                        super::factory::exact_accumulator_input(*kind, *value)
                            .map_err(operator_error)?;
                        states[index].update(*value, timestamp_ms);
                    }
                    (ScalarState::Count(_), _) => states[index].update(0.0, timestamp_ms),
                    _ => return Err(operator_error("summary update requires Float64")),
                }
            }
            GroupStates::Sketches(states) => {
                let state = &mut states[index];
                let before = state.memory_usage_bytes();
                state
                    .update_value(value, timestamp_ms)
                    .map_err(operator_error)?;
                self.memory_bytes = self.memory_bytes - before + state.memory_usage_bytes();
            }
            GroupStates::Keyed { .. } | GroupStates::Hydra { .. } => {
                return Err(operator_error(
                    "a keyed summary is updated with an item and a weight",
                ))
            }
        }
        if let Some(counter) = &mut self.counter {
            counter.latest_applied_ms[index] = Some(timestamp_ms);
        }
        Ok(())
    }

    /// Add `weight` to the item tuple `items` in `group`'s keyed state (a
    /// Count-Min or Count Sketch with a heap), or to the one item `items` in
    /// `group`'s part of a HydraCms grid (a non-negative integer weight within
    /// `i32`). Bad input for a group never added; an operator error when the
    /// family is not keyed or the kernel refuses the item or weight.
    pub fn update_item(&mut self, group: u32, items: &[Value], weight: f64) -> Result<(), Error> {
        let index = self.index(group)?;
        if let GroupStates::Hydra { grid, names } = &mut self.states {
            let [item] = items else {
                return Err(operator_error("a HydraCms update takes one item"));
            };
            let item = hydra_item(item)?;
            let weight = hydra_weight(&Value::Float64(weight))?;
            return grid
                .count(&names[index], &item, weight)
                .map_err(operator_error);
        }
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
    /// same numbers (another shard of one tumbling window), into this one, group by
    /// group; a group only `other` has is added.
    ///
    /// For a counter (rate, increase), each group's samples in the two
    /// shards must be disjoint in time: one shard's last sample before the
    /// other's first. Overlapping shards would count the increase of their
    /// overlap twice, so they are refused as bad input, as are different
    /// families or θ. An operator error when a kernel refuses the merge. On
    /// an error this updater may hold some groups already merged.
    pub fn merge(&mut self, mut other: Self) -> Result<(), Error> {
        if self.family != other.family || self.parameters != other.parameters {
            return Err(Error::Invalid(
                "cannot merge different summary families or parameters".into(),
            ));
        }
        // Held samples are part of each shard's window.
        self.release_all_held()?;
        other.release_all_held()?;
        if let (
            GroupStates::Hydra { grid, names },
            GroupStates::Hydra {
                grid: theirs,
                names: their_names,
            },
        ) = (&mut self.states, &other.states)
        {
            // Group `g` must be the same subpopulation in both shards.
            if names
                .iter()
                .zip(their_names)
                .any(|(ours, theirs)| ours != theirs)
            {
                return Err(Error::Invalid(
                    "HydraCms shards name a group differently: they must number groups alike"
                        .into(),
                ));
            }
            **grid = grid.merge(theirs).map_err(operator_error)?;
            for name in &their_names[names.len().min(their_names.len())..] {
                self.memory_bytes += name.len();
                names.push(name.clone());
            }
            return Ok(());
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
        if let (Some(counter), Some(theirs)) = (&mut self.counter, other.counter) {
            counter.merge_latest_applied(&theirs);
        }
        Ok(())
    }

    /// Memory the open states hold, in bytes.
    pub fn memory_bytes(&self) -> usize {
        self.memory_bytes
    }

    /// The states as an immutable column, state `g` being group `g`'s,
    /// after applying every held sample.
    pub fn seal(mut self) -> StateColumn {
        // A held sample is valid input after its group's applied samples
        // (checked when held), so its state takes it.
        self.release_all_held()
            .expect("held counter samples apply in time order");
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
            GroupStates::Hydra { grid, names } => {
                let grid = Arc::new(*grid);
                StateColumn::from_objects(
                    self.family,
                    names
                        .into_iter()
                        .map(|group| {
                            Arc::new(HydraCmsGroup {
                                grid: grid.clone(),
                                group,
                            }) as Arc<dyn AggregateCore>
                        })
                        .collect(),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests;
