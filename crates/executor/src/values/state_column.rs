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
        let shared = states
            .iter()
            .filter_map(|state| state.shared_state())
            .collect::<std::collections::BTreeMap<_, _>>();
        let state_memory_bytes = states
            .iter()
            .map(|state| state.approx_memory_bytes())
            .sum::<usize>()
            + shared.values().sum::<usize>();
        Self {
            family,
            storage: StateStorage::Objects(states),
            state_memory_bytes,
            held_outside_run: AtomicBool::new(false),
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

    /// Records that something outside any run holds these states.
    pub(crate) fn mark_held_outside_run(&self) {
        self.held_outside_run.store(true, Ordering::Relaxed);
    }

    /// Memory a run that holds this column is charged for its states: none
    /// when something outside the run holds them, else the memory they hold.
    pub(crate) fn charged_state_bytes(&self) -> usize {
        if self.held_outside_run.load(Ordering::Relaxed) {
            0
        } else {
            self.state_memory_bytes
        }
    }
}

impl std::fmt::Debug for StateColumn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateColumn")
            .field("family", &self.family)
            .field("state_count", &self.len())
            .finish()
    }
}
