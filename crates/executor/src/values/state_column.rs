//! The summary states of one batch column.
use super::mapped_states::{MappedSketchKernel, MappedSketches};
use crate::summary_kernels::exact::{ExactAccumulator, ScalarState};
use crate::summary_kernels::increase::IncreaseAccumulator;
use crate::AggregateCore;
use arrow::buffer::BooleanBuffer;
use planner_types::ir::schema::FieldDataType;
use planner_types::summary_type::ExactKind;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// The summary states of one batch column: one family and parameter set
/// (from the plan) for every state, so operators dispatch once per column,
/// not once per row. The batch column itself holds, per row, the index of
/// that row's state here, so filtering, sorting and limiting rows never
/// copy a sketch.
pub struct StateColumn {
    /// The family and parameters every state of the column has.
    family: FieldDataType,
    /// The states, addressed by the indices in the batch column.
    storage: StateStorage,
    /// Memory the states hold: each state's own footprint, plus each state
    /// shared by several states (one Hydra grid) once.
    state_memory_bytes: usize,
    /// Whether something outside the run holds these states (a stored tumbling window
    /// handed to a plan as a source): immutable and shared, so a run that
    /// reads them is not charged for them. Set once, never cleared.
    held_outside_run: AtomicBool,
    /// For an object column derived from other columns (compaction,
    /// concat): per object, whether something outside the run holds it.
    /// `None` when no object is.
    objects_held_outside_run: Option<Box<[bool]>>,
}

/// How a state column stores its states.
pub(crate) enum StateStorage {
    /// One kernel object per state: sketches, and states read from rows.
    Objects(Vec<Arc<dyn AggregateCore>>),
    /// One fixed-size numeric record per state, for an exact accumulator
    /// (sum, count, min, max, rate, increase) of one population: one
    /// contiguous buffer, no allocation per state.
    Exact(ExactRecords),
    /// Sketch states in their view layout, read through read-only views
    /// ([`super::mapped_states`]): sealed windows the run holds in that
    /// layout ([`StateColumn::from_sealed_views`]), or stored ones something
    /// outside the run holds (charged nothing).
    Mapped(MappedSketches),
}

/// The exact records of one column, each stored in the size its family
/// needs (a MAX in 8 bytes and a validity bit, not in the 56 bytes of the
/// largest [`ScalarState`]), in buffers of exactly the column's length.
/// [`Self::get`] gives record `index` back as a [`ScalarState`].
pub(crate) enum ExactRecords {
    /// SUM: the compensated sum and whether any sample was added.
    Sum(Box<[(f64, f64, bool)]>),
    /// COUNT.
    Count(Box<[u64]>),
    /// MIN; absent while empty.
    Min(Optional<f64>),
    /// MAX; absent while empty.
    Max(Optional<f64>),
    /// Rate and increase; absent while empty.
    Counter(Optional<IncreaseAccumulator>),
    /// irate: the last two samples.
    LastTwoSamples(Box<[LastTwoSamples]>),
}

/// An irate record: the last two samples, `(time, value)`, the newer second.
type LastTwoSamples = [Option<(i64, f64)>; 2];

/// Values that may be absent: the values, a filler where one is absent,
/// and one validity bit each.
pub(crate) struct Optional<T> {
    values: Box<[T]>,
    present: BooleanBuffer,
}

impl<T: Clone> Optional<T> {
    fn of(values: impl Iterator<Item = Option<T>>, filler: T) -> Self {
        let mut present = Vec::new();
        let values = values
            .map(|value| {
                present.push(value.is_some());
                value.unwrap_or_else(|| filler.clone())
            })
            .collect();
        Self {
            values,
            present: BooleanBuffer::from(present),
        }
    }

    fn get(&self, index: usize) -> Option<T> {
        self.present
            .value(index)
            .then(|| self.values[index].clone())
    }

    fn memory_bytes(&self) -> usize {
        std::mem::size_of_val(&*self.values) + self.present.inner().capacity()
    }
}

impl ExactRecords {
    /// `states`, all records of the exact `family`, in their family's
    /// layout.
    fn of(family: &FieldDataType, states: Vec<ScalarState>) -> Self {
        let FieldDataType::ExactAggregate(kind, _) = family else {
            unreachable!("exact records are of an exact family")
        };
        fn wrong<T>() -> T {
            unreachable!("one exact family has one kind of record")
        }
        match kind {
            ExactKind::Sum => Self::Sum(
                states
                    .into_iter()
                    .map(|state| match state {
                        ScalarState::Sum {
                            sum,
                            compensation,
                            seen,
                        } => (sum, compensation, seen),
                        _ => wrong(),
                    })
                    .collect(),
            ),
            ExactKind::Count => Self::Count(
                states
                    .into_iter()
                    .map(|state| match state {
                        ScalarState::Count(count) => count,
                        _ => wrong(),
                    })
                    .collect(),
            ),
            ExactKind::Min => Self::Min(Optional::of(
                states.into_iter().map(|state| match state {
                    ScalarState::Min(value) => value,
                    _ => wrong(),
                }),
                0.0,
            )),
            ExactKind::Max => Self::Max(Optional::of(
                states.into_iter().map(|state| match state {
                    ScalarState::Max(value) => value,
                    _ => wrong(),
                }),
                0.0,
            )),
            ExactKind::Counter => Self::Counter(Optional::of(
                states.into_iter().map(|state| match state {
                    ScalarState::Counter(counter) => counter,
                    _ => wrong(),
                }),
                IncreaseAccumulator::new(0.0, 0, 0.0, 0),
            )),
            ExactKind::IRate => Self::LastTwoSamples(
                states
                    .into_iter()
                    .map(|state| match state {
                        ScalarState::LastTwoSamples(samples) => samples,
                        _ => wrong(),
                    })
                    .collect(),
            ),
        }
    }

    /// Record `index`.
    pub(crate) fn get(&self, index: usize) -> ScalarState {
        match self {
            Self::Sum(records) => {
                let (sum, compensation, seen) = records[index];
                ScalarState::Sum {
                    sum,
                    compensation,
                    seen,
                }
            }
            Self::Count(records) => ScalarState::Count(records[index]),
            Self::Min(records) => ScalarState::Min(records.get(index)),
            Self::Max(records) => ScalarState::Max(records.get(index)),
            Self::Counter(records) => ScalarState::Counter(records.get(index)),
            Self::LastTwoSamples(records) => ScalarState::LastTwoSamples(records[index]),
        }
    }

    /// The number of records.
    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Sum(records) => records.len(),
            Self::Count(records) => records.len(),
            Self::Min(records) | Self::Max(records) => records.values.len(),
            Self::Counter(records) => records.values.len(),
            Self::LastTwoSamples(records) => records.len(),
        }
    }

    /// The bytes the records hold.
    fn memory_bytes(&self) -> usize {
        match self {
            Self::Sum(records) => std::mem::size_of_val(&**records),
            Self::Count(records) => std::mem::size_of_val(&**records),
            Self::Min(records) | Self::Max(records) => records.memory_bytes(),
            Self::Counter(records) => records.memory_bytes(),
            Self::LastTwoSamples(records) => std::mem::size_of_val(&**records),
        }
    }
}

impl StateColumn {
    /// The column of kernel objects `states`, all of `family`. Not checked
    /// here: callers pass states their own kernel built for `family`, or
    /// states [`super::Batch::try_new`] validated.
    pub(crate) fn from_objects(family: FieldDataType, states: Vec<Arc<dyn AggregateCore>>) -> Self {
        let state_memory_bytes = objects_memory_bytes(states.iter());
        Self {
            family,
            storage: StateStorage::Objects(states),
            state_memory_bytes,
            held_outside_run: AtomicBool::new(false),
            objects_held_outside_run: None,
        }
    }

    /// The column of kernel objects `states` taken from other columns, each
    /// with whether something outside the run holds it, a bit the column
    /// keeps per object so later derivations read it too. Charged its
    /// pointer slots and bits, plus the states the run holds (built or
    /// copied by it); states held outside the run are shared by pointer,
    /// not charged again.
    pub(crate) fn from_objects_of(
        family: FieldDataType,
        states: Vec<(Arc<dyn AggregateCore>, bool)>,
    ) -> Self {
        let held = states
            .iter()
            .map(|(_, held_outside_run)| *held_outside_run)
            .collect::<Box<[bool]>>();
        let objects_held_outside_run = held.contains(&true).then_some(held);
        let state_memory_bytes = states.len() * std::mem::size_of::<Arc<dyn AggregateCore>>()
            + objects_held_outside_run
                .as_ref()
                .map_or(0, |held| held.len())
            + objects_memory_bytes(
                states
                    .iter()
                    .filter(|(_, held_outside_run)| !held_outside_run)
                    .map(|(state, _)| state),
            );
        Self {
            family,
            storage: StateStorage::Objects(states.into_iter().map(|(state, _)| state).collect()),
            state_memory_bytes,
            held_outside_run: AtomicBool::new(false),
            objects_held_outside_run,
        }
    }

    /// The column of exact records `states`, all of the exact `family`.
    pub(crate) fn from_exact(family: FieldDataType, states: Vec<ScalarState>) -> Self {
        let records = ExactRecords::of(&family, states);
        let state_memory_bytes = records.memory_bytes();
        Self {
            family,
            storage: StateStorage::Exact(records),
            state_memory_bytes,
            held_outside_run: AtomicBool::new(false),
            objects_held_outside_run: None,
        }
    }

    /// The sealed sketch `states` of `family`, stored in their view layout
    /// (the layout a stored window is priced at) in one buffer the run
    /// holds: charged the views, each padded to 8 bytes, and the column's
    /// 4-byte offsets. `None` for a family with no view layout, or a
    /// column over 4 GiB.
    pub(crate) fn from_sealed_views(
        family: FieldDataType,
        states: impl ExactSizeIterator<Item = Arc<dyn AggregateCore>>,
    ) -> Option<Self> {
        let kernel = MappedSketchKernel::of(&family)?;
        let views = states
            .map(|state| super::mapped_states::state_view_bytes(state.as_ref()))
            .collect::<Option<Vec<Vec<u8>>>>()?;
        let (sketches, state_memory_bytes) =
            MappedSketches::packed(kernel, views.iter().map(Vec::as_slice)).ok()?;
        Some(Self::stored_views(
            family,
            StateStorage::Mapped(sketches),
            state_memory_bytes,
        ))
    }

    /// The column of view-layout `storage` the run holds, charged
    /// `state_memory_bytes`.
    pub(crate) fn stored_views(
        family: FieldDataType,
        storage: StateStorage,
        state_memory_bytes: usize,
    ) -> Self {
        Self {
            family,
            storage,
            state_memory_bytes,
            held_outside_run: AtomicBool::new(false),
            objects_held_outside_run: None,
        }
    }

    /// The column of mapped `storage`: held outside any run, charged nothing.
    pub(crate) fn mapped_column(family: FieldDataType, storage: StateStorage) -> Self {
        Self {
            family,
            storage,
            state_memory_bytes: 0,
            held_outside_run: AtomicBool::new(true),
            objects_held_outside_run: None,
        }
    }

    /// The family and parameters of every state.
    pub(crate) fn family(&self) -> &FieldDataType {
        &self.family
    }

    /// How the states are stored.
    pub(crate) fn storage(&self) -> &StateStorage {
        &self.storage
    }

    /// The number of states.
    pub fn len(&self) -> usize {
        match &self.storage {
            StateStorage::Objects(states) => states.len(),
            StateStorage::Exact(states) => states.len(),
            StateStorage::Mapped(states) => states.len(),
        }
    }

    /// Whether the column holds no state.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// State `index` as a kernel object. Shared for an object column; built
    /// from its record for an exact column.
    pub fn state(&self, index: usize) -> Arc<dyn AggregateCore> {
        match &self.storage {
            StateStorage::Objects(states) => states[index].clone(),
            StateStorage::Exact(states) => Arc::new(ExactAccumulator::from_scalar(
                self.family.clone(),
                states.get(index),
            )),
            StateStorage::Mapped(states) => states.accumulator(index, 0),
        }
    }

    /// Whether something outside any run holds these states.
    pub(crate) fn is_held_outside_run(&self) -> bool {
        self.held_outside_run.load(Ordering::Relaxed)
    }

    /// Whether something outside any run holds state `index`: every state
    /// of a column marked so (`true` for any index, the whole column being
    /// held outside), or that object of a derived column. For a derived
    /// column, an index past the column reads `false`, so its state is
    /// charged in full: a wrong index can overcharge, never undercharge.
    pub(crate) fn state_held_outside_run(&self, index: usize) -> bool {
        self.is_held_outside_run()
            || self
                .objects_held_outside_run
                .as_ref()
                .is_some_and(|held| held.get(index).copied().unwrap_or(false))
    }

    /// Records that something outside any run holds these states.
    pub(crate) fn mark_held_outside_run(&self) {
        self.held_outside_run.store(true, Ordering::Relaxed);
    }

    /// Memory a run that holds this column is charged for its states: none
    /// when something outside the run holds them, else
    /// [`Self::state_memory_bytes`].
    pub(crate) fn charged_state_bytes(&self) -> usize {
        if self.is_held_outside_run() {
            0
        } else {
            self.state_memory_bytes
        }
    }

    /// Memory the states hold, whoever holds them.
    pub fn state_memory_bytes(&self) -> usize {
        self.state_memory_bytes
    }
}

/// The memory kernel objects `states` hold: each one's own footprint, plus
/// each state shared by several of them (one Hydra grid) once.
fn objects_memory_bytes<'a>(states: impl Iterator<Item = &'a Arc<dyn AggregateCore>>) -> usize {
    let mut shared = std::collections::BTreeMap::new();
    let own = states
        .map(|state| {
            if let Some((id, bytes)) = state.shared_state() {
                shared.insert(id, bytes);
            }
            state.approx_memory_bytes()
        })
        .sum::<usize>();
    own + shared.values().sum::<usize>()
}

impl std::fmt::Debug for StateColumn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateColumn")
            .field("family", &self.family)
            .field("state_count", &self.len())
            .finish()
    }
}
