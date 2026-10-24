//! Reconstruct selected immediate operators, preserving candidate identity.
//!
//! This decoder checks typed connections, not Stage 2 execution legality or
//! query accuracy targets. The caller must run Stage 2 and the plan validator.
use std::rc::Rc;

use asap_logical_asap_optimization::per_computation_candidates::recompose_guarantees;
use asap_types::ir::{OperatorNode, QueryRoot};

use super::candidate_connections::{
    validate_selected_graph, CandidateDependencyGraph, PhysicalCandidateId, SelectedCandidateGraph,
};

/// Templates indexed by globally unique candidate ID. Each template describes
/// one immediate operator; its ordered `Operator::map_children` positions correspond one-for-one to the
/// candidate's producer requirements. Children serve as typed input contracts,
/// never as implicit selected dependencies.
pub(super) type CandidateOperatorTemplates = Vec<Option<Rc<OperatorNode>>>;

/// Enumerate in exactly the traversal order used when rebinding. `children()`
/// puts direct children before scalar references; `map_children` follows fields.
fn ordered_inputs(node: &OperatorNode) -> Vec<Rc<OperatorNode>> {
    let mut inputs = Vec::new();
    node.operator.map_children(|child| {
        inputs.push(Rc::clone(child));
        Rc::clone(child)
    });
    inputs
}

/// Operator output references in `graph.query_roots` order. A caller with scalar
/// roots must map these references back into its original scalar expressions
/// (and preserve literal-only roots); an output reference is not a whole query.
/// Templates' timing/materialization are deliberately discarded: Stage 2 must
/// apply the selected materialization policy to the reconstructed connections.
pub(super) fn reconstruct_candidate_outputs(
    graph: &CandidateDependencyGraph,
    selected: &SelectedCandidateGraph,
    templates: &CandidateOperatorTemplates,
) -> Result<Vec<Rc<OperatorNode>>, String> {
    graph.validate().map_err(|e| e.to_string())?;
    validate_selected_graph(graph, selected)?;
    let count = graph.candidates.len();
    let mut inputs = graph
        .candidates
        .iter()
        .map(|candidate| vec![None; candidate.producer_requirements.len()])
        .collect::<Vec<_>>();
    let mut consumers = vec![Vec::new(); count];
    for connection in &selected.connections {
        inputs[connection.consumer.0][connection.requirement] = Some(connection.producer);
        consumers[connection.producer.0].push(connection.consumer.0);
    }
    let mut remaining = inputs.iter().map(Vec::len).collect::<Vec<_>>();
    let mut ready = Vec::new();
    for &PhysicalCandidateId(id) in &selected.selected_candidates {
        let template = templates
            .get(id)
            .and_then(Option::as_ref)
            .ok_or_else(|| format!("candidate {id} has no operator template"))?;
        if template.children().len() != inputs[id].len() {
            return Err(format!(
                "candidate {id} template inputs do not match producer requirements"
            ));
        }
        if remaining[id] == 0 {
            ready.push(id);
        }
    }
    let mut rebuilt: Vec<Option<Rc<OperatorNode>>> = vec![None; count];
    while let Some(id) = ready.pop() {
        let template = templates[id].as_ref().unwrap();
        let expected = ordered_inputs(template);
        let mut producers = Vec::with_capacity(inputs[id].len());
        for (position, producer) in inputs[id].iter().enumerate() {
            let producer = rebuilt[producer.unwrap().0].as_ref().unwrap();
            // Serialization includes sketch error/failure parameters whose
            // PartialEq intentionally omits some sizing metadata.
            if expected[position].result_kind != producer.result_kind
                || expected[position].schema != producer.schema
                || serde_json::to_value(&expected[position].schema).map_err(|e| e.to_string())?
                    != serde_json::to_value(&producer.schema).map_err(|e| e.to_string())?
            {
                return Err(format!("candidate {id} input {position} has incompatible schema or state configuration"));
            }
            producers.push(Rc::clone(producer));
        }
        let mut position = 0;
        // Re-derive structural metadata over the selected inputs, preserving
        // only explicit naming overrides. Logical rewrite templates can retain
        // pre-rewrite keys/nullability; copying their entire schema is invalid.
        let node = Rc::new(
            template
                .with_new_children(|_| {
                    let producer = Rc::clone(&producers[position]);
                    position += 1;
                    producer
                })
                .map_err(|e| format!("candidate {id}: {e}"))?
                .with_guarantee(template.guarantee.clone()),
        );
        node.validate_structure()
            .map_err(|e| format!("candidate {id}: {e}"))?;
        rebuilt[id] = Some(node);
        for &consumer in &consumers[id] {
            remaining[consumer] -= 1;
            if remaining[consumer] == 0 {
                ready.push(consumer);
            }
        }
    }
    let mut roots = vec![None; graph.query_roots.len()];
    for root in &selected.query_roots {
        roots[root.query_root] = Some(QueryRoot::Operator(Rc::clone(
            rebuilt[root.candidate.0].as_ref().unwrap(),
        )));
    }
    let roots = roots.into_iter().map(Option::unwrap).collect::<Vec<_>>();
    recompose_guarantees(&roots)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|root| match root {
            QueryRoot::Operator(node) => Ok(node),
            QueryRoot::Scalar(_) => Err("unexpected scalar during operator reconstruction".into()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::candidate_connections::{
        select_candidates, PhysicalCandidate, ProducerRequirement, QueryRootCandidates,
    };
    use super::*;
    use asap_logical_asap_optimization::memo::MemoGroupId;
    use asap_logical_asap_optimization::test_support::lower_promql;

    fn catalog(root: Rc<OperatorNode>) -> (CandidateDependencyGraph, CandidateOperatorTemplates) {
        fn add(
            node: &Rc<OperatorNode>,
            graph: &mut CandidateDependencyGraph,
            templates: &mut CandidateOperatorTemplates,
        ) -> PhysicalCandidateId {
            if let Some(id) = templates
                .iter()
                .position(|t| t.as_ref().is_some_and(|t| Rc::ptr_eq(t, node)))
            {
                return PhysicalCandidateId(id);
            }
            let producer_requirements = ordered_inputs(node)
                .iter()
                .map(|child| ProducerRequirement {
                    compatible_producers: vec![add(child, graph, templates)],
                })
                .collect();
            let id = templates.len();
            templates.push(Some(Rc::clone(node)));
            graph.candidates.push(PhysicalCandidate {
                group: MemoGroupId(id as u32),
                local_cost: 1.0,
                producer_requirements,
            });
            PhysicalCandidateId(id)
        }
        let mut graph = CandidateDependencyGraph::default();
        let mut templates = Vec::new();
        let root = add(&root, &mut graph, &mut templates);
        graph.query_roots.push(QueryRootCandidates {
            compatible_candidates: vec![root],
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
        assert_eq!(graph.candidates[root].producer_requirements.len(), 1);
        // Same group and same template, deliberately distinct physical candidates.
        graph.candidates.push(graph.candidates[root].clone());
        templates.push(templates[root].clone());
        graph.query_roots.push(QueryRootCandidates {
            compatible_candidates: vec![PhysicalCandidateId(root + 1)],
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
    fn repeated_input_positions_can_select_distinct_suppliers() {
        let root = lower_promql(
            "metric + metric",
            asap_types::ir::properties::AccuracyTarget::exact(),
        );
        let (mut graph, mut templates) = catalog(root);
        let root = templates.len() - 1;
        assert_eq!(graph.candidates[root].producer_requirements.len(), 2);
        let first = graph.candidates[root].producer_requirements[0].compatible_producers[0];
        let duplicate = PhysicalCandidateId(templates.len());
        graph.candidates.push(graph.candidates[first.0].clone());
        templates.push(templates[first.0].clone());
        // Both template positions name the same expected child. Connections may differ.
        let template = templates[root].as_ref().unwrap();
        let child = Rc::clone(template.children()[0]);
        templates[root] = Some(Rc::new(OperatorNode::with_schema(
            template.operator.map_children(|_| Rc::clone(&child)),
            template.schema.clone(),
        )));
        graph.candidates[root].producer_requirements[0].compatible_producers = vec![first];
        graph.candidates[root].producer_requirements[1].compatible_producers = vec![duplicate];
        let selected = select_candidates(&graph).unwrap().unwrap();
        let outputs = reconstruct_candidate_outputs(&graph, &selected, &templates).unwrap();
        let children = outputs[0].children();
        assert!(!Rc::ptr_eq(children[0], children[1]));
    }

    // Invalid template metadata must fail before returning reconstructed roots.
    #[test]
    fn missing_template_and_incompatible_input_schema_are_rejected() {
        let (graph, mut templates) = catalog(lower_promql(
            "sum(metric)",
            asap_types::ir::properties::AccuracyTarget::exact(),
        ));
        let selected = select_candidates(&graph).unwrap().unwrap();
        let saved = templates[0].take();
        assert!(reconstruct_candidate_outputs(&graph, &selected, &templates)
            .unwrap_err()
            .contains("no operator template"));
        templates[0] = saved;
        let root = templates.len() - 1;
        let template = templates[root].as_ref().unwrap();
        let wrong = lower_promql(
            "sum(other)",
            asap_types::ir::properties::AccuracyTarget::exact(),
        );
        templates[root] = Some(Rc::new(OperatorNode::with_schema(
            template.operator.map_children(|_| Rc::clone(&wrong)),
            template.schema.clone(),
        )));
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
                    .position(|template| {
                        template
                            .as_ref()
                            .is_some_and(|template| Rc::ptr_eq(template, node))
                    })
                    .unwrap(),
            )
        };
        graph.query_roots.push(QueryRootCandidates {
            compatible_candidates: vec![find(&direct)],
        });
        graph.query_roots.push(QueryRootCandidates {
            compatible_candidates: vec![find(&scalar)],
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
}
