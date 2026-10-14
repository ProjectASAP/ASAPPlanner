//! The generic family-capability table: what each summary family can do,
//! keyed by its [`FieldDataType`].
//!
//! The IR stays permissive: a `SummaryMerge` over any state family is
//! structurally valid. The planner rules and the executor consult this
//! table to decide whether a composition is sound. Today the table holds
//! the mergeability facts; the summary-capability facts (which statistics
//! a family answers) are added as further fields of [`FamilyCapability`].

use super::sketch::{ExactKind, GroupingStrategy, HydraKind, SketchAlgorithm};
use crate::pre_asap::schema::FieldDataType;

/// What one summary family can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FamilyCapability {
    /// How states of the family merge; `None` when the family has no sound
    /// merge.
    pub merge: Option<MergeCapability>,
    /// Whether a state subtracts out of another of the same family
    /// (`SummarySubtract`, reserved): the subtrahend's selection must lie
    /// inside the minuend's, and the result is the state of the difference.
    pub subtractable: bool,
}

/// How a family's states merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeCapability {
    /// How the selections of the merged states must relate.
    pub inputs: SelectionRelation,
    /// How the merged state's error relates to its parts' errors.
    pub error: MergedError,
}

/// How the selections of a merge's inputs must relate for the result to
/// keep the family's guarantee.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionRelation {
    /// No row in two inputs: the family counts a shared row twice.
    Disjoint,
    /// Inputs may share rows: the merge is idempotent.
    OverlapAllowed,
}

/// How a merged state's error relates to its parts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergedError {
    /// The merged state is exact, as its parts are.
    Exact,
    /// Parts built with the same parameters merge into a state with the
    /// guarantee of one build over the union of their rows: the error does
    /// not grow with the number of parts, so a state sized for the whole
    /// window may be built in parts.
    SameAsUnionBuild,
}

impl FamilyCapability {
    /// The capability of `family`. Families not listed have no capability
    /// (they fail closed).
    pub fn of(family: &FieldDataType) -> Self {
        use ExactKind as E;
        use SketchAlgorithm as S;
        let merge = |inputs, error| Some(MergeCapability { inputs, error });
        let (merge, subtractable) = match family {
            // Rate/Increase/IRate read the samples at the window edges, so
            // their parts do not compose into the window's value.
            FieldDataType::ExactAggregate(kind, _) => match kind {
                E::Sum | E::Count => (merge(SelectionRelation::Disjoint, MergedError::Exact), true),
                E::Min | E::Max => (
                    merge(SelectionRelation::OverlapAllowed, MergedError::Exact),
                    false,
                ),
                E::Rate | E::Increase | E::IRate => (None, false),
            },
            // Hydra over a linear sketch is a shared grid of linear
            // sketches: cells add and subtract. Hydra over KLL has no
            // accuracy model.
            FieldDataType::Sketch(_, GroupingStrategy::SharedMultiSubpopulation { kind, .. }) => {
                match kind {
                    HydraKind::HydraCms | HydraKind::HydraCountSketch => (
                        merge(SelectionRelation::Disjoint, MergedError::SameAsUnionBuild),
                        true,
                    ),
                    HydraKind::HydraKll => (None, false),
                }
            }
            FieldDataType::Sketch(kind, GroupingStrategy::PerSubpopulationInstance) => {
                match kind.algorithm() {
                    // Linear sketches: counters add and subtract.
                    S::Cms | S::CountSketch => (
                        merge(SelectionRelation::Disjoint, MergedError::SameAsUnionBuild),
                        true,
                    ),
                    S::Kll | S::DDSketch | S::UnivMon => (
                        merge(SelectionRelation::Disjoint, MergedError::SameAsUnionBuild),
                        false,
                    ),
                    S::Hll => (
                        merge(
                            SelectionRelation::OverlapAllowed,
                            MergedError::SameAsUnionBuild,
                        ),
                        false,
                    ),
                    // Merged heap top-k states have no accuracy model; KMV and
                    // Theta are not offered.
                    S::CmsWithHeap | S::CountSketchWithHeap | S::Kmv | S::Theta => (None, false),
                }
            }
            FieldDataType::Plain(_)
            | FieldDataType::Sample(..)
            | FieldDataType::Wavelet(..)
            | FieldDataType::StatModel(..) => (None, false),
        };
        Self {
            merge,
            subtractable,
        }
    }
}
