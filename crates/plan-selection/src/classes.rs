//! #1095 Steps 1 and 2: fold Stage 2's execution-scope copies into classes,
//! and mark the classes that hold state as shareable.
//!
//! Stage 2 copies a candidate once per execution scope that may need its
//! own instance (`physical-asap-optimization`'s `catalog/scopes.rs`) and
//! wires every consumer copy to every producer copy. The copies are
//! interchangeable, so selection only needs one class per set of equal
//! copies and the number of copies, the class's capacity: how many
//! instances of it may be active at once.
//!
//! ## Folding rule
//!
//! Two candidates are in one class when they are equal in:
//! - operator node (`PhysicalCandidate::operator_node`);
//! - timing and materialization;
//! - `may_produce_query_output` and the per-query keep flags
//!   (`may_keep_across_evaluations_per_sole_reader_query`);
//! - their reader constraints other than `ActivationRequires`, compared with
//!   the constrained candidate left out (`MinimumQueryReaders`,
//!   `HighestSharedQueryMaterialization`, `AllowedOutputs`,
//!   `IncompatibleOutputs`); any difference keeps them apart;
//! - per input, the set of classes of the candidates that may produce it.
//!
//! The last condition refers to classes, so folding is a fixpoint: start from
//! the partition by the other conditions and split classes until every
//! class's copies see the same producer classes per input (partition
//! refinement; the result is the coarsest such partition). A consumer whose
//! copies read different copies of one producer class therefore folds.
//!
//! `ActivationRequires` is Stage 2's symmetry rule between copies ("copy
//! `k + 1` is active only if copy `k` is"). Inside one class it is absorbed by
//! the capacity: when a later stage maps the class's `j` active instances to
//! its first `j` copies in ascending id order, every requirement from a
//! higher copy to a lower copy of the same class holds. Any other
//! `ActivationRequires` (between two classes, or from a lower copy to a
//! higher one) is a real constraint between specific candidates, so both
//! of its candidates are kept in classes of their own (capacity 1) and
//! folding is repeated. A class-level constraint between two such
//! single-copy classes means exactly what the candidate-level one did.
//! Stage 2 today never emits one; the rule is conservative.
//!
//! Classes are numbered in the order of their smallest copy id, and each
//! class lists its copies ascending, so the result depends only on the
//! input.
use std::collections::HashMap;

use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::{NonASAPOp, Operator, OperatorNode, OperatorResultKind};
use asap_types::physical::Materialization;
use asap_types::stages::{
    PhysicalASAPMemoCandidates, PhysicalCandidate, PhysicalCandidateId, ReaderConstraint,
};

/// Index into [`CandidateClasses::classes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct CandidateClassId(pub(crate) usize);

/// The state a shareable class holds (#1095 Step 2), which makes it worth
/// reading from several readers. Checked in this order; the first that
/// applies is recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum HeldState {
    /// `MatAtQueryKeptAcrossEvaluations`: a result kept across evaluations.
    KeptAcrossEvaluations,
    /// `MatAtQuery`: a result built once per evaluation for its readers.
    MaterializedAtQuery,
    /// `MatAtIngestion`: a result built as data arrives and kept for readers.
    MaterializedAtIngestion,
    /// A tumbling-pane chain (`PaneChain`), whatever its materialization.
    PaneChain,
    /// Unfinalized summary or accumulator state
    /// ([`OperatorResultKind::State`]), e.g. a `SummaryAgg` or `SummaryMerge`.
    SummaryState,
    /// A query-time `Scan`: it reads raw data the deployment retains, so it is
    /// where raw-data retention is paid. A shared scan of the shared-scan
    /// rule (#951) is a `Scan` too and is covered here.
    RetainedRawData,
}

/// The state a candidate of `node` at `timing` and `materialization` holds,
/// or `None` for a stateless one, which each reader computes for itself.
pub(crate) fn held_state(
    node: &OperatorNode,
    timing: ExecutionTiming,
    materialization: Materialization,
) -> Option<HeldState> {
    match materialization {
        Materialization::MatAtQueryKeptAcrossEvaluations(_) => {
            return Some(HeldState::KeptAcrossEvaluations)
        }
        Materialization::MatAtQuery(_) => return Some(HeldState::MaterializedAtQuery),
        Materialization::MatAtIngestion(_) => return Some(HeldState::MaterializedAtIngestion),
        Materialization::NotMaterialized => {}
    }
    if matches!(node.asap(), Some(asap_types::ir::ASAPOp::PaneChain { .. })) {
        Some(HeldState::PaneChain)
    } else if node.result_kind == OperatorResultKind::State {
        Some(HeldState::SummaryState)
    } else if timing == ExecutionTiming::QueryTime
        && matches!(node.operator, Operator::NonASAP(NonASAPOp::Scan { .. }))
    {
        Some(HeldState::RetainedRawData)
    } else {
        None
    }
}

/// One set of equal Stage 2 candidates (see the module documentation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CandidateClass {
    /// Its candidates, ascending. Never empty.
    pub(crate) copies: Vec<PhysicalCandidateId>,
    /// Per input of its operator node (in `map_children` order), the classes
    /// whose candidates may produce it, ascending.
    pub(crate) producer_classes_per_input: Vec<Vec<CandidateClassId>>,
    /// The state it holds; `None` for a stateless class.
    pub(crate) held_state: Option<HeldState>,
}

impl CandidateClass {
    /// How many instances of it may be active at once: its copy count.
    pub(crate) fn capacity(&self) -> usize {
        self.copies.len()
    }

    /// Whether it holds state, so readers may share one instance; a
    /// stateless class is costed per reader.
    pub(crate) fn is_shareable(&self) -> bool {
        self.held_state.is_some()
    }
}

/// Stage 2's candidates folded into classes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CandidateClasses {
    /// Indexed by [`CandidateClassId`], in the order of their smallest copy.
    pub(crate) classes: Vec<CandidateClass>,
    /// Indexed by [`PhysicalCandidateId`].
    class_of_candidate: Vec<CandidateClassId>,
}

impl CandidateClasses {
    /// The class candidate `candidate` belongs to.
    pub(crate) fn class_of(&self, candidate: PhysicalCandidateId) -> CandidateClassId {
        self.class_of_candidate[candidate.0]
    }

    /// Class `class`.
    pub(crate) fn class(&self, class: CandidateClassId) -> &CandidateClass {
        &self.classes[class.0]
    }
}

/// A reader constraint with its constrained candidate left out: what two
/// copies must agree on to fold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ReaderConstraintOfOneCandidate<'a> {
    MinimumQueryReaders {
        query_outputs: &'a [Vec<usize>],
        minimum: usize,
    },
    HighestSharedQueryMaterialization {
        query_outputs: &'a [Vec<usize>],
    },
    AllowedOutputs {
        outputs: &'a [usize],
    },
    IncompatibleOutputs {
        left: usize,
        right: usize,
    },
}

/// Folds `physical`'s candidates into classes (see the module
/// documentation for the rule).
pub(crate) fn fold_copies_into_classes(physical: &PhysicalASAPMemoCandidates) -> CandidateClasses {
    let candidate_count = physical.candidates.len();
    let constraint_ids_per_candidate = reader_constraint_ids_per_candidate(physical);
    let mut has_own_class = vec![false; candidate_count];
    let class_of_candidate = loop {
        let class_of_candidate = refine(physical, &constraint_ids_per_candidate, &has_own_class);
        let mut changed = false;
        for constraint in &physical.reader_constraints {
            let ReaderConstraint::ActivationRequires {
                candidate,
                required_candidate,
            } = *constraint
            else {
                continue;
            };
            let absorbed_by_capacity = class_of_candidate[candidate.0]
                == class_of_candidate[required_candidate.0]
                && required_candidate < candidate;
            if !absorbed_by_capacity {
                for endpoint in [candidate, required_candidate] {
                    changed |= !std::mem::replace(&mut has_own_class[endpoint.0], true);
                }
            }
        }
        if !changed {
            break class_of_candidate;
        }
    };
    let class_count = class_of_candidate
        .iter()
        .map(|class| class + 1)
        .max()
        .unwrap_or(0);
    let mut copies_per_class = vec![Vec::new(); class_count];
    for (candidate, &class) in class_of_candidate.iter().enumerate() {
        copies_per_class[class].push(PhysicalCandidateId(candidate));
    }
    let classes = copies_per_class
        .into_iter()
        .map(|copies| {
            let first: &PhysicalCandidate = &physical.candidates[copies[0].0];
            let producer_classes_per_input = first
                .producer_candidates_per_input
                .iter()
                .map(|producers| {
                    let mut classes: Vec<_> = producers
                        .iter()
                        .map(|producer| CandidateClassId(class_of_candidate[producer.0]))
                        .collect();
                    classes.sort_unstable();
                    classes.dedup();
                    classes
                })
                .collect();
            let held_state = held_state(
                physical.physical_node(copies[0]),
                first.timing,
                first.materialization,
            );
            CandidateClass {
                copies,
                producer_classes_per_input,
                held_state,
            }
        })
        .collect();
    CandidateClasses {
        classes,
        class_of_candidate: class_of_candidate
            .into_iter()
            .map(CandidateClassId)
            .collect(),
    }
}

/// Per candidate, its reader constraints other than `ActivationRequires`,
/// each as an id equal for equal constraints, ascending and without
/// repeats.
fn reader_constraint_ids_per_candidate(physical: &PhysicalASAPMemoCandidates) -> Vec<Vec<u32>> {
    let mut ids: HashMap<ReaderConstraintOfOneCandidate<'_>, u32> = HashMap::new();
    let mut per_candidate = vec![Vec::new(); physical.candidates.len()];
    for constraint in &physical.reader_constraints {
        let (candidate, constraint) = match constraint {
            ReaderConstraint::ActivationRequires { .. } => continue,
            ReaderConstraint::MinimumQueryReaders {
                candidate,
                query_outputs,
                minimum,
            } => (
                candidate,
                ReaderConstraintOfOneCandidate::MinimumQueryReaders {
                    query_outputs,
                    minimum: *minimum,
                },
            ),
            ReaderConstraint::HighestSharedQueryMaterialization {
                candidate,
                query_outputs,
            } => (
                candidate,
                ReaderConstraintOfOneCandidate::HighestSharedQueryMaterialization { query_outputs },
            ),
            ReaderConstraint::AllowedOutputs { candidate, outputs } => (
                candidate,
                ReaderConstraintOfOneCandidate::AllowedOutputs { outputs },
            ),
            ReaderConstraint::IncompatibleOutputs {
                candidate,
                left,
                right,
            } => (
                candidate,
                ReaderConstraintOfOneCandidate::IncompatibleOutputs {
                    left: *left,
                    right: *right,
                },
            ),
        };
        let next = ids.len() as u32;
        per_candidate[candidate.0].push(*ids.entry(constraint).or_insert(next));
    }
    for constraint_ids in &mut per_candidate {
        constraint_ids.sort_unstable();
        constraint_ids.dedup();
    }
    per_candidate
}

/// The coarsest partition of the candidates that separates every
/// difference the module documentation lists and is stable under "same
/// producer classes per input"; each candidate with `has_own_class` is alone.
/// Class `k` is the `k`-th class by smallest candidate id.
fn refine(
    physical: &PhysicalASAPMemoCandidates,
    constraint_ids_per_candidate: &[Vec<u32>],
    has_own_class: &[bool],
) -> Vec<usize> {
    // Initial partition: everything but the producers.
    type InitialKey<'a> = (
        usize,
        ExecutionTiming,
        Materialization,
        bool,
        &'a [bool],
        &'a [u32],
        Option<usize>,
        usize,
    );
    let mut initial_ids: HashMap<InitialKey<'_>, usize> = HashMap::new();
    let mut class_of_candidate: Vec<usize> = physical
        .candidates
        .iter()
        .enumerate()
        .map(|(id, candidate)| {
            let key = (
                candidate.operator_node.0,
                candidate.timing,
                candidate.materialization,
                candidate.may_produce_query_output,
                candidate
                    .may_keep_across_evaluations_per_sole_reader_query
                    .as_slice(),
                constraint_ids_per_candidate[id].as_slice(),
                has_own_class[id].then_some(id),
                candidate.producer_candidates_per_input.len(),
            );
            let next = initial_ids.len();
            *initial_ids.entry(key).or_insert(next)
        })
        .collect();
    let mut class_count = initial_ids.len();
    loop {
        // Producer sets share storage; compute each stored set's classes once.
        let mut producer_class_set_of_storage: HashMap<*const Vec<PhysicalCandidateId>, usize> =
            HashMap::new();
        let mut producer_class_set_ids: HashMap<Vec<usize>, usize> = HashMap::new();
        let mut next_ids: HashMap<(usize, Vec<usize>), usize> = HashMap::new();
        let next_class_of_candidate: Vec<usize> = physical
            .candidates
            .iter()
            .enumerate()
            .map(|(id, candidate)| {
                let producer_class_sets = candidate
                    .producer_candidates_per_input
                    .iter()
                    .map(|producers| {
                        let storage: *const Vec<PhysicalCandidateId> = &**producers;
                        *producer_class_set_of_storage
                            .entry(storage)
                            .or_insert_with(|| {
                                let mut classes: Vec<usize> = producers
                                    .iter()
                                    .map(|producer| class_of_candidate[producer.0])
                                    .collect();
                                classes.sort_unstable();
                                classes.dedup();
                                let next = producer_class_set_ids.len();
                                *producer_class_set_ids.entry(classes).or_insert(next)
                            })
                    })
                    .collect();
                let next = next_ids.len();
                *next_ids
                    .entry((class_of_candidate[id], producer_class_sets))
                    .or_insert(next)
            })
            .collect();
        // Refinement only splits classes, so an equal count means no change.
        let stable = next_ids.len() == class_count;
        class_count = next_ids.len();
        class_of_candidate = next_class_of_candidate;
        if stable {
            return class_of_candidate;
        }
    }
}

#[cfg(test)]
mod tests;
