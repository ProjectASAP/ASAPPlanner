//! Summary states used where they are stored: a state column over bytes a
//! deployment holds outside the run (for example a memory-mapped file),
//! read in place through `asap_sketchlib`'s read-only views instead of being
//! rebuilt as objects. A merge folds each mapped state into the run's
//! accumulator, so a query over stored windows allocates only its
//! accumulators.
//!
//! The bytes of state `i` are `bytes[offsets[i]..offsets[i + 1]]`, each
//! state 8-byte aligned, in the layout [`state_view_bytes`] writes:
//! `asap_sketchlib`'s view layout for DDSketch, KLL and Count-Min, and a
//! fixed little-endian record for each exact family. Other families have
//! no mapped form; a deployment rebuilds those states as objects.

use crate::summary_kernels::exact::{ExactAccumulator, ScalarState};
use crate::summary_kernels::increase::IncreaseAccumulator;
use crate::summary_kernels::{
    CountMinSketchAccumulator, DDSketchAccumulator, DatasketchesKLLAccumulator,
};
use crate::{AggregateCore, Error};
use arrow::buffer::{Buffer, ScalarBuffer};
use asap_sketchlib::{CountMinF64View, DdSketchView, KllView};
use planner_types::ir::schema::FieldDataType;
use planner_types::summary_type::{ExactKind, SketchAlgorithm};
use std::sync::Arc;

/// Which kernel a mapped column's states belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MappedKernel {
    DDSketch,
    Kll,
    CountMin,
    Exact(ExactKind),
}

impl MappedKernel {
    /// The mapped kernel of `family`, if it has a mapped form.
    pub(crate) fn of(family: &FieldDataType) -> Option<Self> {
        Some(match family {
            FieldDataType::Sketch(SketchAlgorithm::DDSketch) => Self::DDSketch,
            FieldDataType::Sketch(SketchAlgorithm::Kll) => Self::Kll,
            FieldDataType::Sketch(SketchAlgorithm::Cms) => Self::CountMin,
            FieldDataType::ExactAggregate(kind, _) => Self::Exact(*kind),
            _ => return None,
        })
    }

    /// The fixed record size of an exact kind.
    fn record_bytes(kind: ExactKind) -> usize {
        match kind {
            ExactKind::Sum => 24,
            ExactKind::Count => 8,
            ExactKind::Min | ExactKind::Max => 16,
            ExactKind::Counter => 56,
            ExactKind::IRate => 48,
        }
    }
}

/// Whether a state column of `family` can be mapped.
pub fn family_has_mapped_form(family: &FieldDataType) -> bool {
    MappedKernel::of(family).is_some()
}

/// The states of one column, in bytes held outside the run.
pub struct MappedStates {
    kernel: MappedKernel,
    bytes: Buffer,
    offsets: ScalarBuffer<u32>,
}

impl std::fmt::Debug for MappedStates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MappedStates")
            .field("kernel", &self.kernel)
            .field("states", &self.len())
            .finish()
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

/// Little-endian fixed fields of an exact record.
fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
}
fn f64_at(bytes: &[u8], at: usize) -> f64 {
    f64::from_bits(u64_at(bytes, at))
}
fn i64_at(bytes: &[u8], at: usize) -> i64 {
    u64_at(bytes, at) as i64
}

impl MappedStates {
    /// The states of `family` at `bytes[offsets[i]..offsets[i + 1]]`; every
    /// state is checked here (its view parses, or its record has its
    /// kind's size and flags), so reading them later cannot fail.
    pub(crate) fn new(
        family: &FieldDataType,
        bytes: Buffer,
        offsets: ScalarBuffer<u32>,
    ) -> Result<Self, Error> {
        let kernel = MappedKernel::of(family)
            .ok_or_else(|| invalid(format!("{family:?} has no mapped form")))?;
        if offsets.is_empty() {
            return Err(invalid("mapped states need offsets"));
        }
        let mapped = Self {
            kernel,
            bytes,
            offsets,
        };
        for index in 0..mapped.len() {
            let (start, end) = (
                mapped.offsets[index] as usize,
                mapped.offsets[index + 1] as usize,
            );
            if start > end || end > mapped.bytes.len() || start % 8 != 0 {
                return Err(invalid("mapped state offsets out of range or misaligned"));
            }
            let state = &mapped.bytes[start..end];
            let ok = match kernel {
                MappedKernel::DDSketch => DdSketchView::from_bytes(state).map(|_| ()),
                MappedKernel::Kll => KllView::from_bytes(state).map(|_| ()),
                MappedKernel::CountMin => CountMinF64View::from_bytes(state).map(|_| ()),
                MappedKernel::Exact(kind) => {
                    let flags_ok = match kind {
                        ExactKind::Sum => state.len() == 24 && u64_at(state, 16) <= 1,
                        ExactKind::Min | ExactKind::Max | ExactKind::Counter => {
                            state.len() == MappedKernel::record_bytes(kind) && u64_at(state, 0) <= 1
                        }
                        ExactKind::IRate => {
                            state.len() == 48 && u64_at(state, 0) <= 1 && u64_at(state, 24) <= 1
                        }
                        ExactKind::Count => state.len() == 8,
                    };
                    if flags_ok {
                        Ok(())
                    } else {
                        return Err(invalid(format!("a mapped {kind:?} record is malformed")));
                    }
                }
            };
            ok.map_err(|e| invalid(e.to_string()))?;
        }
        Ok(mapped)
    }

    pub(crate) fn kernel(&self) -> MappedKernel {
        self.kernel
    }

    /// The number of states.
    pub(crate) fn len(&self) -> usize {
        self.offsets.len() - 1
    }

    fn state_bytes(&self, index: usize) -> &[u8] {
        &self.bytes[self.offsets[index] as usize..self.offsets[index + 1] as usize]
    }

    /// The address of state `index`'s bytes, for checking where it is read.
    pub fn state_address(&self, index: usize) -> usize {
        self.state_bytes(index).as_ptr() as usize
    }

    /// Exact record `index`.
    pub(crate) fn scalar(&self, index: usize) -> ScalarState {
        let record = self.state_bytes(index);
        let MappedKernel::Exact(kind) = self.kernel else {
            unreachable!("records of an exact column")
        };
        let present = |at: usize| u64_at(record, at) == 1;
        match kind {
            ExactKind::Sum => ScalarState::Sum {
                sum: f64_at(record, 0),
                compensation: f64_at(record, 8),
                seen: u64_at(record, 16) == 1,
            },
            ExactKind::Count => ScalarState::Count(u64_at(record, 0)),
            ExactKind::Min => ScalarState::Min(present(0).then(|| f64_at(record, 8))),
            ExactKind::Max => ScalarState::Max(present(0).then(|| f64_at(record, 8))),
            ExactKind::Counter => ScalarState::Counter(present(0).then(|| IncreaseAccumulator {
                starting_value: f64_at(record, 8),
                starting_timestamp: i64_at(record, 16),
                last_seen_value: f64_at(record, 24),
                last_seen_timestamp: i64_at(record, 32),
                total_increase: f64_at(record, 40),
                sample_count: u64_at(record, 48),
            })),
            ExactKind::IRate => {
                let sample = |at: usize| {
                    present(at).then(|| (i64_at(record, at + 8), f64_at(record, at + 16)))
                };
                ScalarState::LastTwoSamples([sample(0), sample(24)])
            }
        }
    }

    /// Sketch state `index` as an owned kernel object: an accumulator to
    /// merge further states into, or a single state to estimate from.
    pub(crate) fn state(&self, index: usize, family: &FieldDataType) -> Arc<dyn AggregateCore> {
        let bytes = self.state_bytes(index);
        match self.kernel {
            MappedKernel::DDSketch => Arc::new(DDSketchAccumulator {
                inner: DdSketchView::from_bytes(bytes)
                    .expect("checked at construction")
                    .to_owned_sketch(),
            }),
            MappedKernel::Kll => Arc::new(DatasketchesKLLAccumulator {
                inner: KllView::from_bytes(bytes)
                    .expect("checked at construction")
                    .to_owned_sketch(),
            }),
            MappedKernel::CountMin => Arc::new(CountMinSketchAccumulator {
                inner: CountMinF64View::from_bytes(bytes)
                    .expect("checked at construction")
                    .to_owned_sketch(),
            }),
            MappedKernel::Exact(_) => Arc::new(ExactAccumulator::from_scalar(
                family.clone(),
                self.scalar(index),
            )),
        }
    }

    /// Folds sketch state `index` into `accumulator` in place, reading the
    /// state where it is stored.
    pub(crate) fn merge_into(
        &self,
        index: usize,
        accumulator: &mut dyn AggregateCore,
    ) -> Result<(), Error> {
        let bytes = self.state_bytes(index);
        let any = accumulator.as_any_mut();
        let merged = match self.kernel {
            MappedKernel::DDSketch => any
                .downcast_mut::<DDSketchAccumulator>()
                .ok_or_else(|| invalid("DDSketch merges only with DDSketch"))?
                .inner
                .merge_view(&DdSketchView::from_bytes(bytes).expect("checked at construction")),
            MappedKernel::Kll => any
                .downcast_mut::<DatasketchesKLLAccumulator>()
                .ok_or_else(|| invalid("KLL merges only with KLL"))?
                .inner
                .merge_view(&KllView::from_bytes(bytes).expect("checked at construction")),
            MappedKernel::CountMin => any
                .downcast_mut::<CountMinSketchAccumulator>()
                .ok_or_else(|| invalid("Count-Min Sketch merges only with Count-Min Sketch"))?
                .inner
                .merge_view(&CountMinF64View::from_bytes(bytes).expect("checked at construction")),
            MappedKernel::Exact(_) => {
                return Err(invalid("exact records merge as records"));
            }
        };
        merged.map_err(|e| Error::Operator(e.to_string()))
    }
}

/// A state's bytes in its mapped layout (each 8-byte aligned once placed
/// at an 8-byte boundary); `None` for a state with no mapped form (another
/// family, or a keyed exact state).
pub fn state_view_bytes(state: &dyn AggregateCore) -> Option<Vec<u8>> {
    let any = state.as_any();
    if let Some(dd) = any.downcast_ref::<DDSketchAccumulator>() {
        return Some(dd.inner.to_view_bytes());
    }
    if let Some(kll) = any.downcast_ref::<DatasketchesKLLAccumulator>() {
        return Some(kll.inner.to_view_bytes());
    }
    if let Some(cms) = any.downcast_ref::<CountMinSketchAccumulator>() {
        return Some(cms.inner.to_view_bytes());
    }
    let exact = any.downcast_ref::<ExactAccumulator>()?;
    Some(exact_record_bytes(exact.scalar()?))
}

/// An exact record in its fixed layout.
pub(crate) fn exact_record_bytes(record: &ScalarState) -> Vec<u8> {
    let mut out = Vec::with_capacity(56);
    let mut u64 = |value: u64| out.extend_from_slice(&value.to_le_bytes());
    match record {
        ScalarState::Sum {
            sum,
            compensation,
            seen,
        } => {
            u64(sum.to_bits());
            u64(compensation.to_bits());
            u64(u64::from(*seen));
        }
        ScalarState::Count(count) => u64(*count),
        ScalarState::Min(value) | ScalarState::Max(value) => {
            u64(u64::from(value.is_some()));
            u64(value.unwrap_or(0.0).to_bits());
        }
        ScalarState::Counter(counter) => {
            u64(u64::from(counter.is_some()));
            let c = counter
                .clone()
                .unwrap_or_else(|| IncreaseAccumulator::new(0.0, 0, 0.0, 0));
            u64(c.starting_value.to_bits());
            u64(c.starting_timestamp as u64);
            u64(c.last_seen_value.to_bits());
            u64(c.last_seen_timestamp as u64);
            u64(c.total_increase.to_bits());
            u64(c.sample_count);
        }
        ScalarState::LastTwoSamples(samples) => {
            for sample in samples {
                u64(u64::from(sample.is_some()));
                let (time, value) = sample.unwrap_or((0, 0.0));
                u64(time as u64);
                u64(value.to_bits());
            }
        }
    }
    out
}
