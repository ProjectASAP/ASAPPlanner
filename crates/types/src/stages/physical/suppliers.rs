//! Shared storage for ordered compatible suppliers. Sharing this storage never
//! ties together the independent selections of different consumer input slots.
use super::graph::PhysicalCandidateId;
use std::collections::HashSet;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct SupplierCandidates(Rc<Vec<PhysicalCandidateId>>);

impl From<Vec<PhysicalCandidateId>> for SupplierCandidates {
    fn from(values: Vec<PhysicalCandidateId>) -> Self {
        Self(Rc::new(values))
    }
}

impl FromIterator<PhysicalCandidateId> for SupplierCandidates {
    fn from_iter<T: IntoIterator<Item = PhysicalCandidateId>>(values: T) -> Self {
        Self::from(values.into_iter().collect::<Vec<_>>())
    }
}

impl Deref for SupplierCandidates {
    type Target = Vec<PhysicalCandidateId>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for SupplierCandidates {
    fn deref_mut(&mut self) -> &mut Self::Target {
        Rc::make_mut(&mut self.0)
    }
}

impl<'a> IntoIterator for &'a SupplierCandidates {
    type Item = &'a PhysicalCandidateId;
    type IntoIter = std::slice::Iter<'a, PhysicalCandidateId>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> IntoIterator for &'a mut SupplierCandidates {
    type Item = &'a mut PhysicalCandidateId;
    type IntoIter = std::slice::IterMut<'a, PhysicalCandidateId>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl IntoIterator for SupplierCandidates {
    type Item = PhysicalCandidateId;
    type IntoIter = std::vec::IntoIter<PhysicalCandidateId>;
    fn into_iter(self) -> Self::IntoIter {
        Rc::unwrap_or_clone(self.0).into_iter()
    }
}

/// Exact ordered-content interning. No sorting, deduplication, or compatibility
/// inference occurs here; malformed input remains visible to graph validation.
#[derive(Default)]
pub struct SupplierSetInterner {
    sets: HashSet<SupplierCandidates>,
}

impl SupplierSetInterner {
    pub fn intern(&mut self, values: impl Into<SupplierCandidates>) -> SupplierCandidates {
        let values = values.into();
        if let Some(existing) = self.sets.get(&values) {
            return existing.clone();
        }
        self.sets.insert(values.clone());
        values
    }
}

/// Count physical supplier-list storage separately from logical connections.
/// Distinct input positions remain distinct even when their storage is shared.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SupplierStorageMetrics {
    pub logical_connections: usize,
    pub unique_supplier_sets: usize,
    pub stored_supplier_ids: usize,
}

pub fn supplier_storage_metrics(
    physical: &super::PhysicalASAPMemoCandidates,
) -> SupplierStorageMetrics {
    let mut allocations = HashSet::new();
    let mut metrics = SupplierStorageMetrics::default();
    for suppliers in physical
        .candidates
        .iter()
        .flat_map(|candidate| &candidate.supplier_candidates_per_input)
    {
        metrics.logical_connections += suppliers.len();
        if allocations.insert(Rc::as_ptr(&suppliers.0)) {
            metrics.unique_supplier_sets += 1;
            metrics.stored_supplier_ids += suppliers.len();
        }
    }
    metrics
}

#[cfg(test)]
mod tests {
    use super::*;

    // Two slots can choose different producers even when their compatible lists
    // share one allocation; storage compression adds no selection equality.
    #[test]
    fn shared_supplier_storage_keeps_input_choices_independent() {
        use super::super::graph::{
            validate_selected_connections, ProducerConsumerConnection, QueryRootAssignment,
            SelectedCandidateGraph,
        };
        use super::super::memo_candidates::{
            CandidateForMemoGroupId, OperatorNodeOfCandidateForMemoGroup,
            OperatorNodeOfCandidateForMemoGroupId, PhysicalASAPMemoCandidates, PhysicalCandidate,
        };
        use crate::ir::properties::ExecutionTiming;
        use crate::ir::QueryRoot;
        use crate::physical::Materialization;
        use crate::stages::MemoGroupId;
        let mut interner = SupplierSetInterner::default();
        let choices = interner.intern(vec![PhysicalCandidateId(0), PhysicalCandidateId(1)]);
        use crate::ir::scalar::expr_ir::ScalarValue;
        use crate::ir::schema::{DataType, Field, Schema};
        use crate::ir::{NonASAPOp, Operator, OperatorNode, ScalarExpr};
        let node = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Values {
            rows: vec![vec![ScalarExpr::Literal(ScalarValue::Float64(1.0))]],
            schema: Schema::lifted(vec![Field::plain("value", DataType::Float64, false)], None),
        }))
        .unwrap();
        let physical = PhysicalASAPMemoCandidates::new(
            (0..3)
                .map(|group| OperatorNodeOfCandidateForMemoGroup {
                    candidates_for_memo_group: vec![CandidateForMemoGroupId {
                        group: MemoGroupId(group),
                        index: 0,
                    }],
                    is_implementation_root: true,
                    physical_node: Rc::clone(&node),
                })
                .collect(),
            (0..3)
                .map(|id| PhysicalCandidate {
                    operator_node: OperatorNodeOfCandidateForMemoGroupId(id),
                    timing: ExecutionTiming::QueryTime,
                    materialization: Materialization::NotMaterialized,
                    supplier_candidates_per_input: if id == 2 {
                        vec![choices.clone(), choices.clone()]
                    } else {
                        vec![]
                    },
                    may_produce_query_output: id == 2,
                    may_keep_across_evaluations_per_sole_reader_query: Vec::new(),
                })
                .collect(),
            vec![QueryRoot::Operator(MemoGroupId(2))],
            Vec::new(),
        );
        let selected = SelectedCandidateGraph {
            selected_candidates: vec![
                PhysicalCandidateId(0),
                PhysicalCandidateId(1),
                PhysicalCandidateId(2),
            ],
            connections: (0..2)
                .map(|requirement| ProducerConsumerConnection {
                    producer: PhysicalCandidateId(requirement),
                    consumer: PhysicalCandidateId(2),
                    requirement,
                })
                .collect(),
            query_roots: vec![QueryRootAssignment {
                query_root: 0,
                candidate: PhysicalCandidateId(2),
            }],
            modeled_cost: 0.0,
        };
        validate_selected_connections(
            3,
            |c| physical.candidates[c.0].supplier_candidates_per_input.len(),
            |c, input| {
                physical.candidates[c.0]
                    .supplier_candidates_per_input
                    .get(input)
            },
            &[&physical.query_output_candidates(0).collect::<Vec<_>>()],
            &physical.reader_constraints,
            &selected,
        )
        .unwrap();
        assert_eq!(
            supplier_storage_metrics(&physical),
            SupplierStorageMetrics {
                logical_connections: 4,
                unique_supplier_sets: 1,
                stored_supplier_ids: 2
            }
        );
    }

    // Equal ordered lists share one allocation; mutation of one input detaches
    // it and cannot silently change another input or an interner key.
    #[test]
    fn interned_inputs_detach_on_mutation() {
        let mut interner = SupplierSetInterner::default();
        let original = interner.intern(vec![PhysicalCandidateId(2), PhysicalCandidateId(1)]);
        let mut changed = interner.intern(vec![PhysicalCandidateId(2), PhysicalCandidateId(1)]);
        assert!(Rc::ptr_eq(&original.0, &changed.0));
        changed.retain(|id| id.0 == 1);
        assert_eq!(
            original.as_slice(),
            &[PhysicalCandidateId(2), PhysicalCandidateId(1)]
        );
        assert_eq!(changed.as_slice(), &[PhysicalCandidateId(1)]);
        assert!(!Rc::ptr_eq(&original.0, &changed.0));
        let repeated = interner.intern(original.clone());
        assert!(Rc::ptr_eq(&original.0, &repeated.0));
        let reversed = interner.intern(vec![PhysicalCandidateId(1), PhysicalCandidateId(2)]);
        assert!(!Rc::ptr_eq(&original.0, &reversed.0));
    }
}
