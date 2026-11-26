//! Search-space pruning rule 1, threshold θ only
//! (`SearchSpacePruning::threshold_theta`): a state group lists every θ of
//! its algorithm, but a reader needs only the least θ that meets its
//! requirement, its threshold, and two readers sharing one state need
//! only the least θ at least both thresholds. An edge from any other θ is
//! cut, with [`RootPruneReason::AboveReaderThreshold`], and dead-candidate
//! elimination removes the candidates left without a consumer.
//!
//! Example: `quantile_over_time(0.99, m[5m])` at relative 0.05 reads a KLL
//! state listed at k = 26 … 2863. Its readout's edges are satisfied from
//! k = 132 up; k = 132 is the threshold, so the edges from k = 269, 547,
//! 1404 and 2863 are cut and those builds (and the merges over them) are
//! dead.
//!
//! Exactness: a θ no smaller than another in every size field (a larger
//! KLL `k`, a wider or deeper Count-Min, a smaller DDSketch α) meets every
//! requirement the smaller one meets and costs at least as much, so a plan
//! reading a larger θ through a cut edge reads the threshold θ in its
//! place at no more cost; a θ two readers share is the least listed θ at
//! least both their thresholds, kept. θs of different shapes do not
//! compare and are each kept.

use std::collections::{BTreeMap, BTreeSet};

use asap_types::stages::{CandidateForMemoGroupId, Memo, MemoGroupId, RootPruneReason};
use asap_types::summary_type::{HydraParams, SketchParams};

use crate::dead_candidates::eliminate_dead_candidates;
use crate::edges::CandidateEdge;

/// Cuts every satisfied edge from a θ above the reader's threshold
/// (module doc), repeating with dead-candidate elimination until nothing
/// changes: a threshold is the least θ among the producers still alive.
/// Returns how many edges were cut.
pub(crate) fn cut_edges_above_thresholds(memo: &Memo, edges: &mut [CandidateEdge]) -> usize {
    let mut cut = 0;
    loop {
        let dead: BTreeSet<CandidateForMemoGroupId> = eliminate_dead_candidates(memo, edges)
            .into_iter()
            .map(|pruned| pruned.candidate)
            .collect();
        let live = |candidate: &CandidateForMemoGroupId| !dead.contains(candidate);
        let theta = |candidate: CandidateForMemoGroupId| {
            memo.candidate(candidate).logical_properties.parameters()
        };
        // Per live reader input, its satisfied edges from live θ producers.
        let mut readers: BTreeMap<(CandidateForMemoGroupId, usize), Vec<usize>> = BTreeMap::new();
        for (position, edge) in edges.iter().enumerate() {
            if edge.satisfies.is_ok()
                && live(&edge.consumer)
                && live(&edge.producer)
                && theta(edge.producer).is_some()
            {
                readers
                    .entry((edge.consumer, edge.input))
                    .or_default()
                    .push(position);
            }
        }
        // Per group, each reader's thresholds, then the least θs at least
        // two thresholds of one shape (two readers sharing the state).
        let mut needed: BTreeMap<MemoGroupId, Vec<SizeVector>> = BTreeMap::new();
        for positions in readers.values() {
            let sizes: Vec<SizeVector> = positions
                .iter()
                .filter_map(|&p| theta(edges[p].producer).map(size_vector))
                .collect();
            let group = edges[positions[0]].producer.group;
            let thresholds = needed.entry(group).or_default();
            for threshold in least(&sizes) {
                if !thresholds.contains(&threshold) {
                    thresholds.push(threshold);
                }
            }
        }
        for (group, thresholds) in needed.iter_mut() {
            let present: Vec<SizeVector> = memo.groups()[group.0 as usize]
                .candidates
                .iter()
                .enumerate()
                .filter(|&(index, _)| {
                    live(&CandidateForMemoGroupId {
                        group: *group,
                        index_in_group: index,
                    })
                })
                .filter_map(|(_, candidate)| candidate.logical_properties.parameters())
                .map(size_vector)
                .collect();
            let mut covers = Vec::new();
            for (i, a) in thresholds.iter().enumerate() {
                for b in &thresholds[i + 1..] {
                    let Some(join) = a.join(b) else {
                        continue;
                    };
                    let covering: Vec<SizeVector> = present
                        .iter()
                        .filter(|size| join.no_larger_than(size))
                        .cloned()
                        .collect();
                    covers.extend(least(&covering));
                }
            }
            for cover in covers {
                if !thresholds.contains(&cover) {
                    thresholds.push(cover);
                }
            }
        }
        let mut cut_now = 0;
        for positions in readers.values() {
            for &position in positions {
                let producer = edges[position].producer;
                let size = size_vector(theta(producer).expect("a θ producer"));
                if !needed[&producer.group].contains(&size) {
                    edges[position].satisfies = Err(RootPruneReason::AboveReaderThreshold {
                        consumer: edges[position].consumer,
                        input: edges[position].input,
                    });
                    cut_now += 1;
                }
            }
        }
        if cut_now == 0 {
            return cut;
        }
        cut += cut_now;
    }
}

/// A θ's shape and its size fields, each oriented so larger is larger
/// (a DDSketch's `1 / α`). Two θs compare only within one shape.
#[derive(Debug, Clone, PartialEq)]
struct SizeVector {
    shape: u8,
    sizes: Vec<f64>,
}

impl SizeVector {
    /// Whether every size of `self` is at most `other`'s, in one shape.
    fn no_larger_than(&self, other: &SizeVector) -> bool {
        self.shape == other.shape && self.sizes.iter().zip(&other.sizes).all(|(a, b)| a <= b)
    }

    /// The componentwise largest of both, in one shape; `None` across
    /// shapes.
    fn join(&self, other: &SizeVector) -> Option<SizeVector> {
        (self.shape == other.shape).then(|| SizeVector {
            shape: self.shape,
            sizes: self
                .sizes
                .iter()
                .zip(&other.sizes)
                .map(|(a, b)| a.max(*b))
                .collect(),
        })
    }
}

/// The minimal elements of `sizes`: no other one of its shape is no
/// larger and different.
fn least(sizes: &[SizeVector]) -> Vec<SizeVector> {
    let mut least: Vec<SizeVector> = Vec::new();
    for size in sizes {
        let dominated = sizes
            .iter()
            .any(|other| other != size && other.no_larger_than(size));
        if !dominated && !least.contains(size) {
            least.push(size.clone());
        }
    }
    least
}

fn size_vector(theta: &SketchParams) -> SizeVector {
    let v = |shape: u8, sizes: &[f64]| SizeVector {
        shape,
        sizes: sizes.to_vec(),
    };
    let f = f64::from;
    match *theta {
        SketchParams::UnivMon {
            heap_size,
            sketch_rows,
            sketch_cols,
            layers,
        } => v(
            0,
            &[f(heap_size), f(sketch_rows), f(sketch_cols), f(layers)],
        ),
        SketchParams::Kll { k } => v(1, &[f(k)]),
        SketchParams::Cms { width, depth } => v(2, &[f(width), f(depth)]),
        SketchParams::Hll { precision } => v(3, &[f(precision)]),
        SketchParams::DDSketch { alpha } => v(4, &[1.0 / alpha.max(f64::MIN_POSITIVE)]),
        SketchParams::CmsWithHeap {
            width,
            depth,
            heap_size,
        } => v(5, &[f(width), f(depth), f(heap_size)]),
        SketchParams::Kmv { k } => v(6, &[f(k)]),
        SketchParams::CountSketch { width, depth } => v(7, &[f(width), f(depth)]),
        SketchParams::CountSketchWithHeap {
            width,
            depth,
            heap_size,
        } => v(8, &[f(width), f(depth), f(heap_size)]),
        SketchParams::Hydra(HydraParams::HydraKll { k, shared_buckets }) => {
            v(9, &[f(k), f(shared_buckets)])
        }
        SketchParams::Hydra(HydraParams::HydraCms {
            width,
            depth,
            shared_rows,
            shared_columns,
        }) => v(10, &[f(width), f(depth), f(shared_rows), f(shared_columns)]),
        SketchParams::Hydra(HydraParams::HydraCountSketch {
            width,
            depth,
            shared_rows,
            shared_columns,
        }) => v(11, &[f(width), f(depth), f(shared_rows), f(shared_columns)]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kll(k: u32) -> SizeVector {
        size_vector(&SketchParams::Kll { k })
    }

    fn heap(width: u32, depth: u32, heap_size: u32) -> SizeVector {
        size_vector(&SketchParams::CmsWithHeap {
            width,
            depth,
            heap_size,
        })
    }

    /// One-parameter θs: the least is the smallest; a smaller DDSketch α
    /// is the larger state.
    #[test]
    fn the_least_one_parameter_theta_is_the_smallest() {
        assert_eq!(least(&[kll(547), kll(132), kll(1404)]), vec![kll(132)]);
        let alpha = |alpha| size_vector(&SketchParams::DDSketch { alpha });
        assert_eq!(least(&[alpha(0.01), alpha(0.05)]), vec![alpha(0.05)]);
    }

    /// Multi-parameter θs that do not compare are each least; the join of
    /// two is their componentwise largest, and θs of two shapes have none.
    #[test]
    fn incomparable_thetas_are_each_least_and_join_componentwise() {
        let (a, b) = (heap(55, 7, 10), heap(28, 12, 10));
        assert_eq!(least(&[a.clone(), b.clone()]), vec![a.clone(), b.clone()]);
        assert_eq!(a.join(&b), Some(heap(55, 12, 10)));
        assert_eq!(a.join(&kll(132)), None);
    }
}
