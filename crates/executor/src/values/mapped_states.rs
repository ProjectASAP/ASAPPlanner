//! Summary states held in their view layout, read in place instead of
//! being rebuilt as objects: a sealed sketch column
//! ([`super::StateColumn::from_sealed_views`]), the columns joined from such
//! columns, and states a runtime stored and hands back over its own bytes
//! ([`super::StateColumn::from_views`]). A merge folds each sketch into the
//! run's accumulator through `asap_sketchlib`'s read-only views.
//!
//! The bytes of state `i` are `bytes[offsets[i]..offsets[i + 1]]`, in a
//! state's view layout ([`super::StateColumn::view_bytes`]):
//! `asap_sketchlib`'s for DDSketch, KLL and Count-Min, each view starting
//! 8-byte aligned ([`MappedSketches`]), and a fixed little-endian record for
//! each exact family ([`MappedRecords`]). Other families have no view
//! layout; their states stay objects.

use crate::summary_kernels::exact::ScalarState;
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

impl MappedSketchKernel {
    /// The kernel of a sketch column of `family`; `None` for a family with
    /// no sketch view layout.
    pub(crate) fn of(family: &FieldDataType) -> Option<Self> {
        match family {
            FieldDataType::Sketch(SketchAlgorithm::DDSketch) => Some(Self::DDSketch),
            FieldDataType::Sketch(SketchAlgorithm::KLL) => Some(Self::Kll),
            FieldDataType::Sketch(SketchAlgorithm::CountMinSketch) => Some(Self::CountMin),
            _ => None,
        }
    }
}

/// The view layout of a family's states: a sketch view, or an exact record.
pub(crate) enum ViewLayout {
    Sketch(MappedSketchKernel),
    Record(ExactKind),
}

impl ViewLayout {
    /// `None` for a family with no view layout.
    pub(crate) fn of(family: &FieldDataType) -> Option<Self> {
        match family {
            FieldDataType::ExactAggregate(kind, _) => Some(Self::Record(*kind)),
            family => MappedSketchKernel::of(family).map(Self::Sketch),
        }
    }
}

/// Whether states of `family` have a view layout, so a column of them can
/// be written as view bytes ([`super::StateColumn::view_bytes`]) and read
/// back in place ([`super::StateColumn::from_views`]): DDSketch, KLL,
/// Count-Min and every exact family.
pub fn family_has_view_layout(family: &FieldDataType) -> bool {
    ViewLayout::of(family).is_some()
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

/// One buffer of states: state `i` at
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

/// Checks `offsets` over `bytes`: at least one, ascending, within `bytes`.
fn check_offsets(bytes: &Buffer, offsets: &ScalarBuffer<u32>) -> Result<(), Error> {
    if offsets.is_empty() {
        return Err(invalid("view offsets need the end offset"));
    }
    if offsets.windows(2).any(|pair| pair[0] > pair[1])
        || offsets
            .last()
            .is_some_and(|&end| end as usize > bytes.len())
    {
        return Err(invalid("view offsets descend or run past the bytes"));
    }
    Ok(())
}

/// Checks that every state of `part` is a `kernel` view, so the reads below
/// cannot fail.
fn check_views(kernel: MappedSketchKernel, part: &MappedPart) -> Result<(), Error> {
    for index in 0..part.offsets.len() - 1 {
        let view = part.state(index);
        match kernel {
            MappedSketchKernel::DDSketch => DdSketchView::from_bytes(view).map(|_| ()),
            MappedSketchKernel::Kll => KllView::from_bytes(view).map(|_| ()),
            MappedSketchKernel::CountMin => CountMinF64View::from_bytes(view).map(|_| ()),
        }
        .map_err(|e| invalid(format!("state {index}: {e}")))?;
    }
    Ok(())
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

impl MappedSketches {
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

    /// State `index`'s view bytes, where they lie.
    pub(crate) fn state_bytes(&self, index: usize) -> &[u8] {
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

    /// The `kernel` views at `offsets` over `bytes`, which something
    /// outside the run holds, read where they lie. Each view is checked
    /// here (in range, 8-byte aligned, a `kernel` view), so reading it
    /// later cannot fail.
    pub(crate) fn over(
        kernel: MappedSketchKernel,
        bytes: Buffer,
        offsets: ScalarBuffer<u32>,
    ) -> Result<Self, Error> {
        check_offsets(&bytes, &offsets)?;
        let part = MappedPart { bytes, offsets };
        check_views(kernel, &part)?;
        Ok(Self {
            kernel,
            parts: vec![Arc::new(part)],
            states: None,
        })
    }

    /// The column of `selected` states (column, index), their bytes copied
    /// into one new buffer of exactly their size ([`Self::packed`]): what a
    /// run holds for states it keeps without the rest of their source
    /// buffers.
    pub(crate) fn copied<'c>(
        kernel: MappedSketchKernel,
        selected: impl Iterator<Item = (&'c MappedSketches, usize)>,
    ) -> Result<(Self, usize), Error> {
        let selected: Vec<&[u8]> = selected
            .map(|(column, index)| column.state_bytes(index))
            .collect();
        Self::packed(kernel, selected.iter().copied())
    }

    /// The `kernel` views `views` (each as `to_view_bytes` writes it)
    /// packed into one buffer of 8-byte words the run holds, each view
    /// starting at a word boundary, and the bytes that buffer and its
    /// 4-byte offsets take. Fails over 4 GiB, past what `u32` offsets
    /// address.
    pub(crate) fn packed<'a>(
        kernel: MappedSketchKernel,
        views: impl ExactSizeIterator<Item = &'a [u8]> + Clone,
    ) -> Result<(Self, usize), Error> {
        let words_len: usize = views.clone().map(|view| view.len().div_ceil(8)).sum();
        if words_len * 8 > u32::MAX as usize {
            return Err(invalid("a column of views over 4 GiB"));
        }
        // One allocation of exactly the views' words, so the charge is
        // what the run holds.
        let mut words = Vec::with_capacity(words_len);
        let mut offsets = Vec::with_capacity(views.len() + 1);
        offsets.push(0u32);
        for view in views {
            for chunk in view.chunks(8) {
                let mut bytes = [0u8; 8];
                bytes[..chunk.len()].copy_from_slice(chunk);
                words.push(u64::from_le_bytes(bytes));
            }
            offsets.push((words.len() * 8) as u32);
        }
        debug_assert_eq!(words.len(), words_len);
        let charged = words.capacity() * 8 + offsets.capacity() * 4;
        let part = MappedPart {
            bytes: Buffer::from(words),
            offsets: ScalarBuffer::from(offsets),
        };
        // Views written by `to_view_bytes` read back; every read below
        // expects them to.
        debug_assert!(check_views(kernel, &part).is_ok());
        Ok((
            Self {
                kernel,
                parts: vec![Arc::new(part)],
                states: None,
            },
            charged,
        ))
    }

    /// Sketch state `index` as an owned kernel object, an accumulator to
    /// merge further states into; a KLL accumulator's compaction coin is
    /// seeded with `seed`, so a merge is reproducible.
    pub(crate) fn accumulator(&self, index: usize, seed: u64) -> Arc<dyn AggregateCore> {
        let bytes = self.state_bytes(index);
        match self.kernel {
            MappedSketchKernel::DDSketch => Arc::new(DDSketchAccumulator {
                inner: DdSketchView::from_bytes(bytes)
                    .expect("a view `to_view_bytes` wrote")
                    .to_owned_sketch(),
            }),
            MappedSketchKernel::Kll => Arc::new(DatasketchesKLLAccumulator {
                inner: KllView::from_bytes(bytes)
                    .expect("a view `to_view_bytes` wrote")
                    .to_owned_sketch(seed),
            }),
            MappedSketchKernel::CountMin => Arc::new(CountMinSketchAccumulator {
                inner: CountMinF64View::from_bytes(bytes)
                    .expect("a view `to_view_bytes` wrote")
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
            .expect("a view `to_view_bytes` wrote")
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
                .merge_view(
                    &DdSketchView::from_bytes(bytes).expect("a view `to_view_bytes` wrote"),
                ),
            MappedSketchKernel::Kll => any
                .downcast_mut::<DatasketchesKLLAccumulator>()
                .ok_or_else(|| invalid("KLL merges only with KLL"))?
                .inner
                .merge_view(&KllView::from_bytes(bytes).expect("a view `to_view_bytes` wrote")),
            MappedSketchKernel::CountMin => any
                .downcast_mut::<CountMinSketchAccumulator>()
                .ok_or_else(|| invalid("Count-Min Sketch merges only with Count-Min Sketch"))?
                .inner
                .merge_view(
                    &CountMinF64View::from_bytes(bytes).expect("a view `to_view_bytes` wrote"),
                ),
        };
        merged.map_err(|e| Error::Operator(e.to_string()))
    }
}

/// A `kernel` sketch state's bytes in its view layout (8-byte aligned once
/// placed at an 8-byte boundary); `None` when `state` is not of `kernel`,
/// so a column's views are all of its own kernel.
pub(crate) fn state_view_bytes(
    kernel: MappedSketchKernel,
    state: &dyn AggregateCore,
) -> Option<Vec<u8>> {
    let any = state.as_any();
    match kernel {
        MappedSketchKernel::DDSketch => any
            .downcast_ref::<DDSketchAccumulator>()
            .map(|dd| dd.inner.to_view_bytes()),
        MappedSketchKernel::Kll => any
            .downcast_ref::<DatasketchesKLLAccumulator>()
            .map(|kll| kll.inner.to_view_bytes()),
        MappedSketchKernel::CountMin => any
            .downcast_ref::<CountMinSketchAccumulator>()
            .map(|cms| cms.inner.to_view_bytes()),
    }
}

/// The record of an exact state: little-endian 8-byte words. A presence
/// word is 0 or 1; the words of an absent part are zero.
///
/// | kind | words |
/// |---|---|
/// | SUM | `sum`, `compensation` (f64), `seen` (presence word, last) |
/// | COUNT | `count` (u64) |
/// | MIN, MAX | presence word, `value` (f64) |
/// | counter (rate, increase) | presence word, `starting_value` (f64), `starting_timestamp` (i64), `last_seen_value` (f64), `last_seen_timestamp` (i64), `total_increase` (f64), `sample_count` (u64) |
/// | irate | twice (older first): presence word, `timestamp` (i64), `value` (f64) |
pub(crate) fn exact_record_bytes(record: &ScalarState) -> Vec<u8> {
    let mut out = Vec::with_capacity(56);
    let mut word = |value: u64| out.extend_from_slice(&value.to_le_bytes());
    match record {
        ScalarState::Sum {
            sum,
            compensation,
            seen,
        } => {
            word(sum.to_bits());
            word(compensation.to_bits());
            word(u64::from(*seen));
        }
        ScalarState::Count(count) => word(*count),
        ScalarState::Min(value) | ScalarState::Max(value) => {
            word(u64::from(value.is_some()));
            word(value.unwrap_or(0.0).to_bits());
        }
        ScalarState::Counter(counter) => {
            word(u64::from(counter.is_some()));
            let c = counter
                .clone()
                .unwrap_or_else(|| IncreaseAccumulator::new(0.0, 0, 0.0, 0));
            word(c.starting_value.to_bits());
            word(c.starting_timestamp as u64);
            word(c.last_seen_value.to_bits());
            word(c.last_seen_timestamp as u64);
            word(c.total_increase.to_bits());
            word(c.sample_count);
        }
        ScalarState::LastTwoSamples(samples) => {
            for sample in samples {
                word(u64::from(sample.is_some()));
                let (time, value) = sample.unwrap_or((0, 0.0));
                word(time as u64);
                word(value.to_bits());
            }
        }
    }
    out
}

/// The size in bytes of an exact kind's record ([`exact_record_bytes`]).
fn record_size(kind: ExactKind) -> usize {
    match kind {
        ExactKind::Sum => 24,
        ExactKind::Count => 8,
        ExactKind::Min | ExactKind::Max => 16,
        ExactKind::Counter => 56,
        ExactKind::IRate => 48,
    }
}

/// The byte offsets of a record's presence words, each 0 or 1.
fn presence_offsets(kind: ExactKind) -> &'static [usize] {
    match kind {
        ExactKind::Sum => &[16],
        ExactKind::Count => &[],
        ExactKind::Min | ExactKind::Max | ExactKind::Counter => &[0],
        ExactKind::IRate => &[0, 24],
    }
}

fn u64_at(record: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(record[at..at + 8].try_into().expect("8 bytes"))
}

fn f64_at(record: &[u8], at: usize) -> f64 {
    f64::from_bits(u64_at(record, at))
}

fn i64_at(record: &[u8], at: usize) -> i64 {
    u64_at(record, at) as i64
}

/// Exact records of one kind ([`exact_record_bytes`]) back to back in bytes
/// something outside the run holds, read where they lie.
pub(crate) struct MappedRecords {
    kind: ExactKind,
    bytes: Buffer,
}

impl MappedRecords {
    /// The `kind` records at `offsets` over `bytes`: record `i` must start
    /// at `i` times its kind's size, and each presence word be 0 or 1.
    pub(crate) fn over(
        kind: ExactKind,
        bytes: Buffer,
        offsets: ScalarBuffer<u32>,
    ) -> Result<Self, Error> {
        check_offsets(&bytes, &offsets)?;
        let size = record_size(kind);
        if offsets
            .iter()
            .enumerate()
            .any(|(i, &offset)| offset as usize != i * size)
        {
            return Err(invalid(format!(
                "{kind:?} records are not {size} bytes each"
            )));
        }
        let bytes = bytes.slice_with_length(0, (offsets.len() - 1) * size);
        if let Some(index) = bytes.chunks_exact(size).position(|record| {
            presence_offsets(kind)
                .iter()
                .any(|&at| u64_at(record, at) > 1)
        }) {
            return Err(invalid(format!("{kind:?} record {index} is malformed")));
        }
        Ok(Self { kind, bytes })
    }

    pub(crate) fn len(&self) -> usize {
        self.bytes.len() / record_size(self.kind)
    }

    /// Record `index`'s bytes, where they lie.
    pub(crate) fn record_bytes(&self, index: usize) -> &[u8] {
        let size = record_size(self.kind);
        &self.bytes[index * size..(index + 1) * size]
    }

    /// Record `index`.
    pub(crate) fn get(&self, index: usize) -> ScalarState {
        let record = self.record_bytes(index);
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

#[cfg(test)]
mod tests {
    //! Joining view-layout columns keeps their states in that layout, and
    //! a merge over them answers as over the same states as objects.
    use super::*;
    use crate::values::{Batch, BatchColumn, StateColumn, StateStorage};
    use crate::StateColumnUpdater;
    use arrow::array::UInt32Array;
    use planner_types::ir::schema::{Field, Schema};
    use planner_types::summary_type::SketchParams;

    /// `state`'s view bytes, `state` of `family`.
    fn view_of(family: &FieldDataType, state: &dyn AggregateCore) -> Vec<u8> {
        state_view_bytes(MappedSketchKernel::of(family).unwrap(), state).unwrap()
    }

    fn ddsketch_schema() -> Arc<Schema> {
        Arc::new(Schema {
            fields: vec![Field {
                name: "state".into(),
                dtype: FieldDataType::Sketch(SketchAlgorithm::DDSketch),
                nullable: false,
            }],
            time_index: None,
            unique_keys: vec![],
            closed: false,
        })
    }

    /// A stored DDSketch window of 4 states in the view layout, held
    /// outside the run, its rows referencing states 3 and 1.
    fn stored_batch(window: u32) -> Batch {
        let family = FieldDataType::Sketch(SketchAlgorithm::DDSketch);
        let mut updater =
            StateColumnUpdater::new(&family, Some(&SketchParams::DDSketch { alpha: 0.01 }))
                .unwrap();
        updater.ensure_groups(4).unwrap();
        for group in 0..4 {
            updater
                .update(group, f64::from(group * 10 + window) + 1.0, 0)
                .unwrap();
        }
        let batch = Batch::try_from_columns(
            ddsketch_schema(),
            vec![BatchColumn::States {
                indices: UInt32Array::from(vec![3, 1]),
                states: Arc::new(updater.seal()),
            }],
        )
        .unwrap();
        batch.mark_states_held_outside_run();
        batch
    }

    /// A sealed DDSketch window of `states` series, held by the run in the
    /// view layout, every row referencing its own state.
    fn sealed_window(states: u32, window: u32) -> Batch {
        let family = FieldDataType::Sketch(SketchAlgorithm::DDSketch);
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
            ddsketch_schema(),
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
                crate::values::Value::Summary { family, state } => view_of(family, state.as_ref()),
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

    /// Concatenating stored view windows held outside the run points at
    /// their buffers: no state is copied, nothing is charged.
    #[test]
    fn concat_keeps_stored_views_where_they_are() {
        let (a, b) = (stored_batch(1), stored_batch(2));
        let joined = Batch::concat(a.schema(), &[a.clone(), b.clone()]).unwrap();
        let states = joined.state_column(0).unwrap();
        assert!(matches!(states.storage(), StateStorage::Mapped(_)));
        assert_eq!(states.state_memory_bytes(), 0);
        assert_eq!(views_of(&joined), [views_of(&a), views_of(&b)].concat());
    }

    /// Window `window` of `series` states of `family`, `samples` samples
    /// each, as kernel objects. Count-Min counts keys, so its states are
    /// built from keys, not from sample values.
    fn window_states(
        family: &FieldDataType,
        parameters: &SketchParams,
        window: u32,
        series: u32,
        samples: u32,
    ) -> Vec<Arc<dyn AggregateCore>> {
        if *family == FieldDataType::Sketch(SketchAlgorithm::CountMinSketch) {
            return (0..series)
                .map(|s| {
                    let mut state = CountMinSketchAccumulator::new(3, 64).unwrap();
                    for sample in 0..samples {
                        state.update(&format!("k{}", (s + window + sample) % 17), 1.5);
                    }
                    Arc::new(state) as Arc<dyn AggregateCore>
                })
                .collect();
        }
        let mut updater = StateColumnUpdater::new(family, Some(parameters)).unwrap();
        updater.ensure_groups(series as usize).unwrap();
        for s in 0..series {
            for sample in 0..samples {
                let value = f64::from(s * 7 + window * samples + sample) * 0.5 + 1.0;
                updater.update(s, value, 0).unwrap();
            }
        }
        let states = updater.seal();
        (0..states.len()).map(|index| states.state(index)).collect()
    }

    /// `states` as a batch of (series name, state), the states as objects
    /// or (`views`) in their view layout.
    fn window_batch(
        family: &FieldDataType,
        states: Vec<Arc<dyn AggregateCore>>,
        views: bool,
    ) -> Batch {
        use arrow::array::DictionaryArray;
        use arrow::datatypes::UInt32Type;
        use planner_types::ir::schema::DataType;
        let schema = Arc::new(Schema {
            fields: vec![
                Field {
                    name: "series".into(),
                    dtype: FieldDataType::Plain(DataType::Utf8),
                    nullable: false,
                },
                Field {
                    name: "state".into(),
                    dtype: family.clone(),
                    nullable: false,
                },
            ],
            time_index: None,
            unique_keys: vec![],
            closed: false,
        });
        let names: Vec<String> = (0..states.len()).map(|s| format!("s{s}")).collect();
        let names: DictionaryArray<UInt32Type> = names.iter().map(String::as_str).collect();
        let indices = UInt32Array::from_iter_values(0..states.len() as u32);
        let column = if views {
            let column =
                StateColumn::from_sealed_views(family.clone(), states.into_iter()).unwrap();
            assert!(matches!(column.storage(), StateStorage::Mapped(_)));
            column
        } else {
            StateColumn::from_objects(family.clone(), states)
        };
        Batch::try_from_columns(
            schema,
            vec![
                BatchColumn::Plain(Arc::new(names)),
                BatchColumn::States {
                    indices,
                    states: Arc::new(column),
                },
            ],
        )
        .unwrap()
    }

    /// `SummaryMerge` by series over `batches`, its output sealed into the
    /// view layout (`sealed`, as `SummaryMergeWindows` seals a chain
    /// level) or not: its output batches.
    fn merge(batches: Vec<Batch>, sealed: bool) -> Vec<Batch> {
        merge_with_peak(batches, sealed).0
    }

    /// [`merge`], and the most bytes the run held at once.
    fn merge_with_peak(batches: Vec<Batch>, sealed: bool) -> (Vec<Batch>, usize) {
        use crate::operators::Operator;
        use crate::plan::PhysicalDAG;
        use crate::runtime::{Limits, RunContext, Scope};
        use futures::StreamExt;
        let schema = batches[0].schema().clone();
        let mut dag = PhysicalDAG::default();
        let lifetime = || planner_types::stages::Lifetime::None;
        dag.add(
            0,
            vec![],
            Operator::source(schema.clone(), batches).unwrap(),
            lifetime(),
        )
        .unwrap();
        let merge = Operator::summary_merge(schema, 1, vec![0]).unwrap();
        let merge = if sealed {
            merge.with_sealed_output()
        } else {
            merge
        };
        dag.add(1, vec![0], merge, lifetime()).unwrap();
        let context = RunContext::new(
            Scope::Query {
                evaluation_time_ms: 0,
            },
            Limits { max_bytes: 1 << 30 },
        )
        .unwrap();
        let merged = futures::executor::block_on(async {
            let mut output = dag.execute(&[1], context.clone()).unwrap().remove(0);
            let mut out = Vec::new();
            while let Some(batch) = output.next().await {
                out.push(Batch::clone(&batch.unwrap()));
            }
            out
        });
        (merged, context.peak_bytes())
    }

    /// [`merge`] by series over `batches`: each series' merged state, as
    /// its view bytes, or (`readout`) as quantiles.
    fn merged_by_series(batches: Vec<Batch>, readout: bool) -> Vec<(String, Vec<u8>)> {
        by_series(&merge(batches, false), readout)
    }

    /// Each series' state in `merged`, as its view bytes, or (`readout`) as
    /// quantiles.
    fn by_series(merged: &[Batch], readout: bool) -> Vec<(String, Vec<u8>)> {
        use planner_types::summary_type::SketchStatistic;
        let mut by_series = Vec::new();
        for batch in merged {
            for row in batch.to_rows() {
                let crate::values::Value::Summary { family, state } = &row[1] else {
                    unreachable!()
                };
                let bytes = if readout {
                    [0.0, 0.1, 0.5, 0.9, 0.99, 1.0]
                        .iter()
                        .flat_map(|&q| {
                            state
                                .estimate(&SketchStatistic::Quantile { q })
                                .unwrap()
                                .to_le_bytes()
                        })
                        .collect()
                } else {
                    view_of(family, state.as_ref())
                };
                by_series.push((format!("{:?}", row[0]), bytes));
            }
        }
        by_series.sort();
        by_series
    }

    /// Every family with a view layout merges to the same answer from its
    /// views as from objects: DDSketch and Count-Min by their bytes, KLL by
    /// its quantiles (12 windows of 15 samples stay below KLL's first
    /// compaction, so they are exact too); over view windows, object
    /// windows, and a mix.
    #[test]
    fn view_states_merge_as_object_states() {
        let cases = [
            (
                FieldDataType::Sketch(SketchAlgorithm::DDSketch),
                SketchParams::DDSketch { alpha: 0.01 },
                false,
            ),
            (
                FieldDataType::Sketch(SketchAlgorithm::KLL),
                SketchParams::Kll { k: 200 },
                true,
            ),
            (
                FieldDataType::Sketch(SketchAlgorithm::CountMinSketch),
                SketchParams::Cms {
                    width: 64,
                    depth: 3,
                },
                false,
            ),
        ];
        for (family, parameters, readout) in cases {
            let windows = |views: &dyn Fn(u32) -> bool| -> Vec<Batch> {
                (0..12)
                    .map(|w| {
                        let states = window_states(&family, &parameters, w, 50, 15);
                        window_batch(&family, states, views(w))
                    })
                    .collect()
            };
            let expected = merged_by_series(windows(&|_| false), readout);
            assert_eq!(expected.len(), 50, "{family:?}");
            assert_eq!(
                merged_by_series(windows(&|_| true), readout),
                expected,
                "{family:?} views"
            );
            assert_eq!(
                merged_by_series(windows(&|w| w % 2 == 0), readout),
                expected,
                "{family:?} mixed"
            );
        }
    }

    /// Past KLL's first compaction (12 windows of 1,100 samples per
    /// series), view windows answer within KLL's bound of the object ones,
    /// and the same view windows merge to the same sketch on every run
    /// (each group's accumulator is seeded from its key).
    #[test]
    fn kll_view_merges_are_reproducible_and_within_the_bound() {
        let family = FieldDataType::Sketch(SketchAlgorithm::KLL);
        let parameters = SketchParams::Kll { k: 200 };
        let windows = |views: bool| -> Vec<Batch> {
            (0..12)
                .map(|w| {
                    let states = window_states(&family, &parameters, w, 20, 1_100);
                    window_batch(&family, states, views)
                })
                .collect()
        };
        let views = windows(true);
        let first = merged_by_series(views.clone(), true);
        for _ in 0..4 {
            assert_eq!(merged_by_series(views.clone(), true), first);
        }
        let expected = merged_by_series(windows(false), true);
        // Values of a series span about 12 x 1,100 x 0.5 = 6,600; k = 200
        // keeps ranks within about 1.7 %, so quantiles within 3 % of the span.
        for ((ks, vs), (ke, ve)) in first.iter().zip(&expected) {
            assert_eq!(ks, ke);
            for (a, b) in vs.chunks(8).zip(ve.chunks(8)) {
                let (a, b) = (
                    f64::from_le_bytes(a.try_into().unwrap()),
                    f64::from_le_bytes(b.try_into().unwrap()),
                );
                assert!((a - b).abs() <= 0.03 * 6_600.0, "{ks}: {a} vs {b}");
            }
        }
    }

    /// A chain level merged from narrower windows (`SummaryMergeWindows`, a
    /// merge with a sealed output) seals its DDSketch windows into their
    /// view layout, as a chain seals its own: it holds the views, each
    /// padded to 8 bytes, and an offset each plus one, and answers as the
    /// unsealed merge.
    #[test]
    fn a_level_merge_seals_its_ddsketch_windows_into_views() {
        let family = FieldDataType::Sketch(SketchAlgorithm::DDSketch);
        let parameters = SketchParams::DDSketch { alpha: 0.01 };
        let windows: Vec<Batch> = (0..12)
            .map(|w| {
                window_batch(
                    &family,
                    window_states(&family, &parameters, w, 50, 15),
                    true,
                )
            })
            .collect();
        let unsealed = merge(windows.clone(), false);
        let sealed = merge(windows, true);
        assert_eq!(by_series(&sealed, false), by_series(&unsealed, false));
        let [batch] = sealed.as_slice() else {
            panic!("one output batch")
        };
        let states = batch.state_column(1).unwrap();
        let views: Vec<Vec<u8>> = (0..states.len())
            .map(|i| view_of(&family, states.state(i).as_ref()))
            .collect();
        assert_eq!(states.state_memory_bytes(), held_for(&views));
        let unsealed_bytes = unsealed[0].state_column(1).unwrap().state_memory_bytes();
        assert!(held_for(&views) * 2 < unsealed_bytes);
    }

    /// Stored view windows held outside the run are charged nothing until
    /// merged: a merge over 12 DDSketch windows of 2,000 series in the view
    /// layout, marked held outside the run (as a store of chain windows
    /// marks them), peaks no higher than over the same windows as objects.
    #[test]
    fn a_merge_over_stored_view_windows_peaks_no_higher_than_over_objects() {
        let family = FieldDataType::Sketch(SketchAlgorithm::DDSketch);
        let parameters = SketchParams::DDSketch { alpha: 0.01 };
        let windows = |views: bool| -> Vec<Batch> {
            (0..12)
                .map(|w| {
                    let states = window_states(&family, &parameters, w, 2_000, 15);
                    let batch = window_batch(&family, states, views);
                    if views {
                        batch.mark_states_held_outside_run();
                    }
                    batch
                })
                .collect()
        };
        let (_, object_peak) = merge_with_peak(windows(false), false);
        let (_, view_peak) = merge_with_peak(windows(true), false);
        assert!(object_peak > 0);
        assert!(
            view_peak <= object_peak,
            "views {view_peak} B, objects {object_peak} B"
        );
    }

    /// Every exact kind's record, present and absent, is its kind's size
    /// and reads back as the state it was written from.
    #[test]
    fn exact_records_read_back_as_written() {
        let counter = IncreaseAccumulator::new(3.0, 1_000, 10.5, 9_000);
        let cases = [
            (
                ExactKind::Sum,
                vec![
                    ScalarState::Sum {
                        sum: 7.25,
                        compensation: -1e-17,
                        seen: true,
                    },
                    ScalarState::Sum {
                        sum: 0.0,
                        compensation: 0.0,
                        seen: false,
                    },
                ],
            ),
            (
                ExactKind::Count,
                vec![ScalarState::Count(42), ScalarState::Count(0)],
            ),
            (
                ExactKind::Min,
                vec![ScalarState::Min(Some(-2.5)), ScalarState::Min(None)],
            ),
            (
                ExactKind::Max,
                vec![ScalarState::Max(Some(f64::NAN)), ScalarState::Max(None)],
            ),
            (
                ExactKind::Counter,
                vec![
                    ScalarState::Counter(Some(counter)),
                    ScalarState::Counter(None),
                ],
            ),
            (
                ExactKind::IRate,
                vec![
                    ScalarState::LastTwoSamples([Some((1_000, 1.0)), Some((2_000, 4.0))]),
                    ScalarState::LastTwoSamples([None, Some((-5, 0.5))]),
                    ScalarState::LastTwoSamples([None, None]),
                ],
            ),
        ];
        for (kind, states) in cases {
            let records: Vec<Vec<u8>> = states.iter().map(exact_record_bytes).collect();
            assert!(
                records.iter().all(|r| r.len() == record_size(kind)),
                "{kind:?}"
            );
            let offsets: Vec<u32> = (0..=records.len())
                .map(|i| (i * record_size(kind)) as u32)
                .collect();
            let mapped = MappedRecords::over(
                kind,
                Buffer::from_vec(records.concat()),
                ScalarBuffer::from(offsets),
            )
            .unwrap();
            assert_eq!(mapped.len(), states.len());
            for (index, state) in states.iter().enumerate() {
                assert_eq!(format!("{:?}", mapped.get(index)), format!("{state:?}"));
                // Bit-equal, so a NaN compares equal to itself.
                assert_eq!(
                    exact_record_bytes(&mapped.get(index)),
                    records[index],
                    "{kind:?} {state:?}"
                );
                assert_eq!(mapped.record_bytes(index), &records[index][..]);
            }
        }
    }
}
