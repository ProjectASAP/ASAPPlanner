//! Unit tests of Stage 2's reconstruction of a selection
//! (`asap_physical_asap_optimization::catalog`) that select with the solver.
use std::rc::Rc;

use crate::search::candidates::dag::{
    DagOfPhysicalCandidatesWithCosts, PhysicalCandidatesWithCosts,
};
use crate::search::candidates::graph::SelectedCandidateGraph;
use asap_physical_asap_optimization::catalog::reconstruct_selected_candidates;
use asap_physical_asap_optimization::catalog::test_support::{
    ordered_inputs, output_contract_satisfied, paired_node_identities,
};
use asap_types::ir::OperatorNode;
use asap_types::stages::PhysicalCandidateId;

/// Operator output references in `graph.query_roots` order, candidate `i`
/// running `nodes[i]`, after Stage 3 validated `selected` against `graph`. A
/// caller with scalar roots must map these references back into its original
/// scalar expressions (and preserve literal-only roots); an output reference
/// is not a whole query. Nodes' timing/materialization are deliberately
/// discarded: Stage 2 must apply the selected materialization policy to the
/// reconstructed connections.
#[cfg(test)]
pub(in crate::search) fn reconstruct_candidate_outputs(
    graph: &DagOfPhysicalCandidatesWithCosts,
    selected: &SelectedCandidateGraph,
    nodes: &[Rc<OperatorNode>],
) -> Result<Vec<Rc<OperatorNode>>, String> {
    use asap_types::ir::properties::ExecutionTiming;
    use asap_types::physical::Materialization;
    let configured = PhysicalCandidatesWithCosts::from_dag_for_tests(
        graph.clone(),
        nodes
            .iter()
            .map(|node| {
                (
                    Rc::clone(node),
                    (ExecutionTiming::QueryTime, Materialization::NotMaterialized),
                )
            })
            .collect(),
    );
    configured.validate_selected(selected)?;
    Ok(
        reconstruct_selected_candidates(&configured.physical, &selected.to_selected_candidates())?
            .outputs,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::candidates::dag::{
        InputProducerCandidates, PhysicalCandidateWithCost, QueryOutputCandidates,
    };
    use crate::search::milp::model::select_candidates;
    use asap_logical_asap_optimization::test_support::lower_promql;

    // Physical identity is reflexive for NaN, without equating different
    // non-finite quantile parameters that JSON would serialize as null.
    #[test]
    fn unchanged_nan_parameter_preserves_identity() {
        let lower =
            |query| lower_promql(query, asap_types::ir::properties::AccuracyTarget::exact());
        let before = lower("quantile by(job)(NaN, data)");
        let unchanged = lower("quantile by(job)(NaN, data)");
        assert!(paired_node_identities(std::slice::from_ref(&before), &[unchanged]).is_ok());
        let changed = lower("quantile by(job)(Inf, data)");
        assert!(paired_node_identities(&[before], &[changed]).is_err());
    }

    // A producer may prove stronger uniqueness, but cannot drop a key that
    // its consumer relies on when interpreting or combining rows.
    #[test]
    fn input_contract_accepts_only_sufficient_unique_keys() {
        let root = lower_promql(
            "sum by(job)(data)",
            asap_types::ir::properties::AccuracyTarget::exact(),
        );
        let mut expected = (*root).clone();
        let mut supplied = (*root).clone();
        expected.schema.unique_keys = vec![vec![0, 1]];
        supplied.schema.unique_keys = vec![vec![0]];
        assert!(output_contract_satisfied(&expected, &supplied).unwrap());
        expected.schema.unique_keys = vec![vec![0]];
        supplied.schema.unique_keys = vec![vec![0, 1]];
        assert!(!output_contract_satisfied(&expected, &supplied).unwrap());
        supplied.schema.unique_keys.clear();
        assert!(!output_contract_satisfied(&expected, &supplied).unwrap());
        expected.schema.unique_keys.clear();
        supplied.schema.unique_keys = vec![vec![0]];
        assert!(output_contract_satisfied(&expected, &supplied).unwrap());
    }

    fn catalog(
        root: Rc<OperatorNode>,
    ) -> (DagOfPhysicalCandidatesWithCosts, Vec<Rc<OperatorNode>>) {
        fn add(
            node: &Rc<OperatorNode>,
            graph: &mut DagOfPhysicalCandidatesWithCosts,
            templates: &mut Vec<Rc<OperatorNode>>,
        ) -> PhysicalCandidateId {
            if let Some(id) = templates.iter().position(|t| Rc::ptr_eq(t, node)) {
                return PhysicalCandidateId(id);
            }
            let producer_candidates_per_input = ordered_inputs(node)
                .iter()
                .map(|child| InputProducerCandidates {
                    candidates: vec![add(child, graph, templates)].into(),
                })
                .collect();
            let id = templates.len();
            templates.push(Rc::clone(node));
            graph.candidates.push(PhysicalCandidateWithCost {
                local_cost: 1.0,
                producer_candidates_per_input,
            });
            PhysicalCandidateId(id)
        }
        let mut graph = DagOfPhysicalCandidatesWithCosts::default();
        let mut templates = Vec::new();
        let root = add(&root, &mut graph, &mut templates);
        graph.query_roots.push(QueryOutputCandidates {
            candidates: vec![root],
        });
        (graph, templates)
    }

    // Shared producers retain one identity; separately selected readers do not merge.
    #[test]
    fn two_readers_share_one_build_and_distinct_candidates_keep_identity() {
        let (mut graph, mut templates) = catalog(lower_promql(
            "sum(sum(metric))",
            asap_types::ir::properties::AccuracyTarget::exact(),
        ));
        let root = templates.len() - 1;
        assert_eq!(
            graph.candidates[root].producer_candidates_per_input.len(),
            1
        );
        // Same group and same template, deliberately distinct physical candidates.
        graph.candidates.push(graph.candidates[root].clone());
        templates.push(templates[root].clone());
        graph.query_roots.push(QueryOutputCandidates {
            candidates: vec![PhysicalCandidateId(root + 1)],
        });
        let selected = select_candidates(&graph).unwrap().unwrap();
        let outputs = reconstruct_candidate_outputs(&graph, &selected, &templates).unwrap();
        assert!(!Rc::ptr_eq(&outputs[0], &outputs[1]));
        assert!(Rc::ptr_eq(
            outputs[0].children()[0],
            outputs[1].children()[0]
        ));
        assert!(outputs.iter().all(|node| node.timing.is_none()));
    }

    // Input positions remain independent even when template references are identical.
    #[test]
    fn repeated_input_positions_can_select_distinct_producers() {
        let root = lower_promql(
            "metric + metric",
            asap_types::ir::properties::AccuracyTarget::exact(),
        );
        let (mut graph, mut templates) = catalog(root);
        let root = templates.len() - 1;
        assert_eq!(
            graph.candidates[root].producer_candidates_per_input.len(),
            2
        );
        let first = graph.candidates[root].producer_candidates_per_input[0].candidates[0];
        let duplicate = PhysicalCandidateId(templates.len());
        graph.candidates.push(graph.candidates[first.0].clone());
        templates.push(templates[first.0].clone());
        // Both template positions name the same expected child. Connections may differ.
        let template = &templates[root];
        let child = Rc::clone(template.children()[0]);
        templates[root] = Rc::new(OperatorNode::with_schema(
            template.operator.map_children(|_| Rc::clone(&child)),
            template.schema.clone(),
        ));
        graph.candidates[root].producer_candidates_per_input[0].candidates = vec![first].into();
        graph.candidates[root].producer_candidates_per_input[1].candidates = vec![duplicate].into();
        let selected = select_candidates(&graph).unwrap().unwrap();
        let outputs = reconstruct_candidate_outputs(&graph, &selected, &templates).unwrap();
        let children = outputs[0].children();
        assert!(!Rc::ptr_eq(children[0], children[1]));
    }

    // Invalid node metadata must fail before returning reconstructed roots.
    #[test]
    fn incompatible_input_schema_is_rejected() {
        let (graph, mut templates) = catalog(lower_promql(
            "sum(metric)",
            asap_types::ir::properties::AccuracyTarget::exact(),
        ));
        let selected = select_candidates(&graph).unwrap().unwrap();
        let root = templates.len() - 1;
        let template = &templates[root];
        let wrong = lower_promql(
            "sum(other)",
            asap_types::ir::properties::AccuracyTarget::exact(),
        );
        templates[root] = Rc::new(OperatorNode::with_schema(
            template.operator.map_children(|_| Rc::clone(&wrong)),
            template.schema.clone(),
        ));
        assert!(reconstruct_candidate_outputs(&graph, &selected, &templates)
            .unwrap_err()
            .contains("incompatible schema"));
    }
    // Scalar references precede direct inputs during rebinding, unlike children().
    // Verify identities and scalar contents, so equal schemas cannot hide swaps.
    #[test]
    fn scalar_reference_and_direct_input_preserve_their_distinct_positions() {
        use asap_types::ir::scalar::expr_ir::ScalarValue;
        use asap_types::ir::scalar::ProjectItem;
        use asap_types::ir::{NonASAPOp, Operator, ScalarExpr};

        let direct = lower_promql(
            "sum(direct_metric)",
            asap_types::ir::properties::AccuracyTarget::exact(),
        );
        let scalar = lower_promql(
            "sum(scalar_metric)",
            asap_types::ir::properties::AccuracyTarget::exact(),
        );
        assert_eq!(direct.schema, scalar.schema);
        let root = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
            cols: vec![
                ProjectItem {
                    alias: Some("scalar_result".into()),
                    expr: ScalarExpr::PromqlScalarFromVector(Rc::clone(&scalar)),
                },
                ProjectItem {
                    alias: Some("constant".into()),
                    expr: ScalarExpr::Literal(ScalarValue::Float64(7.0)),
                },
            ],
            qualifier: None,
            child: Rc::clone(&direct),
        }))
        .unwrap();
        assert!(Rc::ptr_eq(root.children()[0], &direct));
        assert!(Rc::ptr_eq(&ordered_inputs(&root)[0], &scalar));
        let (mut graph, templates) = catalog(Rc::clone(&root));
        let find = |node: &Rc<OperatorNode>| {
            PhysicalCandidateId(
                templates
                    .iter()
                    .position(|template| Rc::ptr_eq(template, node))
                    .unwrap(),
            )
        };
        graph.query_roots.push(QueryOutputCandidates {
            candidates: vec![find(&direct)],
        });
        graph.query_roots.push(QueryOutputCandidates {
            candidates: vec![find(&scalar)],
        });
        let selected = select_candidates(&graph).unwrap().unwrap();
        let outputs = reconstruct_candidate_outputs(&graph, &selected, &templates).unwrap();
        let Some(NonASAPOp::Project {
            cols,
            qualifier,
            child,
        }) = outputs[0].non_asap()
        else {
            panic!("expected reconstructed projection");
        };
        assert!(Rc::ptr_eq(child, &outputs[1]));
        let ScalarExpr::PromqlScalarFromVector(scalar) = &cols[0].expr else {
            panic!("scalar conversion must be preserved");
        };
        assert!(Rc::ptr_eq(scalar, &outputs[2]));
        assert!(!Rc::ptr_eq(child, scalar));
        assert_eq!(cols[0].alias.as_deref(), Some("scalar_result"));
        assert_eq!(cols[1].alias.as_deref(), Some("constant"));
        assert_eq!(cols[1].expr, ScalarExpr::Literal(ScalarValue::Float64(7.0)));
        assert!(qualifier.is_none());
    }
    // Completeness and uniqueness may strengthen; field and time semantics cannot drift.
    #[test]
    fn output_contract_allows_only_open_to_closed_refinement() {
        let node = lower_promql(
            "metric",
            asap_types::ir::properties::AccuracyTarget::exact(),
        );
        let mut expected = (*node).clone();
        expected.schema.closed = false;
        let mut supplied = expected.clone();
        supplied.schema.closed = true;
        assert!(output_contract_satisfied(&expected, &supplied).unwrap());
        assert!(!output_contract_satisfied(&supplied, &expected).unwrap());
        let mut renamed = supplied.clone();
        renamed.schema.fields[0].name.push_str("_changed");
        assert!(!output_contract_satisfied(&expected, &renamed).unwrap());
        let mut nullable = supplied.clone();
        nullable.schema.fields[0].nullable = !nullable.schema.fields[0].nullable;
        assert!(!output_contract_satisfied(&expected, &nullable).unwrap());
        let mut timeless = supplied.clone();
        timeless.schema.time_index = None;
        assert!(!output_contract_satisfied(&expected, &timeless).unwrap());
        let mut keyed = supplied.clone();
        keyed.schema.unique_keys.push(vec![0]);
        assert!(output_contract_satisfied(&expected, &keyed).unwrap());
        assert!(!output_contract_satisfied(&keyed, &expected).unwrap());
    }
}
