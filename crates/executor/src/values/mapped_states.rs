//! Summary states used where they are stored: a state column over bytes a
//! deployment holds outside the run (for example a memory-mapped file),
//! read in place instead of being rebuilt as objects. A merge folds each
//! mapped sketch into the run's accumulator through `asap_sketchlib`'s
//! read-only views, and reads each mapped exact record where it lies.
//!
//! The bytes of state `i` are `bytes[offsets[i]..offsets[i + 1]]`, each
//! state 8-byte aligned, in the layout [`state_view_bytes`] writes:
//! `asap_sketchlib`'s view layout for DDSketch, KLL and Count-Min
//! ([`MappedSketches`]), and a fixed little-endian record for each exact
//! family ([`MappedRecords`]). Other families have no mapped form; a
//! deployment rebuilds those states as objects.

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

/// Which sketch kernel a mapped column's states belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MappedSketchKernel {
    DDSketch,
    Kll,
    CountMin,
}

/// How a family's states are mapped, if they can be.
pub(crate) enum MappedForm {
    Sketch(MappedSketchKernel),
    Record(ExactKind),
}

impl MappedForm {
    pub(crate) fn of(family: &FieldDataType) -> Option<Self> {
        Some(match family {
            FieldDataType::Sketch(SketchAlgorithm::DDSketch) => {
                Self::Sketch(MappedSketchKernel::DDSketch)
            }
            FieldDataType::Sketch(SketchAlgorithm::Kll) => Self::Sketch(MappedSketchKernel::Kll),
            FieldDataType::Sketch(SketchAlgorithm::Cms) => {
                Self::Sketch(MappedSketchKernel::CountMin)
            }
            FieldDataType::ExactAggregate(kind, _) => Self::Record(*kind),
            _ => return None,
        })
    }
}

/// Whether a state column of `family` can be mapped.
pub fn family_has_mapped_form(family: &FieldDataType) -> bool {
    MappedForm::of(family).is_some()
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

/// One deployment buffer of states: state `i` at
/// `bytes[offsets[i]..offsets[i + 1]]`.
struct MappedPart {
    bytes: Buffer,
    offsets: ScalarBuffer<u32>,
}

impl MappedPart {
    fn state(&self, index: usize) -> &[u8] {
        &self.bytes[self.offsets[index] as usize..self.offsets[index + 1] as usize]
    }
}

/// Mapped sketch states, possibly from several buffers (a column joined
/// from several mapped columns keeps pointing at their buffers).
pub struct MappedSketches {
    kernel: MappedSketchKernel,
    parts: Vec<Arc<MappedPart>>,
    /// State `i` is `parts[p].state(s)` for `states[i] == (p, s)`; `None`:
    /// one part, state `i` is its state `i`.
    states: Option<Vec<(u32, u32)>>,
}

impl std::fmt::Debug for MappedSketches {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MappedSketches")
            .field("kernel", &self.kernel)
            .field("states", &self.len())
            .finish()
    }
}

/// Checks `offsets` over `bytes`: in range, ascending, 8-byte aligned.
fn check_offsets(bytes: &Buffer, offsets: &ScalarBuffer<u32>) -> Result<(), Error> {
    if offsets.is_empty() {
        return Err(invalid("mapped states need offsets"));
    }
    for pair in offsets.windows(2) {
        let (start, end) = (pair[0] as usize, pair[1] as usize);
        if start > end || end > bytes.len() || start % 8 != 0 {
            return Err(invalid("mapped state offsets out of range or misaligned"));
        }
    }
    Ok(())
}

impl MappedSketches {
    /// Sketch states of `kernel`; each state's view is checked here, so
    /// reading them later cannot fail.
    fn new(
        kernel: MappedSketchKernel,
        bytes: Buffer,
        offsets: ScalarBuffer<u32>,
    ) -> Result<Self, Error> {
        check_offsets(&bytes, &offsets)?;
        let part = MappedPart { bytes, offsets };
        for index in 0..part.offsets.len() - 1 {
            let state = part.state(index);
            let checked = match kernel {
                MappedSketchKernel::DDSketch => DdSketchView::from_bytes(state).map(|_| ()),
                MappedSketchKernel::Kll => KllView::from_bytes(state).map(|_| ()),
                MappedSketchKernel::CountMin => CountMinF64View::from_bytes(state).map(|_| ()),
            };
            checked.map_err(|e| invalid(e.to_string()))?;
        }
        Ok(Self {
            kernel,
            parts: vec![Arc::new(part)],
            states: None,
        })
    }

    pub(crate) fn kernel(&self) -> MappedSketchKernel {
        self.kernel
    }

    /// The number of states.
    pub(crate) fn len(&self) -> usize {
        match &self.states {
            Some(states) => states.len(),
            None => self.parts[0].offsets.len() - 1,
        }
    }

    fn state_bytes(&self, index: usize) -> &[u8] {
        match &self.states {
            Some(states) => {
                let (part, state) = states[index];
                self.parts[part as usize].state(state as usize)
            }
            None => self.parts[0].state(index),
        }
    }

    /// The mapped bytes of state `index`'s view, for sharing it into
    /// another column ([`Self::gather`]).
    fn locate(&self, index: usize) -> (&Arc<MappedPart>, u32) {
        match &self.states {
            Some(states) => {
                let (part, state) = states[index];
                (&self.parts[part as usize], state)
            }
            None => (&self.parts[0], index as u32),
        }
    }

    /// The column of `selected` states (column, index), each still read
    /// where it is mapped: joining mapped columns copies no state.
    pub(crate) fn gather<'c>(
        kernel: MappedSketchKernel,
        selected: impl Iterator<Item = (&'c MappedSketches, usize)>,
    ) -> Self {
        let mut parts: Vec<Arc<MappedPart>> = Vec::new();
        let mut states = Vec::new();
        for (column, index) in selected {
            let (part, state) = column.locate(index);
            let at = match parts.iter().position(|p| Arc::ptr_eq(p, part)) {
                Some(at) => at,
                None => {
                    parts.push(part.clone());
                    parts.len() - 1
                }
            };
            states.push((at as u32, state));
        }
        Self {
            kernel,
            parts,
            states: Some(states),
        }
    }

    /// The column of `selected` states (column, index), their bytes copied
    /// into one new buffer of exactly their size, and the bytes that
    /// buffer and its offsets take: what a run holds for states it keeps
    /// without the rest of their source buffers.
    pub(crate) fn copied<'c>(
        kernel: MappedSketchKernel,
        selected: impl Iterator<Item = (&'c MappedSketches, usize)>,
    ) -> (Self, usize) {
        let selected: Vec<&[u8]> = selected
            .map(|(column, index)| column.state_bytes(index))
            .collect();
        let words_len = selected
            .iter()
            .map(|bytes| bytes.len().div_ceil(8))
            .sum::<usize>();
        let (words, offsets) = pack_words(words_len, selected.iter().copied());
        let charged = words.len() * 8 + offsets.len() * 4;
        let part = MappedPart {
            bytes: Buffer::from(words),
            offsets: ScalarBuffer::from(offsets),
        };
        (
            Self {
                kernel,
                parts: vec![Arc::new(part)],
                states: None,
            },
            charged,
        )
    }

    /// Sketch state `index` as an owned kernel object, an accumulator to
    /// merge further states into; a KLL accumulator's compaction coin is
    /// seeded with `seed`, so a merge is reproducible.
    pub(crate) fn accumulator(&self, index: usize, seed: u64) -> Arc<dyn AggregateCore> {
        let bytes = self.state_bytes(index);
        match self.kernel {
            MappedSketchKernel::DDSketch => Arc::new(DDSketchAccumulator {
                inner: DdSketchView::from_bytes(bytes)
                    .expect("checked at construction")
                    .to_owned_sketch(),
            }),
            MappedSketchKernel::Kll => Arc::new(DatasketchesKLLAccumulator {
                inner: KllView::from_bytes(bytes)
                    .expect("checked at construction")
                    .to_owned_sketch(seed),
            }),
            MappedSketchKernel::CountMin => Arc::new(CountMinSketchAccumulator {
                inner: CountMinF64View::from_bytes(bytes)
                    .expect("checked at construction")
                    .to_owned_sketch(),
            }),
        }
    }

    /// The interpolated quantile `q` of DDSketch state `index`, read on its
    /// view (as `DDSketchAccumulator::estimate` answers it); `None` for an
    /// empty state.
    pub(crate) fn ddsketch_quantile(&self, index: usize, q: f64) -> Result<Option<f64>, String> {
        debug_assert_eq!(self.kernel, MappedSketchKernel::DDSketch);
        DdSketchView::from_bytes(self.state_bytes(index))
            .expect("checked at construction")
            .quantile_interpolated(q)
    }

    /// The bytes state `index`'s view reads, an upper bound on what
    /// merging it can add to an accumulator.
    pub(crate) fn state_len(&self, index: usize) -> usize {
        self.state_bytes(index).len()
    }

    /// Folds sketch state `index` into `accumulator` in place, reading the
    /// state where it is stored. Fails when the accumulator is of another
    /// kernel or shape (as merging two owned states would).
    pub(crate) fn merge_into(
        &self,
        index: usize,
        accumulator: &mut dyn AggregateCore,
    ) -> Result<(), Error> {
        let bytes = self.state_bytes(index);
        let any = accumulator.as_any_mut();
        let merged = match self.kernel {
            MappedSketchKernel::DDSketch => any
                .downcast_mut::<DDSketchAccumulator>()
                .ok_or_else(|| invalid("DDSketch merges only with DDSketch"))?
                .inner
                .merge_view(&DdSketchView::from_bytes(bytes).expect("checked at construction")),
            MappedSketchKernel::Kll => any
                .downcast_mut::<DatasketchesKLLAccumulator>()
                .ok_or_else(|| invalid("KLL merges only with KLL"))?
                .inner
                .merge_view(&KllView::from_bytes(bytes).expect("checked at construction")),
            MappedSketchKernel::CountMin => any
                .downcast_mut::<CountMinSketchAccumulator>()
                .ok_or_else(|| invalid("Count-Min Sketch merges only with Count-Min Sketch"))?
                .inner
                .merge_view(&CountMinF64View::from_bytes(bytes).expect("checked at construction")),
        };
        merged.map_err(|e| Error::Operator(e.to_string()))
    }
}

/// `states` packed into exactly `words_len` 8-byte words, each starting at
/// a word boundary, and their byte offsets (one per state, plus one).
pub(crate) fn pack_words<'a>(
    words_len: usize,
    states: impl ExactSizeIterator<Item = &'a [u8]>,
) -> (Vec<u64>, Vec<u32>) {
    let mut words = Vec::with_capacity(words_len);
    let mut offsets = Vec::with_capacity(states.len() + 1);
    offsets.push(0u32);
    for state in states {
        for chunk in state.chunks(8) {
            let mut bytes = [0u8; 8];
            bytes[..chunk.len()].copy_from_slice(chunk);
            words.push(u64::from_le_bytes(bytes));
        }
        offsets.push(u32::try_from(words.len() * 8).expect("a column under 4 GiB"));
    }
    debug_assert_eq!(words.len(), words_len);
    (words, offsets)
}

/// Mapped exact records of one kind, fixed size each.
pub struct MappedRecords {
    kind: ExactKind,
    bytes: Buffer,
}

impl std::fmt::Debug for MappedRecords {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MappedRecords")
            .field("kind", &self.kind)
            .field("records", &self.len())
            .finish()
    }
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

/// The byte offsets of a record's presence words: each must be 0 or 1.
fn flag_offsets(kind: ExactKind) -> &'static [usize] {
    match kind {
        ExactKind::Sum => &[16],
        ExactKind::Count => &[],
        ExactKind::Min | ExactKind::Max | ExactKind::Counter => &[0],
        ExactKind::IRate => &[0, 24],
    }
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

impl MappedRecords {
    /// Records of `kind` at `offsets` over `bytes`, which must be the
    /// records back to back, each of its kind's size and flags.
    fn new(kind: ExactKind, bytes: Buffer, offsets: ScalarBuffer<u32>) -> Result<Self, Error> {
        check_offsets(&bytes, &offsets)?;
        let size = record_bytes(kind);
        let count = offsets.len() - 1;
        if offsets
            .iter()
            .enumerate()
            .any(|(i, &offset)| offset as usize != i * size)
        {
            return Err(invalid(format!(
                "mapped {kind:?} records are not {size} bytes each"
            )));
        }
        let bytes = bytes.slice_with_length(0, count * size);
        for record in bytes.chunks_exact(size) {
            if flag_offsets(kind).iter().any(|&at| u64_at(record, at) > 1) {
                return Err(invalid(format!("a mapped {kind:?} record is malformed")));
            }
        }
        Ok(Self { kind, bytes })
    }

    pub(crate) fn len(&self) -> usize {
        self.bytes.len() / record_bytes(self.kind)
    }

    /// Record `index`.
    pub(crate) fn get(&self, index: usize) -> ScalarState {
        let size = record_bytes(self.kind);
        let record = &self.bytes[index * size..(index + 1) * size];
        let present = |at: usize| u64_at(record, at) == 1;
        match self.kind {
            ExactKind::Sum => ScalarState::Sum {
                sum: f64_at(record, 0),
                compensation: f64_at(record, 8),
                seen: present(16),
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
}

/// The mapped storage of a column of `family`.
pub(crate) enum Mapped {
    Sketches(MappedSketches),
    Records(MappedRecords),
}

/// `family`'s states in `bytes` at `offsets`, checked.
pub(crate) fn mapped(
    family: &FieldDataType,
    bytes: Buffer,
    offsets: ScalarBuffer<u32>,
) -> Result<Mapped, Error> {
    match MappedForm::of(family) {
        Some(MappedForm::Sketch(kernel)) => {
            MappedSketches::new(kernel, bytes, offsets).map(Mapped::Sketches)
        }
        Some(MappedForm::Record(kind)) => {
            MappedRecords::new(kind, bytes, offsets).map(Mapped::Records)
        }
        None => Err(invalid(format!("{family:?} has no mapped form"))),
    }
}

/// A state's bytes in its mapped layout (8-byte aligned once placed at an
/// 8-byte boundary); `None` for a state with no mapped form (another
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
fn exact_record_bytes(record: &ScalarState) -> Vec<u8> {
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

#[cfg(test)]
mod tests {
    //! Joining mapped columns keeps their states mapped.
    use super::*;
    use crate::values::{Batch, BatchColumn, StateColumn, StateStorage};
    use crate::StateColumnUpdater;
    use arrow::array::UInt32Array;
    use planner_types::ir::schema::{Field, Schema};
    use planner_types::summary_type::SketchParams;

    fn mapped_batch(window: u32) -> Batch {
        let family = FieldDataType::Sketch(SketchAlgorithm::DDSketch);
        let schema = Arc::new(Schema {
            fields: vec![Field {
                name: "state".into(),
                dtype: family.clone(),
                nullable: false,
            }],
            time_index: None,
            unique_keys: vec![],
            closed: false,
        });
        let mut updater =
            StateColumnUpdater::new(&family, Some(&SketchParams::DDSketch { alpha: 0.01 }))
                .unwrap();
        updater.ensure_groups(4).unwrap();
        for group in 0..4 {
            updater
                .update(group, f64::from(group * 10 + window) + 1.0, 0)
                .unwrap();
        }
        let owned = updater.seal();
        let mut bytes = Vec::new();
        let mut offsets = vec![0u32];
        for index in 0..owned.len() {
            bytes.extend(state_view_bytes(owned.state(index).as_ref()).unwrap());
            bytes.resize(bytes.len().div_ceil(8) * 8, 0);
            offsets.push(bytes.len() as u32);
        }
        let words: Vec<u64> = bytes
            .chunks(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect();
        let column =
            StateColumn::from_mapped(family, Buffer::from(words), ScalarBuffer::from(offsets))
                .unwrap();
        Batch::try_from_columns(
            schema,
            vec![BatchColumn::States {
                indices: UInt32Array::from(vec![3, 1]),
                states: Arc::new(column),
            }],
        )
        .unwrap()
    }

    /// A sealed DDSketch window of `states` series, held by the run in the
    /// view layout, every row referencing its own state.
    fn sealed_window(states: u32, window: u32) -> Batch {
        let family = FieldDataType::Sketch(SketchAlgorithm::DDSketch);
        let schema = Arc::new(Schema {
            fields: vec![Field {
                name: "state".into(),
                dtype: family.clone(),
                nullable: false,
            }],
            time_index: None,
            unique_keys: vec![],
            closed: false,
        });
        let mut updater =
            StateColumnUpdater::new(&family, Some(&SketchParams::DDSketch { alpha: 0.01 }))
                .unwrap();
        updater.ensure_groups(states as usize).unwrap();
        for group in 0..states {
            for sample in 0..15 {
                updater
                    .update(group, f64::from(group * 31 + window * 7 + sample) + 1.0, 0)
                    .unwrap();
            }
        }
        Batch::try_from_columns(
            schema,
            vec![BatchColumn::States {
                indices: UInt32Array::from_iter_values(0..states),
                states: Arc::new(updater.seal()),
            }],
        )
        .unwrap()
    }

    /// The bytes a run holds for `views`: each padded to 8, and an offset
    /// each plus one.
    fn held_for(views: &[Vec<u8>]) -> usize {
        views.iter().map(|v| v.len().div_ceil(8) * 8).sum::<usize>() + 4 * (views.len() + 1)
    }

    fn views_of(batch: &Batch) -> Vec<Vec<u8>> {
        batch
            .to_rows()
            .iter()
            .map(|row| match &row[0] {
                crate::values::Value::Summary { state, .. } => {
                    state_view_bytes(state.as_ref()).unwrap()
                }
                _ => unreachable!(),
            })
            .collect()
    }

    /// A filter keeping one row of a sealed 10,000-state window copies that
    /// state into a buffer of its own: the batch holds and is charged that
    /// state's view, not the window's buffer.
    #[test]
    fn compacting_sealed_views_keeps_only_the_states_used() {
        let window = sealed_window(10_000, 0);
        let whole = window.state_column(0).unwrap().state_memory_bytes();
        let mut keep = vec![false; 10_000];
        keep[4_321] = true;
        let one = window
            .filter(&arrow::array::BooleanArray::from(keep))
            .unwrap();
        let states = one.state_column(0).unwrap();
        assert_eq!(states.len(), 1);
        assert!(matches!(states.storage(), StateStorage::Mapped(_)));
        assert_eq!(states.state_memory_bytes(), held_for(&views_of(&one)));
        assert!(states.state_memory_bytes() * 1_000 < whole);
        assert_eq!(views_of(&one), vec![views_of(&window).swap_remove(4_321)]);
    }

    /// Concatenating sealed view windows copies the states the rows use
    /// into one buffer, charged exactly what it holds, with the same states.
    #[test]
    fn concat_of_sealed_views_holds_and_charges_the_states_used() {
        let (a, b) = (sealed_window(500, 1), sealed_window(500, 2));
        let joined = Batch::concat(a.schema(), &[a.clone(), b.clone()]).unwrap();
        let states = joined.state_column(0).unwrap();
        assert!(matches!(states.storage(), StateStorage::Mapped(_)));
        assert!(!states.is_held_outside_run());
        let mut expected = views_of(&a);
        expected.extend(views_of(&b));
        assert_eq!(views_of(&joined), expected);
        assert_eq!(states.state_memory_bytes(), held_for(&expected));
    }

    #[test]
    fn concat_keeps_mapped_states_mapped() {
        let (a, b) = (mapped_batch(1), mapped_batch(2));
        let joined = Batch::concat(a.schema(), &[a.clone(), b.clone()]).unwrap();
        let states = joined.state_column(0).unwrap();
        assert!(matches!(states.storage(), StateStorage::Mapped(_)));
        assert_eq!(states.state_memory_bytes(), 0);
        let rows: Vec<String> = joined
            .to_rows()
            .iter()
            .map(|row| {
                format!(
                    "{:?}",
                    state_view_bytes(match &row[0] {
                        crate::values::Value::Summary { state, .. } => state.as_ref(),
                        _ => unreachable!(),
                    })
                )
            })
            .collect();
        let expected: Vec<String> = [a, b]
            .iter()
            .flat_map(|batch| batch.to_rows())
            .map(|row| {
                format!(
                    "{:?}",
                    state_view_bytes(match &row[0] {
                        crate::values::Value::Summary { state, .. } => state.as_ref(),
                        _ => unreachable!(),
                    })
                )
            })
            .collect();
        assert_eq!(rows, expected);
    }
}
