//! Hydra over Count-Min Sketch (`HydraKind::HydraCms`) over `asap_sketchlib::Hydra`.
//!
//! One `shared_rows × shared_columns` grid of Count-Min cells serves every
//! group: a group hashes to one cell per row, and that cell's Count-Min
//! adds up the group's item weights (1 per row for a count). Colliding groups share a cell, so a point count
//! only overestimates.
//!
//! The kernel answers a group's item frequency (Hydra's median estimator).
//! A group's total needs a group-mass query that `asap_sketchlib`'s Hydra
//! does not offer, so it is not answered here.
use crate::{AggregateCore, KernelError};
use asap_sketchlib::{input::HydraCounter, CountMin, DataInput, FastPath, Hydra, Vector2D};
use planner_types::post_asap::SketchStatistic;
use std::sync::Arc;

/// The single Hydra key column: a group's whole label tuple is one
/// subpopulation, so no row fans out to sub-groupings.
const GROUP: &str = "group";

/// One shared grid for all groups, sized by `HydraParams::HydraCms`.
#[derive(Debug, Clone)]
pub struct HydraCms {
    inner: Hydra,
    width: usize,
    depth: usize,
    /// Total weight added. Every counter is at most this, so keeping it within
    /// `i32` keeps sketchlib's `i32` counters from overflowing.
    total: i64,
}

impl HydraCms {
    pub fn new(
        shared_rows: usize,
        shared_columns: usize,
        width: usize,
        depth: usize,
    ) -> Result<Self, KernelError> {
        if [shared_rows, shared_columns, width, depth].contains(&0) {
            return Err("HydraCms dimensions must be positive".into());
        }
        let cell = HydraCounter::CM(CountMin::<Vector2D<i32>, FastPath>::with_dimensions(
            depth, width,
        ));
        Ok(Self {
            inner: Hydra::with_schema(shared_rows, shared_columns, [GROUP], cell)?,
            width,
            depth,
            total: 0,
        })
    }

    /// `(shared_rows, shared_columns, width, depth)`.
    pub fn shape(&self) -> (usize, usize, usize, usize) {
        (
            self.inner.row_num,
            self.inner.col_num,
            self.width,
            self.depth,
        )
    }

    /// Add `weight` occurrences of `item` in `group` (1 for a row count).
    /// sketchlib's Hydra adds `i32` weights, so weights are non-negative
    /// integers and their total stays within `i32`.
    pub fn count(&mut self, group: &str, item: &str, weight: i32) -> Result<(), KernelError> {
        if weight < 0 {
            return Err("HydraCms weights must be non-negative".into());
        }
        let total = self.total + i64::from(weight);
        if total > i64::from(i32::MAX) {
            return Err("HydraCms count exceeds its i32 counters".into());
        }
        self.inner
            .update(&[group], &DataInput::Str(item), Some(weight))?;
        self.total = total;
        Ok(())
    }

    /// Estimated frequency of `item` within `group`: Hydra's median over rows
    /// of each cell's Count-Min estimate (the paper's Theorem 2 estimator).
    pub fn point(&self, group: &str, item: &str) -> Result<f64, KernelError> {
        Ok(self
            .inner
            .query_frequency(&[Some(group)], &DataInput::Str(item))?)
    }

    /// Merge two grids built over disjoint inputs. Count-Min cells add
    /// counter-wise under the same hashes, so the result equals the grid built
    /// from the union of the inputs.
    pub fn merge(&self, other: &Self) -> Result<Self, KernelError> {
        // sketchlib's Count-Min merge panics on a cell shape mismatch.
        if self.shape() != other.shape() {
            return Err("HydraCms merge requires identical dimensions".into());
        }
        let total = self.total + other.total;
        if total > i64::from(i32::MAX) {
            return Err("HydraCms count exceeds its i32 counters".into());
        }
        let mut merged = self.clone();
        merged.inner.merge(&other.inner)?;
        merged.total = total;
        Ok(merged)
    }

    pub fn approx_memory_bytes(&self) -> usize {
        let (rows, columns, width, depth) = self.shape();
        std::mem::size_of::<Self>() + rows * columns * (64 + width * depth * 4)
    }
}

/// One group's row state: the shared grid plus the group it reads. A grouped
/// `SummaryAgg` emits one row per group, all pointing at the same grid.
#[derive(Debug, Clone)]
pub struct HydraCmsGroup {
    pub grid: Arc<HydraCms>,
    pub group: String,
}

impl AggregateCore for HydraCmsGroup {
    fn clone_boxed_core(&self) -> Box<dyn AggregateCore> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn merge_with(&self, other: &dyn AggregateCore) -> Result<Box<dyn AggregateCore>, KernelError> {
        let other = other
            .as_any()
            .downcast_ref::<Self>()
            .ok_or("HydraCms merges only with HydraCms")?;
        if self.group != other.group {
            return Err("HydraCms merge requires the same group".into());
        }
        Ok(Box::new(Self {
            grid: Arc::new(self.grid.merge(&other.grid)?),
            group: self.group.clone(),
        }))
    }

    /// A valued point count reads that item's frequency within the group.
    fn estimate(&self, query: &SketchStatistic) -> Result<f64, KernelError> {
        match query {
            SketchStatistic::PointCount {
                value: Some(item), ..
            } => self.grid.point(&self.group, item),
            _ => Err(format!("{query:?} is not supported by HydraCms").into()),
        }
    }

    /// The whole shared grid: groups of one build share it, so this
    /// over-reserves, which is the safe direction.
    fn approx_memory_bytes(&self) -> usize {
        self.grid.approx_memory_bytes() + self.group.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROWS: [(&str, &str); 9] = [
        ("api", "checkout"),
        ("api", "checkout"),
        ("api", "search"),
        ("api", "a:b;c"),
        ("batch", "checkout"),
        ("batch", "export"),
        ("db;x:y", "checkout"),
        ("db;x:y", "vacuum"),
        ("db;x:y", "vacuum"),
    ];

    fn exact(group: &str, item: &str) -> f64 {
        ROWS.iter().filter(|r| **r == (group, item)).count() as f64
    }

    fn build(rows: &[(&str, &str)], shared_columns: usize) -> HydraCms {
        let mut grid = HydraCms::new(3, shared_columns, 64, 3).unwrap();
        for (group, item) in rows {
            grid.count(group, item, 1).unwrap();
        }
        grid
    }

    // Point counts stay within the CMS bound of exact values; N = 9, so the
    // additive slack is e·N/64 per level.
    #[test]
    fn point_estimates_within_cms_bound() {
        let grid = build(&ROWS, 64);
        let slack = std::f64::consts::E * ROWS.len() as f64 / 64.0 * 2.0;
        for group in ["api", "batch", "db;x:y"] {
            for item in ["checkout", "search", "export", "vacuum", "a:b;c"] {
                let estimate = grid.point(group, item).unwrap();
                let truth = exact(group, item);
                assert!(
                    estimate >= truth && estimate <= truth + slack,
                    "{group}/{item}: {estimate} vs {truth}"
                );
            }
        }
    }

    // With one shared column every group collides, so a point count reads
    // the item's count over the whole stream: never below the exact value.
    #[test]
    fn collisions_only_overestimate() {
        let grid = build(&ROWS, 1);
        assert_eq!(grid.point("batch", "checkout").unwrap(), 4.0);
    }

    // Merging grids over a split stream equals the grid over the whole stream.
    #[test]
    fn merge_equals_union_build() {
        let (left, right) = ROWS.split_at(4);
        let merged = build(left, 8).merge(&build(right, 8)).unwrap();
        let whole = build(&ROWS, 8);
        for group in ["api", "batch", "db;x:y"] {
            for item in ["checkout", "vacuum"] {
                assert_eq!(
                    merged.point(group, item).unwrap(),
                    whole.point(group, item).unwrap()
                );
            }
        }
        assert!(build(left, 8).merge(&build(right, 16)).is_err());
    }

    // Group views answer only their own group's item counts and merge only
    // with the same group; a group total is not answered.
    #[test]
    fn group_view_reads_and_merges_one_group() {
        let grid = Arc::new(build(&ROWS, 64));
        let view = |group: &str| HydraCmsGroup {
            grid: grid.clone(),
            group: group.into(),
        };
        let checkout = SketchStatistic::PointCount {
            key: planner_types::pre_asap::ColumnRef::Named("item".into()),
            value: Some("checkout".into()),
        };
        assert_eq!(view("batch").estimate(&checkout).unwrap(), 1.0);
        let merged = view("batch").merge_with(&view("batch")).unwrap();
        assert_eq!(merged.estimate(&checkout).unwrap(), 2.0);
        assert!(view("api").merge_with(&view("batch")).is_err());
        let total = SketchStatistic::PointCount {
            key: planner_types::pre_asap::ColumnRef::SampleValue,
            value: None,
        };
        assert!(view("api").estimate(&total).is_err());
        assert!(view("api").estimate(&SketchStatistic::Cardinality).is_err());
    }

    // Counts that could overflow the i32 counters are rejected.
    #[test]
    fn rejects_overflowing_counts() {
        let mut grid = HydraCms::new(1, 1, 1, 1).unwrap();
        grid.total = i64::from(i32::MAX);
        assert!(grid.count("g", "x", 1).is_err());
        assert!(grid.merge(&HydraCms::new(1, 1, 1, 1).unwrap()).is_ok());
        assert!(grid.merge(&grid).is_err());
    }
}
