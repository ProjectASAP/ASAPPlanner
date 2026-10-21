//! Float64 weighted Count-Min and Count Sketch over `asap_sketchlib`'s `f64`
//! regular-path matrices, with a top-k heap or, at capacity 0, without one.
//!
//! An identity tuple is one `TupleKey`, hashed and retained as its bytes.
use crate::AggregateCore;
use crate::{values::Value, Error};
use asap_sketchlib::{
    CountMinSketch, CountMinSketchWithHeap, CountSketch, CountSketchWithHeap, DataInput, KeyColumn,
    OwnedDataInput, RegularPath, TupleKey, Vector2D,
};

/// Which matrix the state keeps.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FrequencyAlgorithm {
    Cms,
    CountSketch,
}

type Matrix = Vector2D<f64>;

#[derive(Clone)]
enum Sketch {
    Cms(CountMinSketch<Matrix, RegularPath>),
    CountSketch(CountSketch<Matrix, RegularPath>),
    CmsHeap(CountMinSketchWithHeap<Matrix, RegularPath>),
    CountSketchHeap(CountSketchWithHeap<Matrix, RegularPath>),
}

/// The shapes a state is built with: those `asap_sketchlib` builds (positive
/// `width` and `depth`), and an odd Count Sketch `depth`, so its median is
/// one row's estimate.
pub fn check_shape(algorithm: FrequencyAlgorithm, width: usize, depth: usize) -> Result<(), Error> {
    let shape = match algorithm {
        FrequencyAlgorithm::Cms => CountMinSketch::check_shape(depth, width),
        FrequencyAlgorithm::CountSketch if depth.is_multiple_of(2) => {
            Err(format!("Count Sketch depth {depth} is not odd"))
        }
        FrequencyAlgorithm::CountSketch => CountSketch::check_shape(depth, width),
    };
    shape.map_err(|problem| Error::Invalid(format!("invalid weighted frequency shape: {problem}")))
}

/// The canonical key of an identity tuple. A Float64 identity must be
/// finite; `-0.0` and `0.0` are one identity.
fn identity_key(values: &[Value]) -> Result<Vec<u8>, Error> {
    let columns = values
        .iter()
        .map(|value| match value {
            Value::Null => Ok(KeyColumn::Null),
            Value::Bool(v) => Ok(KeyColumn::Bool(*v)),
            Value::Int64(v) => Ok(KeyColumn::I64(*v)),
            Value::Float64(v) if v.is_finite() => Ok(KeyColumn::F64(*v)),
            Value::Float64(_) => Err(Error::Invalid(
                "non-finite weighted frequency identity".into(),
            )),
            Value::Utf8(v) => Ok(KeyColumn::String(v.to_string())),
            _ => Err(Error::Invalid(
                "unsupported weighted frequency identity".into(),
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    TupleKey::new(columns).to_bytes().map_err(Error::Invalid)
}

fn identity_values(key: &[u8]) -> Vec<Value> {
    let tuple = TupleKey::from_bytes(key).expect("a retained key is a tuple key");
    tuple
        .columns()
        .iter()
        .map(|column| match column {
            KeyColumn::Null => Value::Null,
            KeyColumn::Bool(v) => Value::Bool(*v),
            KeyColumn::I64(v) => Value::Int64(*v),
            KeyColumn::F64(v) => Value::Float64(*v),
            KeyColumn::String(v) => Value::Utf8(v.as_str().into()),
        })
        .collect()
}

#[derive(Clone)]
pub struct WeightedFrequency {
    sketch: Sketch,
    width: usize,
    depth: usize,
    capacity: usize,
}

impl std::fmt::Debug for WeightedFrequency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WeightedFrequency")
            .field("algorithm", &self.algorithm())
            .field("shape", &self.shape())
            .finish()
    }
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
        check_shape(algorithm, width, depth)?;
        Ok((algorithm, width, depth, capacity as usize))
    }

    /// Algorithm and `(width, depth, capacity)` shape, so a deployment can check
    /// a decoded state against its declared family.
    pub fn algorithm(&self) -> FrequencyAlgorithm {
        match self.sketch {
            Sketch::Cms(_) | Sketch::CmsHeap(_) => FrequencyAlgorithm::Cms,
            Sketch::CountSketch(_) | Sketch::CountSketchHeap(_) => FrequencyAlgorithm::CountSketch,
        }
    }
    pub fn shape(&self) -> (usize, usize, usize) {
        (self.width, self.depth, self.capacity)
    }

    /// Fails for a shape [`check_shape`] refuses.
    pub fn new(
        algorithm: FrequencyAlgorithm,
        width: usize,
        depth: usize,
        capacity: usize,
    ) -> Result<Self, Error> {
        check_shape(algorithm, width, depth)?;
        let sketch = match (algorithm, capacity) {
            (FrequencyAlgorithm::Cms, 0) => {
                Sketch::Cms(CountMinSketch::with_dimensions(depth, width))
            }
            (FrequencyAlgorithm::CountSketch, 0) => {
                Sketch::CountSketch(CountSketch::with_dimensions(depth, width))
            }
            (FrequencyAlgorithm::Cms, _) => {
                Sketch::CmsHeap(CountMinSketchWithHeap::new(depth, width, capacity))
            }
            (FrequencyAlgorithm::CountSketch, _) => {
                Sketch::CountSketchHeap(CountSketchWithHeap::new(depth, width, capacity))
            }
        };
        Ok(Self {
            sketch,
            width,
            depth,
            capacity,
        })
    }

    /// Adds `weight` to the identity tuple of `labels`, one `Utf8` identity
    /// per label, as the DAG's keyed build keys a label vector.
    pub(crate) fn update_labels(&mut self, labels: &[String], weight: f64) -> Result<(), Error> {
        let columns = labels
            .iter()
            .map(|label| KeyColumn::String(label.clone()))
            .collect();
        let key = TupleKey::new(columns).to_bytes().map_err(Error::Invalid)?;
        self.update_key(&key, weight)
    }

    /// The weight rule: finite, and nonnegative for Count-Min.
    pub(crate) fn check_weight(&self, weight: f64) -> Result<(), String> {
        match self.algorithm() {
            FrequencyAlgorithm::Cms => CountMinSketch::<Matrix, RegularPath>::check_weight(weight),
            FrequencyAlgorithm::CountSketch => {
                CountSketch::<Matrix, RegularPath>::check_weight(weight)
            }
        }
    }

    /// Empties the state in place, keeping its algorithm and shape.
    pub(crate) fn clear(&mut self) {
        match &mut self.sketch {
            Sketch::Cms(s) => s.clear(),
            Sketch::CountSketch(s) => s.clear(),
            Sketch::CmsHeap(s) => s.clear(),
            Sketch::CountSketchHeap(s) => s.clear(),
        }
    }

    /// Adds `weight` to the identity tuple `values`. An error, and the state
    /// unchanged, for a weight [`Self::check_weight`] refuses, an identity
    /// that is not Null, Bool, Int64, finite Float64 or Utf8, or an update
    /// that would carry a counter past `f64::MAX`.
    pub fn update(&mut self, values: &[Value], weight: f64) -> Result<(), Error> {
        let key = identity_key(values)?;
        self.update_key(&key, weight)
    }

    /// Adds `weight` to `key`. A heap state keeps the old kernel's
    /// membership: every resident is rescored after the counters change and
    /// before `key` is offered, so the heap admits `key` against the
    /// residents' current estimates, not the counts they had when last
    /// touched.
    fn update_key(&mut self, key: &[u8], weight: f64) -> Result<(), Error> {
        self.check_weight(weight).map_err(Error::Operator)?;
        let key = DataInput::Bytes(key);
        // The weight passed `check_weight`, so sketchlib refuses only a
        // counter carried past `f64::MAX`, leaving the sketch unchanged.
        let overflow = |e: String| Error::Operator(format!("weighted frequency sum overflow: {e}"));
        match &mut self.sketch {
            Sketch::Cms(s) => s.try_update_weighted(&key, weight).map_err(overflow)?,
            Sketch::CountSketch(s) => s.try_update_weighted(&key, weight).map_err(overflow)?,
            Sketch::CmsHeap(s) => {
                s.count_min_sketch_mut()
                    .try_update_weighted(&key, weight)
                    .map_err(overflow)?;
                s.rescore_heap();
                let estimate = s.estimate(&key);
                s.heap_mut().update(&key, estimate);
            }
            Sketch::CountSketchHeap(s) => {
                s.count_sketch_mut()
                    .try_update_weighted(&key, weight)
                    .map_err(overflow)?;
                s.rescore_heap();
                let estimate = s.estimate(&key);
                s.heap_mut().update(&key, estimate);
            }
        }
        Ok(())
    }

    fn estimate_key(&self, key: &[u8]) -> f64 {
        let key = DataInput::Bytes(key);
        match &self.sketch {
            Sketch::Cms(s) => s.estimate(&key),
            Sketch::CountSketch(s) => s.estimate(&key),
            Sketch::CmsHeap(s) => s.estimate(&key),
            Sketch::CountSketchHeap(s) => s.estimate(&key),
        }
    }

    /// Point estimate for one identity tuple: the minimum over rows for
    /// Count-Min, the sign-corrected median over rows for CountSketch.
    pub fn point_estimate(&self, values: &[Value]) -> Result<f64, Error> {
        Ok(self.estimate_key(&identity_key(values)?))
    }

    /// The keys the heap retains; none at capacity 0.
    fn retained_keys(&self) -> Vec<&[u8]> {
        let keys: Vec<&OwnedDataInput> = match &self.sketch {
            Sketch::Cms(_) | Sketch::CountSketch(_) => return Vec::new(),
            Sketch::CmsHeap(s) => s.heap().heap().iter().map(|e| &e.key).collect(),
            Sketch::CountSketchHeap(s) => s.heap().heap().iter().map(|e| &e.key).collect(),
        };
        keys.into_iter()
            .map(|key| match key {
                OwnedDataInput::Bytes(bytes) => bytes.as_slice(),
                _ => unreachable!("the state retains tuple-key bytes"),
            })
            .collect()
    }

    /// Up to `n` retained identity tuples, each followed by its current
    /// estimate as a Float64, by estimate descending, ties by key bytes
    /// ascending. Retention alone does not establish top-k completeness.
    pub fn rows(&self, n: usize) -> Vec<Vec<Value>> {
        let mut scored = self
            .retained_keys()
            .into_iter()
            .map(|key| (self.estimate_key(key), key))
            .collect::<Vec<_>>();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        scored
            .into_iter()
            .take(n)
            .map(|(score, key)| {
                let mut row = identity_values(key);
                row.push(Value::Float64(score));
                row
            })
            .collect()
    }

    /// The `depth x width` counters, row by row.
    pub fn counters(&self) -> &[f64] {
        match &self.sketch {
            Sketch::Cms(s) => s.as_storage().as_slice(),
            Sketch::CountSketch(s) => s.as_storage().as_slice(),
            Sketch::CmsHeap(s) => s.count_min_sketch().as_storage().as_slice(),
            Sketch::CountSketchHeap(s) => s.count_sketch().as_storage().as_slice(),
        }
    }

    /// Merge with a state of the same algorithm and shape, leaving both unchanged.
    pub fn merge(&self, other: &Self) -> Result<Self, Error> {
        // sketchlib's merges panic on a shape mismatch.
        if self.algorithm() != other.algorithm() || self.shape() != other.shape() {
            return Err(Error::Invalid("weighted frequency shape mismatch".into()));
        }
        let mut merged = self.clone();
        match (&mut merged.sketch, &other.sketch) {
            (Sketch::Cms(s), Sketch::Cms(o)) => s.merge(o),
            (Sketch::CountSketch(s), Sketch::CountSketch(o)) => s.merge(o),
            (Sketch::CmsHeap(s), Sketch::CmsHeap(o)) => s.merge(o),
            (Sketch::CountSketchHeap(s), Sketch::CountSketchHeap(o)) => s.merge(o),
            _ => unreachable!("one algorithm and capacity is one kind of sketch"),
        }
        // Both inputs are left unchanged, so a merge past `f64::MAX` is
        // refused by inspecting the copy.
        if merged.counters().iter().any(|c| !c.is_finite()) {
            return Err(Error::Operator("weighted frequency merge overflow".into()));
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
    /// This adapter's fields plus sketchlib's count of the sketch, which
    /// includes the sketch's own struct.
    fn approx_memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>() - std::mem::size_of::<Sketch>()
            + match &self.sketch {
                Sketch::Cms(s) => s.memory_bytes(),
                Sketch::CountSketch(s) => s.memory_bytes(),
                Sketch::CmsHeap(s) => s.memory_bytes(),
                Sketch::CountSketchHeap(s) => s.memory_bytes(),
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Top-k membership equals the old kernel's, which rescored every
    // candidate after every update and kept the `capacity` best: on small,
    // colliding sketches, an update also moves residents it did not touch,
    // and after every update the retained keys are the reference model's
    // (up to exact ties).
    #[test]
    fn membership_matches_rescoring_every_candidate_after_every_update() {
        for algorithm in [FrequencyAlgorithm::Cms, FrequencyAlgorithm::CountSketch] {
            let (width, depth, capacity) = (8, 3, 4);
            let mut state = WeightedFrequency::new(algorithm, width, depth, capacity).unwrap();
            // The reference: candidate keys, rescored from the same counters.
            let mut reference: Vec<i64> = Vec::new();
            let mut seed = 0x2545_f491_4f6c_dd1du64;
            for step in 0..400 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let key = (seed % 24) as i64;
                // Distinct fractional weights, so no two estimates tie.
                let weight = 1.0 + (seed >> 40) as f64 / (1u64 << 24) as f64;
                let weight = if algorithm == FrequencyAlgorithm::CountSketch && step % 3 == 0 {
                    -weight
                } else {
                    weight
                };
                state.update(&[Value::Int64(key)], weight).unwrap();
                if !reference.contains(&key) {
                    reference.push(key);
                }
                let score = |k: i64| state.point_estimate(&[Value::Int64(k)]).unwrap();
                reference.sort_by(|a, b| score(*b).total_cmp(&score(*a)));
                reference.truncate(capacity);
                // Keys that share every cell tie exactly, and the two
                // models break ties differently, so membership is compared by
                // the retained keys' current estimates.
                let scores = |keys: &[i64]| {
                    let mut scores: Vec<f64> = keys.iter().map(|&k| score(k)).collect();
                    scores.sort_by(f64::total_cmp);
                    scores
                };
                let got: Vec<i64> = state
                    .rows(capacity)
                    .into_iter()
                    .map(|row| match row[0] {
                        Value::Int64(k) => k,
                        _ => unreachable!(),
                    })
                    .collect();
                assert_eq!(
                    scores(&got),
                    scores(&reference),
                    "{algorithm:?} after update {step}: {got:?} vs {reference:?}"
                );
            }
        }
    }

    // The charge is sketchlib's count of the sketch plus this adapter's own
    // fields, each struct counted once.
    #[test]
    fn memory_charge_counts_the_sketch_once() {
        let state = WeightedFrequency::new(FrequencyAlgorithm::Cms, 64, 3, 4).unwrap();
        let Sketch::CmsHeap(sketch) = &state.sketch else {
            unreachable!()
        };
        assert_eq!(
            state.approx_memory_bytes(),
            std::mem::size_of::<WeightedFrequency>() - std::mem::size_of::<Sketch>()
                + sketch.memory_bytes()
        );
    }

    // Merge uses the same Float64 state representation and rejects other shapes.
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

    // Count Sketch keeps signed weights and ranks by signed estimate; typed
    // identities do not collide by formatting.
    #[test]
    fn signed_weights_and_typed_identities() {
        let mut state =
            WeightedFrequency::new(FrequencyAlgorithm::CountSketch, 4096, 5, 8).unwrap();
        state.update(&[Value::Int64(1)], -10.5).unwrap();
        state.update(&[Value::Utf8("1".into())], 0.125).unwrap();
        state.update(&[Value::Null], 0.3125).unwrap();
        let rows = state.rows(3);
        assert!(matches!(rows[0][0], Value::Null));
        assert!(matches!(rows[1][0], Value::Utf8(_)));
        assert!(matches!(rows[2][..], [Value::Int64(1), Value::Float64(score)] if score == -10.5));
    }

    // Weights outside the rule, non-finite or unsupported identities and an
    // update carrying a counter past f64::MAX are errors that leave the state
    // unchanged.
    #[test]
    fn invalid_updates_leave_the_state_unchanged() {
        let mut cms = WeightedFrequency::new(FrequencyAlgorithm::Cms, 64, 3, 4).unwrap();
        cms.update(&[Value::Int64(1)], f64::MAX).unwrap();
        let snapshot =
            |s: &WeightedFrequency| (s.counters().to_vec(), format!("{:?}", s.rows(usize::MAX)));
        let before = snapshot(&cms);
        for weight in [-1.0, f64::INFINITY, f64::NAN] {
            assert!(cms.update(&[Value::Null], weight).is_err());
        }
        // A counter carried past f64::MAX: the same key's cells again.
        assert!(cms.update(&[Value::Int64(1)], f64::MAX).is_err());
        for identity in [
            Value::Float64(f64::NAN),
            Value::Float64(f64::INFINITY),
            Value::Date(1),
        ] {
            assert!(cms.update(&[identity], 1.0).is_err());
        }
        assert_eq!(snapshot(&cms), before);
        let mut cs = WeightedFrequency::new(FrequencyAlgorithm::CountSketch, 64, 3, 4).unwrap();
        assert!(cs.update(&[Value::Null], -1.0).is_ok());
        cms.clear();
        assert!(cms.rows(4).is_empty());
    }

    // Signed zeros are one identity; an even Count Sketch depth and a zero
    // dimension are refused, and deep shapes are built.
    #[test]
    fn signed_zero_and_shape_rules() {
        let mut state = WeightedFrequency::new(FrequencyAlgorithm::Cms, 64, 3, 4).unwrap();
        state.update(&[Value::Float64(-0.0)], 1.0).unwrap();
        state.update(&[Value::Float64(0.0)], 2.0).unwrap();
        let rows = state.rows(4);
        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0][..], [Value::Float64(z), Value::Float64(3.0)] if z == 0.0));
        assert!(WeightedFrequency::new(FrequencyAlgorithm::CountSketch, 8, 2, 2).is_err());
        assert!(WeightedFrequency::new(FrequencyAlgorithm::Cms, 0, 3, 2).is_err());
        assert!(WeightedFrequency::new(FrequencyAlgorithm::Cms, 8, 0, 2).is_err());
        assert!(WeightedFrequency::new(FrequencyAlgorithm::Cms, 8, 32, 2).is_ok());
        assert!(WeightedFrequency::new(FrequencyAlgorithm::CountSketch, 8, 255, 0).is_ok());
    }

    // States deeper than ASAPv1's 20 rows build, update, merge and read.
    #[test]
    fn deep_states_update_merge_and_read() {
        for (algorithm, width, depth, capacity) in [
            (FrequencyAlgorithm::Cms, 272, 32, 4),
            (FrequencyAlgorithm::CountSketch, 64, 21, 0),
            (FrequencyAlgorithm::CountSketch, 64, 21, 4),
        ] {
            let mut state = WeightedFrequency::new(algorithm, width, depth, capacity).unwrap();
            state.update(&[Value::Int64(1)], 2.0).unwrap();
            let merged = state.merge(&state).unwrap();
            assert_eq!(merged.point_estimate(&[Value::Int64(1)]).unwrap(), 4.0);
            assert_eq!(merged.counters().len(), width * depth);
            assert_eq!(merged.rows(usize::MAX).len(), capacity.min(1));
        }
    }
}
