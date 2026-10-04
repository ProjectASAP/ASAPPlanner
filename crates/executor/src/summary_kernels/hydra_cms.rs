//! Hydra over Count-Min Sketch (`HydraKind::HydraCms`) over `asap_sketchlib::Hydra`.
//!
//! One `shared_rows × shared_columns` grid of Count-Min cells serves every
//! group: a group hashes to one cell per row, and that cell's Count-Min
//! counts the group's items. Colliding groups share a cell, so both readouts
//! only overestimate (weights are non-negative).
use crate::{AggregateCore, KernelError};
use asap_sketchlib::{
    hash_for_matrix_seeded, input::HydraCounter, CountMin, DataInput, FastPath, Hydra, Vector2D,
    HYDRA_SEED,
};
use planner_types::ir::schema::SketchStatistic;
use std::sync::Arc;

/// The single Hydra key column: a group's whole label tuple is one
/// subpopulation, so no record fans out into sub-groupings.
const GROUP: &str = "group";

/// One shared grid for all groups, sized by `HydraParams::HydraCms`.
#[derive(Debug, Clone)]
pub struct HydraCms {
    inner: Hydra,
    width: usize,
    depth: usize,
    /// Total update weight. Every counter is at most this, so keeping it
    /// within `i32` keeps sketchlib's `i32` counters from overflowing.
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

    /// Add `weight` occurrences of `item` to `group`.
    pub fn update(&mut self, group: &str, item: &str, weight: i32) -> Result<(), KernelError> {
        if weight < 0 {
            return Err("HydraCms weights must be non-negative".into());
        }
        let total = self.total + i64::from(weight);
        if total > i64::from(i32::MAX) {
            return Err("HydraCms total weight exceeds its i32 counters".into());
        }
        if weight > 0 {
            self.inner
                .update(&[group], &DataInput::Str(item), Some(weight))?;
        }
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

    /// Estimated total weight of `group`: the minimum over rows of the mass in
    /// the group's cell. Every row's cell holds the group's whole mass plus
    /// colliding groups', so the minimum is never below the true total, is at
    /// most the median Theorem 2 bounds, and is an exact integer.
    pub fn group_count(&self, group: &str) -> f64 {
        let hash = hash_for_matrix_seeded(
            HYDRA_SEED,
            self.inner.row_num,
            self.inner.col_num,
            &DataInput::Str(&subkey(group)),
        );
        self.inner
            .sketches
            .fast_query_min_with_key(&hash, &(), |cell, _, _, _| match cell {
                HydraCounter::CM(cms) => cms
                    .as_storage()
                    .row_slice(0)
                    .iter()
                    .map(|&c| i64::from(c))
                    .sum::<i64>(),
                _ => unreachable!("HydraCms cells are Count-Min"),
            }) as f64
    }

    /// Merge two grids built over disjoint inputs. Count-Min cells add
    /// counter-wise under the same hashes, so the result equals the grid built
    /// from the union of the inputs.
    pub fn merge(&self, other: &Self) -> Result<Self, KernelError> {
        // sketchlib's Count-Min merge panics on a shape mismatch.
        if self.shape() != other.shape() {
            return Err("HydraCms merge requires identical dimensions".into());
        }
        let total = self.total + other.total;
        if total > i64::from(i32::MAX) {
            return Err("HydraCms total weight exceeds its i32 counters".into());
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

/// sketchlib's canonical subkey for `{GROUP = group}` (`label:value`, with
/// `\`, `:` and `;` escaped). `Hydra::update` hashes this string; the group
/// total reads the same cells, so the encodings must agree. The kernel tests
/// compare group totals with exact counts and fail if they drift apart.
fn subkey(group: &str) -> String {
    let mut out = format!("{GROUP}:");
    for ch in group.chars() {
        if matches!(ch, '\\' | ':' | ';') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// One group's row state: the shared grid plus the group it reads. A grouped
/// SummaryAgg emits one row per group, all pointing at the same grid.
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

    /// A bare point count reads the group total; a valued one reads that
    /// item's frequency within the group.
    fn estimate(&self, query: &SketchStatistic) -> Result<f64, KernelError> {
        match query {
            SketchStatistic::PointCount { value: None, .. } => {
                Ok(self.grid.group_count(&self.group))
            }
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

    const ROWS: [(&str, &str, i32); 9] = [
        ("api", "checkout", 3),
        ("api", "checkout", 2),
        ("api", "search", 1),
        ("api", "a:b;c", 4),
        ("batch", "checkout", 7),
        ("batch", "export", 1),
        ("db;x:y", "checkout", 1),
        ("db;x:y", "vacuum", 6),
        ("db;x:y", "vacuum", 2),
    ];

    fn exact_point(group: &str, item: &str) -> f64 {
        ROWS.iter()
            .filter(|(g, i, _)| *g == group && *i == item)
            .map(|(_, _, w)| f64::from(*w))
            .sum()
    }

    fn exact_count(group: &str) -> f64 {
        ROWS.iter()
            .filter(|(g, _, _)| *g == group)
            .map(|(_, _, w)| f64::from(*w))
            .sum()
    }

    fn build(rows: &[(&str, &str, i32)], shared_columns: usize) -> HydraCms {
        let mut grid = HydraCms::new(3, shared_columns, 64, 3).unwrap();
        for (group, item, weight) in rows {
            grid.update(group, item, *weight).unwrap();
        }
        grid
    }

    // Group totals and per-group item counts stay within the CMS bound of
    // exact values; total N = 27, so the additive slack is e * N / 64 per level.
    #[test]
    fn group_and_point_estimates_within_cms_bound() {
        let grid = build(&ROWS, 64);
        let n = f64::from(ROWS.iter().map(|r| r.2).sum::<i32>());
        let slack = std::f64::consts::E * n / 64.0 * 2.0;
        for group in ["api", "batch", "db;x:y"] {
            let estimate = grid.group_count(group);
            let exact = exact_count(group);
            assert!(estimate >= exact && estimate <= exact + slack, "{group}");
            for item in ["checkout", "search", "export", "vacuum", "a:b;c"] {
                let estimate = grid.point(group, item).unwrap();
                let exact = exact_point(group, item);
                assert!(
                    estimate >= exact && estimate <= exact + slack,
                    "{group}/{item}: {estimate} vs {exact}"
                );
            }
        }
        // An unseen group reads (near) zero rather than another group's cell.
        assert!(grid.group_count("unseen") <= slack);
    }

    // With one shared column every group collides, so each total reads the
    // whole stream's mass: never below the exact value.
    #[test]
    fn collisions_only_overestimate() {
        let grid = build(&ROWS, 1);
        assert_eq!(grid.group_count("api"), 27.0);
        assert!(grid.point("batch", "checkout").unwrap() >= exact_point("batch", "checkout"));
    }

    // Merging grids over a split stream equals the grid over the whole stream.
    #[test]
    fn merge_equals_union_build() {
        let (left, right) = ROWS.split_at(4);
        let merged = build(left, 8).merge(&build(right, 8)).unwrap();
        let whole = build(&ROWS, 8);
        for group in ["api", "batch", "db;x:y"] {
            assert_eq!(merged.group_count(group), whole.group_count(group));
            assert_eq!(
                merged.point(group, "checkout").unwrap(),
                whole.point(group, "checkout").unwrap()
            );
        }
        assert!(build(left, 8).merge(&build(right, 16)).is_err());
    }

    // Group views answer only their own group and merge only with it.
    #[test]
    fn group_view_reads_and_merges_one_group() {
        let grid = Arc::new(build(&ROWS, 64));
        let view = |group: &str| HydraCmsGroup {
            grid: grid.clone(),
            group: group.into(),
        };
        let count = SketchStatistic::PointCount {
            key: planner_types::ir::scalar::ColumnRef::SampleValue,
            value: None,
        };
        assert_eq!(view("batch").estimate(&count).unwrap(), 8.0);
        let merged = view("batch").merge_with(&view("batch")).unwrap();
        assert_eq!(merged.estimate(&count).unwrap(), 16.0);
        assert!(view("api").merge_with(&view("batch")).is_err());
        assert!(view("api").estimate(&SketchStatistic::Cardinality).is_err());
    }

    // Updates that could overflow the i32 counters are rejected.
    #[test]
    fn rejects_negative_and_overflowing_weights() {
        let mut grid = HydraCms::new(1, 1, 1, 1).unwrap();
        assert!(grid.update("g", "x", -1).is_err());
        grid.update("g", "x", i32::MAX).unwrap();
        assert!(grid.update("g", "x", 1).is_err());
        assert!(grid.merge(&grid).is_err());
    }
}
