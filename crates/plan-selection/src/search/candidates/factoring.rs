//! Unit tests of Stage 2's terminal-reader factoring
//! (`asap_physical_asap_optimization::catalog`) that select with the solver.
use asap_physical_asap_optimization::catalog::test_support::{
    factor_terminal_readers, CandidateInventory, MemoGroupOutputKey,
    MemoGroupOutputToCompatibleProducersCache,
};
use asap_types::stages::MemoGroupId;
use asap_types::stages::PhysicalCandidateId;

#[cfg(test)]
mod tests {
    use super::*;
    use asap_logical_asap_optimization::test_support::lower_promql;
    use asap_types::ir::properties::AccuracyTarget;

    /// Node costs: states 0 and 1 cost 8 and 12, their readers 1 each (the
    /// factored reader keeps index 2).
    const NODE_COSTS: [f64; 4] = [8.0, 12.0, 1.0, 1.0];

    fn terminal_fixture() -> CandidateInventory {
        use asap_logical_asap_optimization::test_support::{cms_summary, estimate_over};
        use asap_types::ir::properties::AccuracyGuarantee;
        use std::rc::Rc;
        let state = Rc::new(cms_summary("count(m)"));
        let reader = estimate_over(state.clone(), AccuracyGuarantee::exact("fixture"));
        crate::search::candidates::configurations::inventory_for_tests(
            vec![
                (state.clone(), vec![]),
                (state, vec![]),
                (reader.clone(), vec![vec![PhysicalCandidateId(0)]]),
                (reader, vec![vec![PhysicalCandidateId(1)]]),
            ],
            vec![vec![PhysicalCandidateId(2), PhysicalCandidateId(3)]; 2],
        )
    }

    /// `inventory`'s nodes as candidates (states materialized at ingestion,
    /// readers at query time) at [`NODE_COSTS`], with execution scopes for
    /// two output references of one query.
    fn with_scopes(
        inventory: CandidateInventory,
    ) -> crate::search::candidates::dag::PhysicalCandidatesWithCosts {
        use crate::search::candidates::dag::{
            DagOfPhysicalCandidatesWithCosts, InputProducerCandidates, PhysicalCandidateWithCost,
            PhysicalCandidatesWithCosts, QueryOutputCandidates,
        };
        use asap_types::ir::properties::ExecutionTiming;
        use asap_types::physical::{Materialization, Medium};
        let count = inventory.operator_nodes_of_candidates_for_memo_groups.len();
        let dag = DagOfPhysicalCandidatesWithCosts {
            candidates: (0..count)
                .map(|id| PhysicalCandidateWithCost {
                    local_cost: NODE_COSTS[id],
                    producer_candidates_per_input: inventory.producer_nodes_per_input[id]
                        .iter()
                        .map(|suppliers| InputProducerCandidates {
                            candidates: suppliers.clone(),
                        })
                        .collect(),
                })
                .collect(),
            query_roots: inventory
                .query_output_nodes
                .iter()
                .map(|nodes| QueryOutputCandidates {
                    candidates: nodes.clone(),
                })
                .collect(),
            ..Default::default()
        };
        let nodes = (0..count)
            .map(|id| {
                (
                    inventory.operator_nodes_of_candidates_for_memo_groups[id]
                        .physical_node
                        .clone(),
                    if id < 2 {
                        (
                            ExecutionTiming::IngestionTime,
                            Materialization::MatAtIngestion(Medium::Memory),
                        )
                    } else {
                        (ExecutionTiming::QueryTime, Materialization::NotMaterialized)
                    },
                )
            })
            .collect();
        crate::search::candidates::scopes::expand_execution_scopes_for_tests(
            PhysicalCandidatesWithCosts::from_dag_for_tests(dag, nodes),
            &vec![1; count],
            &[0, 0],
        )
        .unwrap()
    }

    // Enumerate every output-to-state assignment, including split bindings for
    // two references of one query. State 1 models a higher-retention producer:
    // its different cost and identity must survive despite equal reader types.
    #[test]
    fn terminal_quotient_preserves_all_bindings_and_pay_once_costs() {
        use crate::search::candidates::dag::ReaderConstraint;
        use crate::search::milp::model::select_candidates;
        for assignment in [[0, 0], [0, 1], [1, 0], [1, 1]] {
            let original = with_scopes(terminal_fixture());
            let mut factored = terminal_fixture();
            assert_eq!(factor_terminal_readers(&mut factored, None).unwrap(), 1);
            assert_eq!(
                factored.operator_nodes_of_candidates_for_memo_groups.len(),
                3
            );
            let compressed = with_scopes(factored);
            assert!(compressed.dag.candidates.len() < original.dag.candidates.len());
            for mut configured in [original, compressed] {
                for state in 0..2 {
                    configured
                        .dag
                        .reader_constraints
                        .push(ReaderConstraint::AllowedOutputs {
                            candidate: PhysicalCandidateId(state),
                            outputs: assignment
                                .iter()
                                .enumerate()
                                .filter_map(|(output, &producer)| {
                                    (producer == state).then_some(output)
                                })
                                .collect(),
                        });
                }
                let selected = select_candidates(&configured.dag)
                    .unwrap()
                    .expect("every original assignment remains feasible");
                let expected = if assignment[0] == assignment[1] {
                    [8.0, 12.0][assignment[0]] + 1.0
                } else {
                    22.0
                };
                assert_eq!(selected.modeled_cost, expected, "assignment {assignment:?}");
            }
        }
    }

    #[test]
    fn duplicate_terminal_interfaces_reduce_scoped_candidates_and_connections() {
        let build = || {
            let mut inventory = terminal_fixture();
            for reader in &mut inventory.producer_nodes_per_input[2..] {
                reader[0] = vec![PhysicalCandidateId(0), PhysicalCandidateId(1)].into();
            }
            inventory
        };
        let original = with_scopes(build());
        let mut compact = build();
        assert_eq!(factor_terminal_readers(&mut compact, None).unwrap(), 1);
        let compact = with_scopes(compact);
        let counts = |graph: &crate::search::candidates::dag::DagOfPhysicalCandidatesWithCosts| {
            (
                graph.candidates.len(),
                graph
                    .candidates
                    .iter()
                    .flat_map(|candidate| &candidate.producer_candidates_per_input)
                    .map(|input| input.candidates.len())
                    .sum::<usize>(),
            )
        };
        assert_eq!(counts(&original.dag), (6, 8));
        assert_eq!(counts(&compact.dag), (4, 4));
    }

    #[test]
    fn terminal_quotient_preserves_interfaces_and_internal_consumers() {
        use asap_types::ir::properties::AccuracyGuarantee;
        use std::rc::Rc;
        let mut accuracy = terminal_fixture();
        let reader = &mut accuracy.operator_nodes_of_candidates_for_memo_groups[3].physical_node;
        let mut other = (**reader).clone();
        other.guarantee = Some(AccuracyGuarantee::exact("different proof contract"));
        *reader = Rc::new(other);
        assert_eq!(factor_terminal_readers(&mut accuracy, None).unwrap(), 0);
        let mut eligibility = terminal_fixture();
        eligibility.query_output_nodes[1] = vec![PhysicalCandidateId(3)];
        assert_eq!(factor_terminal_readers(&mut eligibility, None).unwrap(), 0);
        let mut internal = terminal_fixture();
        internal.producer_nodes_per_input[0].push(vec![PhysicalCandidateId(2)].into());
        assert_eq!(factor_terminal_readers(&mut internal, None).unwrap(), 0);
    }

    // Logical ownership (including window semantics), closedness and required
    // keys remain distinct even when their fields have identical types.
    #[test]
    fn compatibility_cache_keeps_every_expected_output_requirement() {
        let expected = lower_promql("latency", AccuracyTarget::exact());
        let group = MemoGroupId(1);
        let key = MemoGroupOutputKey::new(group, &expected).unwrap();
        let mut cache = MemoGroupOutputToCompatibleProducersCache::default();
        let suppliers = vec![PhysicalCandidateId(3), PhysicalCandidateId(7)];
        cache.remember(key, suppliers.clone());
        assert_eq!(
            &*cache
                .get(&MemoGroupOutputKey::new(group, &expected).unwrap())
                .unwrap(),
            &suppliers
        );
        assert!(cache
            .get(&MemoGroupOutputKey::new(MemoGroupId(2), &expected).unwrap())
            .is_none());
        let mut closed = (*expected).clone();
        closed.schema.closed = !closed.schema.closed;
        assert!(cache
            .get(&MemoGroupOutputKey::new(group, &closed).unwrap())
            .is_none());
        let mut keys = (*expected).clone();
        keys.schema.unique_keys.push(vec![]);
        assert!(cache
            .get(&MemoGroupOutputKey::new(group, &keys).unwrap())
            .is_none());
    }

    // Storage sharing never couples the two consumers' producer decisions.
    #[test]
    fn two_reader_bindings_keep_independent_producer_lists() {
        let expected = lower_promql("latency", AccuracyTarget::exact());
        let key = || MemoGroupOutputKey::new(MemoGroupId(1), &expected).unwrap();
        let mut cache = MemoGroupOutputToCompatibleProducersCache::default();
        cache.remember(key(), vec![PhysicalCandidateId(1), PhysicalCandidateId(2)]);
        let mut first = cache.get(&key()).unwrap();
        let second = cache.get(&key()).unwrap();
        first.retain(|id| id.0 == 1);
        assert_eq!(&*first, &[PhysicalCandidateId(1)]);
        assert_eq!(&*second, &[PhysicalCandidateId(1), PhysicalCandidateId(2)]);
        assert_eq!(cache.get(&key()).unwrap(), second);
    }
    // Exhaust every two-reader binding, charging each retained producer once.
    // A longer retention requirement has its own logical group and cannot
    // accidentally inherit the shorter reader's producer set from the cache.
    #[test]
    fn cached_interfaces_preserve_exhaustive_sharing_and_retention_choices() {
        let expected = lower_promql("latency", AccuracyTarget::exact());
        let short = vec![PhysicalCandidateId(0), PhysicalCandidateId(1)];
        let long = vec![PhysicalCandidateId(1)];
        let mut cache = MemoGroupOutputToCompatibleProducersCache::default();
        let key = |group| MemoGroupOutputKey::new(MemoGroupId(group), &expected).unwrap();
        cache.remember(key(0), short.clone());
        cache.remember(key(1), long.clone());
        let enumerate = |left: &[PhysicalCandidateId], right: &[PhysicalCandidateId]| {
            let costs = [8, 12];
            left.iter()
                .flat_map(|a| {
                    right.iter().map(move |b| {
                        let cost = costs[a.0] + if a == b { 0 } else { costs[b.0] } + 2;
                        ((a.0, b.0), cost)
                    })
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        for (right, group, optimum) in [(&short, 0, 10), (&long, 1, 14)] {
            let reference = enumerate(&short, right);
            let factored = enumerate(
                &cache.get(&key(0)).unwrap(),
                &cache.get(&key(group)).unwrap(),
            );
            assert_eq!(factored, reference);
            assert_eq!(factored.values().min(), Some(&optimum));
            assert_eq!(
                factored.get(&(0, 1)),
                Some(&22),
                "two distinct producer bindings remain feasible"
            );
        }
    }
}
