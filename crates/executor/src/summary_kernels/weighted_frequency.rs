//! Keyed Float64 frequency state over sketchlib's f64 Count-Min and Count
//! Sketch with a candidate heap (`CountMinSketchWithHeap`,
//! `CountSketchWithHeap`), keyed by the item tuple's canonical `TupleKey`
//! bytes. A heap of capacity 0 makes a plain Count Sketch.
//!
//! The sketches hash each row with its own seed (`RegularPath`). Until
//! sketchlib releases its checked update, this adapter enforces the weight
//! rule (finite, and nonnegative for Count-Min) and keeps every counter
//! finite, and it rescores the heap's residents after each update, so the
//! heap holds the current top-k by the current estimates.
use crate::AggregateCore;
use crate::{values::Value, Error};
use asap_sketchlib::{
    CountMinSketchWithHeap, CountSketchWithHeap, DataInput, KeyColumn, OwnedDataInput,
    RegularPath, TopKHeap, TupleKey, Vector2D,
};

/// The counter matrix of a [`WeightedFrequency`] state.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FrequencyAlgorithm {
    /// Count-Min: nonnegative weights, the minimum over rows.
    Cms,
    /// Count Sketch: signed weights, the median over an odd number of rows.
    CountSketch,
}

#[derive(Clone)]
enum Sketch {
    Cms(CountMinSketchWithHeap<Vector2D<f64>, RegularPath>),
    CountSketch(CountSketchWithHeap<Vector2D<f64>, RegularPath>),
}

#[derive(Clone)]
pub struct WeightedFrequency {
    sketch: Sketch,
    /// The sum of every added weight's magnitude, merges included. No
    /// counter's magnitude exceeds it, so while it is finite every counter is.
    weight_mass: f64,
}

impl std::fmt::Debug for WeightedFrequency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WeightedFrequency")
            .field("algorithm", &self.algorithm())
            .field("shape", &self.shape())
            .finish()
    }
}

fn key_column(value: &Value) -> Result<KeyColumn, Error> {
    Ok(match value {
        Value::Null => KeyColumn::Null,
        Value::Bool(v) => KeyColumn::Bool(*v),
        Value::Int64(v) => KeyColumn::I64(*v),
        Value::Float64(v) if v.is_finite() => KeyColumn::F64(*v),
        Value::Float64(_) => {
            return Err(Error::Invalid(
                "non-finite weighted frequency identity".into(),
            ))
        }
        Value::Utf8(v) => KeyColumn::String(v.to_string()),
        _ => {
            return Err(Error::Invalid(
                "unsupported weighted frequency identity".into(),
            ))
        }
    })
}

fn value(column: &KeyColumn) -> Value {
    match column {
        KeyColumn::Null => Value::Null,
        KeyColumn::Bool(v) => Value::Bool(*v),
        KeyColumn::I64(v) => Value::Int64(*v),
        KeyColumn::F64(v) => Value::Float64(*v),
        KeyColumn::String(v) => Value::Utf8(v.as_str().into()),
    }
}

fn tuple_key(columns: Vec<KeyColumn>) -> Result<Vec<u8>, Error> {
    TupleKey::new(columns).to_bytes().map_err(Error::Invalid)
}

/// Writes each resident's current estimate back to the heap:
/// `update_owned` re-sifts a resident and never evicts one.
fn rescore(heap: &mut TopKHeap<f64>, scores: Vec<(OwnedDataInput, f64)>) {
    for (key, count) in &scores {
        heap.update_owned(key, *count);
    }
}

/// The current estimate of every resident of `$sketch`'s heap.
macro_rules! resident_scores {
    ($sketch:expr) => {
        $sketch
            .heap()
            .heap()
            .iter()
            .map(|entry| (entry.key.clone(), $sketch.estimate(&entry.key.as_data_input())))
            .collect::<Vec<(OwnedDataInput, f64)>>()
    };
}

impl WeightedFrequency {
    pub(crate) fn configuration(
        kind: &planner_types::ir::schema::SketchKind,
    ) -> Result<(FrequencyAlgorithm, usize, usize, usize), Error> {
        use planner_types::ir::schema::{SketchAlgorithm as A, SketchParams as P};
        let (algorithm, width, depth, capacity) = match (kind.algorithm(), kind.params()) {
            (
                A::CmsWithHeap,
                P::CmsWithHeap {
                    width,
                    depth,
                    heap_size,
                },
            ) => (FrequencyAlgorithm::Cms, *width, *depth, *heap_size),
            (
                A::CountSketchWithHeap,
                P::CountSketchWithHeap {
                    width,
                    depth,
                    heap_size,
                },
            ) => (FrequencyAlgorithm::CountSketch, *width, *depth, *heap_size),
            _ => {
                return Err(Error::Invalid(
                    "unsupported weighted frequency family".into(),
                ))
            }
        };
        // A heap family needs a heap.
        if capacity == 0 {
            return Err(Error::Invalid(
                "weighted frequency heap capacity is 0".into(),
            ));
        }
        let (width, depth) = (width as usize, depth as usize);
        Self::check_shape(algorithm, width, depth)?;
        Ok((algorithm, width, depth, capacity as usize))
    }

    /// The shapes this state builds: sketchlib's (both dimensions non-zero,
    /// the matrix within `usize`), and an odd depth for Count Sketch, whose
    /// median the guarantee assumes. Any depth: past `MATRIX_MAX_ROWS` rows
    /// sketchlib derives each row's seed from its list, so rows stay
    /// independent (the state then has no ASAPv1 encoding, which the
    /// executor does not use).
    pub fn check_shape(
        algorithm: FrequencyAlgorithm,
        width: usize,
        depth: usize,
    ) -> Result<(), Error> {
        asap_sketchlib::CountMinSketch::check_shape(depth, width).map_err(Error::Invalid)?;
        if algorithm == FrequencyAlgorithm::CountSketch && depth % 2 == 0 {
            return Err(Error::Invalid(
                "Count Sketch depth must be odd for its median".into(),
            ));
        }
        Ok(())
    }

    /// Algorithm and `(width, depth, capacity)` shape, so a deployment can check
    /// a decoded state against its declared family.
    pub fn algorithm(&self) -> FrequencyAlgorithm {
        match self.sketch {
            Sketch::Cms(_) => FrequencyAlgorithm::Cms,
            Sketch::CountSketch(_) => FrequencyAlgorithm::CountSketch,
        }
    }
    pub fn shape(&self) -> (usize, usize, usize) {
        match &self.sketch {
            Sketch::Cms(s) => (s.cols(), s.rows(), s.heap().capacity()),
            Sketch::CountSketch(s) => (s.cols(), s.rows(), s.heap().capacity()),
        }
    }
    pub fn new(
        algorithm: FrequencyAlgorithm,
        width: usize,
        depth: usize,
        capacity: usize,
    ) -> Result<Self, Error> {
        Self::check_shape(algorithm, width, depth)?;
        let sketch = match algorithm {
            FrequencyAlgorithm::Cms => Sketch::Cms(CountMinSketchWithHeap::new(depth, width, capacity)),
            FrequencyAlgorithm::CountSketch => {
                Sketch::CountSketch(CountSketchWithHeap::new(depth, width, capacity))
            }
        };
        Ok(Self {
            sketch,
            weight_mass: 0.0,
        })
    }
    /// Adds `weight` to the identity tuple of `labels`, one `Utf8` identity
    /// per label, as the DAG's keyed build keys a label vector.
    pub(crate) fn update_labels(&mut self, labels: &[String], weight: f64) -> Result<(), Error> {
        let key = tuple_key(labels.iter().cloned().map(KeyColumn::String).collect())?;
        self.update_key(&key, weight)
    }
    /// The weight rule: finite, and nonnegative for Count-Min.
    pub(crate) fn check_weight(&self, weight: f64) -> Result<(), Error> {
        if !weight.is_finite() || (self.algorithm() == FrequencyAlgorithm::Cms && weight < 0.0) {
            return Err(Error::Operator(
                "weighted frequency requires finite weights; CMS additionally requires nonnegative weights".into(),
            ));
        }
        Ok(())
    }
    /// Empties the state in place, keeping its algorithm and shape.
    pub(crate) fn clear(&mut self) {
        match &mut self.sketch {
            Sketch::Cms(s) => s.clear(),
            Sketch::CountSketch(s) => s.clear(),
        }
        self.weight_mass = 0.0;
    }
    pub fn update(&mut self, values: &[Value], weight: f64) -> Result<(), Error> {
        let key = tuple_key(values.iter().map(key_column).collect::<Result<_, _>>()?)?;
        self.update_key(&key, weight)
    }
    /// Adds `weight` to `key`, rescores the heap's residents and offers `key`
    /// to the heap. A rejected weight or a sum past `f64::MAX` leaves the
    /// state unchanged.
    fn update_key(&mut self, key: &[u8], weight: f64) -> Result<(), Error> {
        self.check_weight(weight)?;
        let weight_mass = self.weight_mass + weight.abs();
        if !weight_mass.is_finite() {
            return Err(Error::Operator("weighted frequency sum overflow".into()));
        }
        self.weight_mass = weight_mass;
        let key = DataInput::Bytes(key);
        match &mut self.sketch {
            Sketch::Cms(s) => {
                s.count_min_sketch_mut().update_weighted(&key, weight);
                let scores = resident_scores!(s);
                rescore(s.heap_mut(), scores);
                let estimate = s.estimate(&key);
                s.heap_mut().update(&key, estimate);
            }
            Sketch::CountSketch(s) => {
                s.count_sketch_mut().update_weighted(&key, weight);
                let scores = resident_scores!(s);
                rescore(s.heap_mut(), scores);
                let estimate = s.estimate(&key);
                s.heap_mut().update(&key, estimate);
            }
        }
        Ok(())
    }
    /// Point estimate for one identity tuple: the minimum over rows for
    /// Count-Min, the sign-corrected median over rows for CountSketch.
    pub fn point_estimate(&self, values: &[Value]) -> Result<f64, Error> {
        let key = tuple_key(values.iter().map(key_column).collect::<Result<_, _>>()?)?;
        let key = DataInput::Bytes(&key);
        Ok(match &self.sketch {
            Sketch::Cms(s) => s.estimate(&key),
            Sketch::CountSketch(s) => s.estimate(&key),
        })
    }
    /// The heap's `n` highest-scoring candidates, highest first (ties by
    /// key bytes), each as its identity values followed by its score.
    pub fn rows(&self, n: usize) -> Vec<Vec<Value>> {
        let heap = match &self.sketch {
            Sketch::Cms(s) => s.heap().heap(),
            Sketch::CountSketch(s) => s.heap().heap(),
        };
        let mut entries: Vec<(&[u8], f64)> = heap
            .iter()
            .map(|entry| match &entry.key {
                OwnedDataInput::Bytes(key) => (key.as_slice(), entry.count),
                _ => unreachable!("weighted frequency heap keys are tuple-key bytes"),
            })
            .collect();
        entries.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        entries
            .into_iter()
            .take(n)
            .map(|(key, score)| {
                let tuple = TupleKey::from_bytes(key).expect("heap keys are encoded tuple keys");
                let mut row = tuple.columns().iter().map(value).collect::<Vec<_>>();
                row.push(Value::Float64(score));
                row
            })
            .collect()
    }
    /// The counters, `depth` rows of `width` one after another, so a test
    /// can check per-row hashing or compare two states cell by cell.
    pub fn counters(&self) -> &[f64] {
        match &self.sketch {
            Sketch::Cms(s) => s.count_min_sketch().as_storage().as_slice(),
            Sketch::CountSketch(s) => s.count_sketch().as_storage().as_slice(),
        }
    }
    fn merge(&self, other: &Self) -> Result<Self, Error> {
        // sketchlib's merge panics on a shape mismatch.
        if self.algorithm() != other.algorithm() || self.shape() != other.shape() {
            return Err(Error::Invalid("weighted frequency shape mismatch".into()));
        }
        let weight_mass = self.weight_mass + other.weight_mass;
        if !weight_mass.is_finite() {
            return Err(Error::Operator("weighted frequency merge overflow".into()));
        }
        let mut merged = self.clone();
        merged.weight_mass = weight_mass;
        // Both merges re-estimate every candidate of both heaps.
        match (&mut merged.sketch, &other.sketch) {
            (Sketch::Cms(s), Sketch::Cms(o)) => s.merge(o),
            (Sketch::CountSketch(s), Sketch::CountSketch(o)) => s.merge(o),
            _ => unreachable!("the algorithms match"),
        }
        Ok(merged)
    }
}
impl AggregateCore for WeightedFrequency {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn merge_with(
        &self,
        other: &dyn AggregateCore,
    ) -> Result<Box<dyn AggregateCore>, Box<dyn std::error::Error + Send + Sync>> {
        let other = other
            .as_any()
            .downcast_ref::<Self>()
            .ok_or("weighted frequency state type mismatch")?;
        Ok(Box::new(self.merge(other)?))
    }
    /// sketchlib's count of the struct, the counters and the heap.
    fn approx_memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>() - std::mem::size_of::<Sketch>()
            + match &self.sketch {
                Sketch::Cms(s) => s.memory_bytes(),
                Sketch::CountSketch(s) => s.memory_bytes(),
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Merge adds fractional weights exactly and rejects other shapes.
    #[test]
    fn compatible_merge_preserves_fractional_weights() {
        let mut left = WeightedFrequency::new(FrequencyAlgorithm::Cms, 4096, 5, 8).unwrap();
        let mut right = left.clone();
        left.update(&[Value::Int64(7)], 0.125).unwrap();
        right.update(&[Value::Int64(7)], 0.25).unwrap();
        let merged = left.merge_with(&right).unwrap();
        let merged = merged.as_any().downcast_ref::<WeightedFrequency>().unwrap();
        assert!(matches!(merged.rows(1)[0][1], Value::Float64(0.375)));
        assert!(left
            .merge_with(&WeightedFrequency::new(FrequencyAlgorithm::Cms, 32, 5, 8).unwrap())
            .is_err());
    }

    // Count Sketch keeps signed weights per typed identity, and a rejected
    // weight leaves the state unchanged.
    #[test]
    fn count_sketch_signed_updates_and_rejected_weights() {
        let mut state =
            WeightedFrequency::new(FrequencyAlgorithm::CountSketch, 4096, 5, 8).unwrap();
        state.update(&[Value::Int64(1)], -10.5).unwrap();
        state.update(&[Value::Null], 0.125).unwrap();
        state.update(&[Value::Null], 0.1875).unwrap();
        let rows = state.rows(2);
        assert!(matches!(rows[0][0], Value::Null));
        assert!(matches!(rows[0][1], Value::Float64(0.3125)));
        assert!(matches!(rows[1][1], Value::Float64(-10.5)));
        let before = state.counters().to_vec();
        for weight in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(state.update(&[Value::Null], weight).is_err());
            assert_eq!(state.counters(), before.as_slice());
        }
        assert!(state.update(&[Value::Float64(f64::NAN)], 1.0).is_err());
    }

    // Count-Min rejects negative weights, and a sum past f64::MAX is refused
    // before it reaches a counter.
    #[test]
    fn cms_rejects_negative_weights_and_overflow() {
        let mut state = WeightedFrequency::new(FrequencyAlgorithm::Cms, 64, 3, 4).unwrap();
        assert!(state.update(&[Value::Int64(1)], -1.0).is_err());
        state.update(&[Value::Int64(1)], f64::MAX).unwrap();
        assert!(state.update(&[Value::Int64(2)], f64::MAX).is_err());
        assert!(state.counters().iter().all(|c| c.is_finite()));
        assert!(state.merge_with(&state).is_err());
    }

    // A heap resident's score follows later updates that share its cells.
    #[test]
    fn residents_are_rescored_after_every_update() {
        // One column: every key shares every cell.
        let mut state = WeightedFrequency::new(FrequencyAlgorithm::Cms, 1, 1, 2).unwrap();
        state.update(&[Value::Int64(1)], 1.0).unwrap();
        state.update(&[Value::Int64(2)], 2.0).unwrap();
        let scores: Vec<_> = state.rows(2).into_iter().map(|row| row[1].clone()).collect();
        assert!(matches!(scores[..], [Value::Float64(3.0), Value::Float64(3.0)]));
    }

    // Shapes: odd Count Sketch depth, any depth past MATRIX_MAX_ROWS (the
    // planner sizes Count Sketch heaps at depth 85), no zero dimension.
    #[test]
    fn shape_rules() {
        use FrequencyAlgorithm::*;
        assert!(WeightedFrequency::check_shape(CountSketch, 8, 2).is_err());
        assert!(WeightedFrequency::check_shape(Cms, 8, 2).is_ok());
        assert!(WeightedFrequency::check_shape(CountSketch, 8, 85).is_ok());
        assert!(WeightedFrequency::check_shape(Cms, 0, 3).is_err());
        assert!(WeightedFrequency::check_shape(Cms, 8, 0).is_err());
    }
}
