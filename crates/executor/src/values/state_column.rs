//! The summary states of one batch column.
use crate::summary_kernels::exact::{ExactAccumulator, ScalarState};
use crate::AggregateCore;
use planner_types::ir::schema::FieldDataType;
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
    /// Whether something outside the run holds these states (a stored pane
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
    Exact(Vec<ScalarState>),
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
        let state_memory_bytes = states.capacity() * std::mem::size_of::<ScalarState>();
        Self {
            family,
            storage: StateStorage::Exact(states),
            state_memory_bytes,
            held_outside_run: AtomicBool::new(false),
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
                states[index].clone(),
            )),
        }
    }

    /// Whether something outside any run holds these states.
    pub(crate) fn is_held_outside_run(&self) -> bool {
        self.held_outside_run.load(Ordering::Relaxed)
    }

    /// Whether something outside any run holds state `index`: every state
    /// of a column marked so, or that object of a derived column.
    pub(crate) fn state_held_outside_run(&self, index: usize) -> bool {
        self.is_held_outside_run()
            || self
                .objects_held_outside_run
                .as_ref()
                .is_some_and(|held| held[index])
    }

    /// Records that something outside any run holds these states.
    pub(crate) fn mark_held_outside_run(&self) {
        self.held_outside_run.store(true, Ordering::Relaxed);
    }

    /// Memory a run that holds this column is charged for its states: none
    /// when something outside the run holds them, else the memory they hold.
    pub(crate) fn charged_state_bytes(&self) -> usize {
        if self.is_held_outside_run() {
            0
        } else {
            self.state_memory_bytes
        }
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
