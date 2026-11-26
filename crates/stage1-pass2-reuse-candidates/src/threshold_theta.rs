//! Search-space pruning rule 1, threshold θ only
//! (`SearchSpacePruning::threshold_theta`): a state is listed at every θ of
//! its algorithm, and the candidates that pass a state on (a merge, a
//! chain, a `TimeRange` over states) carry its θ, but a θ another θ of the
//! same shape does at least as well, more cheaply, is in no optimal plan.
//!
//! The candidates linked by edges between equal θs (a build and the merges,
//! chains and ranges over it) form a θ set; its readers are the edges out
//! of it into candidates without θ (readouts). In one set, θ `s` is left
//! out when a different θ `t` of the same shape is no larger in every size
//! field, meets every reader `s` meets, and has a candidate in every place
//! `s` has one (same group, same operator): a plan reading `s` reads `t` in
//! its place, at no more cost, whatever readers share it. Its edges are
//! cut with [`RootPruneReason::AboveReaderThreshold`], and dead-candidate
//! elimination removes the candidates left without a consumer.
//!
//! Example: `topk(5, sum by (host) (m))` reads a `CmsWithHeap` state listed
//! at 240 θs (width × depth × heap size). Every θ that a smaller θ meeting
//! the same readers matches in every place is left out.
//!
//! Exactness rests on a larger θ never costing less (the cost model's size
//! and operation counts grow with every size field: counters, depth, heap
//! rows, DDSketch buckets, HLL registers, UnivMon layers; a measured
//! operation-cost table must be monotone the same way) and never meeting
//! fewer requirements. Sketches that keep up to `k` items (KLL, KMV, Hydra
//! over KLL) are left as they are: once `k` reaches the values a state
//! holds they keep every value and stop compacting, so a larger `k` can
//! cost less (a stored KLL window at k = 1,404 costs less than one at
//! k = 547 in the `deployment_exclusions` example). A set one of whose θs a
//! deployment exclusion's θ range matches is left as it is too: Stage 2 may
//! prune the smaller θ.

use std::collections::{BTreeMap, BTreeSet};

use asap_types::deployment_capabilities::DeploymentExclusion;
use asap_types::stages::{CandidateForMemoGroupId, Memo, MemoGroupId, RootPruneReason};
use asap_types::summary_type::{HydraParams, SketchParams};

use crate::dead_candidates::eliminate_dead_candidates;
use crate::edges::CandidateEdge;

/// Cuts every satisfied edge out of a θ another θ of its set does as well
/// (module doc), repeating with dead-candidate elimination until nothing
/// changes.
pub(crate) fn cut_edges_of_dominated_thetas(
    memo: &Memo,
    edges: &mut [CandidateEdge],
    exclusions: &[DeploymentExclusion],
) {
    let ranges: Vec<_> = exclusions
        .iter()
        .filter_map(|exclusion| exclusion.exclusion.sketch_parameter_range)
        .collect();
    let theta = |candidate: CandidateForMemoGroupId| {
        memo.candidate(candidate).logical_properties.parameters()
    };
    loop {
        let dead: BTreeSet<CandidateForMemoGroupId> = eliminate_dead_candidates(memo, edges)
            .into_iter()
            .map(|pruned| pruned.candidate)
            .collect();
        let live = |candidate: &CandidateForMemoGroupId| !dead.contains(candidate);
        let usable = |edge: &CandidateEdge| {
            edge.satisfies.is_ok() && live(&edge.producer) && live(&edge.consumer)
        };
        // The θ sets: live θ candidates linked by edges between equal θs.
        let mut sets = UnionFind::default();
        for edge in edges.iter().filter(|edge| usable(edge)) {
            if let (Some(produced), Some(consumed)) = (theta(edge.producer), theta(edge.consumer)) {
                if produced == consumed {
                    sets.union(edge.producer, edge.consumer);
                }
            }
        }
        // Per set and θ: the places it has candidates in (group, operator)
        // and the readers it meets (consumer inputs without θ).
        let mut values: BTreeMap<CandidateForMemoGroupId, Vec<ThetaInSet>> = BTreeMap::new();
        let position_of = |values: &mut BTreeMap<CandidateForMemoGroupId, Vec<ThetaInSet>>,
                           set: CandidateForMemoGroupId,
                           theta: &SketchParams| {
            let entries = values.entry(set).or_default();
            match entries.iter().position(|entry| entry.theta == *theta) {
                Some(position) => position,
                None => {
                    entries.push(ThetaInSet::new(theta.clone()));
                    entries.len() - 1
                }
            }
        };
        for (group, memo_group) in memo.groups().iter().enumerate() {
            for (index_in_group, candidate) in memo_group.candidates.iter().enumerate() {
                let id = CandidateForMemoGroupId {
                    group: MemoGroupId(group as u32),
                    index_in_group,
                };
                let Some(parameters) = candidate.logical_properties.parameters() else {
                    continue;
                };
                if !live(&id) {
                    continue;
                }
                let set = sets.find(id);
                let position = position_of(&mut values, set, parameters);
                let place = (id.group, format!("{:?}", candidate.expression.operator));
                values.get_mut(&set).expect("a set")[position]
                    .places
                    .insert(place);
            }
        }
        for edge in edges.iter().filter(|edge| usable(edge)) {
            if let (Some(produced), None) = (theta(edge.producer), theta(edge.consumer)) {
                let set = sets.find(edge.producer);
                let position = position_of(&mut values, set, produced);
                values.get_mut(&set).expect("a set")[position]
                    .readers
                    .insert((edge.consumer, edge.input));
            }
        }
        // The θs another θ of their set does as well.
        let mut dominated: BTreeMap<CandidateForMemoGroupId, Vec<bool>> = BTreeMap::new();
        for (set, entries) in &values {
            let applies = entries.iter().all(|entry| {
                costs_grow_with_every_field(&entry.theta)
                    && !ranges.iter().any(|range| range.contains(&entry.theta))
            });
            let flags = entries
                .iter()
                .map(|s| {
                    applies
                        && entries.iter().any(|t| {
                            t.theta != s.theta
                                && size_vector(&t.theta).no_larger_than(&size_vector(&s.theta))
                                && t.readers.is_superset(&s.readers)
                                && t.places.is_superset(&s.places)
                        })
                })
                .collect();
            dominated.insert(*set, flags);
        }
        let mut cut = 0;
        for edge in edges.iter_mut() {
            if !(edge.satisfies.is_ok() && live(&edge.producer) && live(&edge.consumer)) {
                continue;
            }
            let Some(produced) = theta(edge.producer) else {
                continue;
            };
            let set = sets.find(edge.producer);
            let position = values[&set]
                .iter()
                .position(|entry| entry.theta == *produced)
                .expect("a θ of the set");
            if dominated[&set][position] {
                edge.satisfies = Err(RootPruneReason::AboveReaderThreshold {
                    consumer: edge.consumer,
                    input: edge.input,
                });
                cut += 1;
            }
        }
        if cut == 0 {
            return;
        }
    }
}

/// One θ of a θ set: the places it has candidates in and the readers it
/// meets.
struct ThetaInSet {
    theta: SketchParams,
    places: BTreeSet<(MemoGroupId, String)>,
    readers: BTreeSet<(CandidateForMemoGroupId, usize)>,
}

impl ThetaInSet {
    fn new(theta: SketchParams) -> Self {
        Self {
            theta,
            places: BTreeSet::new(),
            readers: BTreeSet::new(),
        }
    }
}

/// Disjoint sets of candidates, each named by a representative.
#[derive(Default)]
struct UnionFind {
    parent: BTreeMap<CandidateForMemoGroupId, CandidateForMemoGroupId>,
}

impl UnionFind {
    fn find(&mut self, candidate: CandidateForMemoGroupId) -> CandidateForMemoGroupId {
        let parent = *self.parent.entry(candidate).or_insert(candidate);
        if parent == candidate {
            return candidate;
        }
        let root = self.find(parent);
        self.parent.insert(candidate, root);
        root
    }

    fn union(&mut self, a: CandidateForMemoGroupId, b: CandidateForMemoGroupId) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.parent.insert(b, a);
        }
    }
}

/// Whether a larger θ of `theta`'s shape never costs less: every shape but
/// the item-keeping ones (module doc).
fn costs_grow_with_every_field(theta: &SketchParams) -> bool {
    !matches!(
        theta,
        SketchParams::Kll { .. }
            | SketchParams::Kmv { .. }
            | SketchParams::Hydra(HydraParams::HydraKll { .. })
    )
}

/// A θ's shape and its size fields, each oriented so larger is larger (a
/// DDSketch's `1 / α`). Two θs compare only within one shape.
#[derive(Debug, Clone, PartialEq)]
struct SizeVector {
    shape: std::mem::Discriminant<SketchParams>,
    hydra: Option<std::mem::Discriminant<HydraParams>>,
    sizes: Vec<f64>,
}

impl SizeVector {
    /// Whether every size of `self` is at most `other`'s, in one shape.
    fn no_larger_than(&self, other: &SizeVector) -> bool {
        self.shape == other.shape
            && self.hydra == other.hydra
            && self.sizes.iter().zip(&other.sizes).all(|(a, b)| a <= b)
    }
}

fn size_vector(theta: &SketchParams) -> SizeVector {
    let f = f64::from;
    let (hydra, sizes) = match *theta {
        SketchParams::UnivMon {
            heap_size,
            sketch_rows,
            sketch_cols,
            layers,
        } => (
            None,
            vec![
                f(heap_size),
                f(sketch_rows),
                f(sketch_cols),
                f64::from(layers),
            ],
        ),
        SketchParams::Kll { k } | SketchParams::Kmv { k } => (None, vec![f(k)]),
        SketchParams::Cms { width, depth } | SketchParams::CountSketch { width, depth } => {
            (None, vec![f(width), f(depth)])
        }
        SketchParams::Hll { precision } => (None, vec![f64::from(precision)]),
        SketchParams::DDSketch { alpha } => (None, vec![1.0 / alpha.max(f64::MIN_POSITIVE)]),
        SketchParams::CmsWithHeap {
            width,
            depth,
            heap_size,
        }
        | SketchParams::CountSketchWithHeap {
            width,
            depth,
            heap_size,
        } => (None, vec![f(width), f(depth), f(heap_size)]),
        SketchParams::Hydra(ref hydra) => {
            let sizes = match *hydra {
                HydraParams::HydraKll { k, shared_buckets } => vec![f(k), f(shared_buckets)],
                HydraParams::HydraCms {
                    width,
                    depth,
                    shared_rows,
                    shared_columns,
                }
                | HydraParams::HydraCountSketch {
                    width,
                    depth,
                    shared_rows,
                    shared_columns,
                } => vec![f(width), f(depth), f(shared_rows), f(shared_columns)],
            };
            (Some(std::mem::discriminant(hydra)), sizes)
        }
    };
    SizeVector {
        shape: std::mem::discriminant(theta),
        hydra,
        sizes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heap(width: u32, depth: u32, heap_size: u32) -> SizeVector {
        size_vector(&SketchParams::CmsWithHeap {
            width,
            depth,
            heap_size,
        })
    }

    /// A smaller DDSketch α is the larger state; θs of two shapes do not
    /// compare.
    #[test]
    fn sizes_compare_within_one_shape_only() {
        let alpha = |alpha| size_vector(&SketchParams::DDSketch { alpha });
        assert!(alpha(0.05).no_larger_than(&alpha(0.01)));
        assert!(!alpha(0.01).no_larger_than(&alpha(0.05)));
        let count_sketch = size_vector(&SketchParams::CountSketchWithHeap {
            width: 55,
            depth: 7,
            heap_size: 10,
        });
        assert!(!heap(55, 7, 10).no_larger_than(&count_sketch));
    }

    /// Multi-field θs that do not compare dominate neither way.
    #[test]
    fn incomparable_thetas_dominate_neither_way() {
        let (a, b) = (heap(55, 7, 10), heap(28, 12, 10));
        assert!(!a.no_larger_than(&b) && !b.no_larger_than(&a));
        assert!(heap(28, 7, 10).no_larger_than(&a));
    }
}
