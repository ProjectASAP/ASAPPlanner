//! Realize a selected DAG through Stage 2: one operator node per selected
//! node, over its selected producers, each exported with its
//! `PhysicalCandidate`'s timing and lifetime, its θ and its memo group's
//! summary coverage (concepts doc, section 3, "Stage 3: baseline selection
//! and realization"). This validates execution legality, not cost
//! estimates or accuracy targets.

use std::collections::HashMap;
use std::rc::Rc;

use asap_types::ir::materialized_view::ViewCoverage;
use asap_types::ir::physical_export::{physical_asap_dag_with_assignments, ExportedNodeAssignment};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, QueryRoot, TimeRangeKind};
use asap_types::physical::{ExecutionTiming, Lifetime, PhysicalProperties, PhysicalWorkloadPlan};
use asap_types::stages::{
    AssignmentRealizer, DagOfSelectedCandidates, MemoGroupId, PhysicalASAPMemoCandidates,
    PhysicalCandidateError, RealizedPlan, WindowLayout,
};
use asap_types::workload::bounds::confirmed_weights;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

/// Stage 2's [`AssignmentRealizer`], over the planning context: the
/// workload's entries (one per query, for the range evaluations a
/// tumbling-window chain's retention covers) and its data (which proves
/// summary update weights non-negative). The facade builds it, passes it
/// to Stage 3, and builds the selected plan with it.
pub struct Stage2AssignmentRealizer<'a> {
    pub entries: &'a [QueryWorkloadEntry],
    pub data: &'a DataWorkload,
}

impl AssignmentRealizer for Stage2AssignmentRealizer<'_> {
    /// Each selected node, producers first, becomes the operator node of
    /// its candidate's expression over the nodes of its selected producers,
    /// in input order. A candidate with a tumbling-window layout of width
    /// $w$ over a range `TimeRange(W)` becomes a `SummaryTumblingWindowChain`
    /// of $W / w$ windows over the range's input, read through a
    /// `SummaryMerge` of the whole chain: the chain has the candidate's
    /// timing, lifetime and θ; the merge runs at query time unless every
    /// consumer of the node runs at ingestion time. A summary build's update
    /// weights the data proves non-negative are confirmed
    /// (`bounds::confirmed_weights`). The query roots are the workload's
    /// roots over groups with each output reference replaced by its node.
    ///
    /// Err [`PhysicalCandidateError::RejectedAssignment`] when `selected` is
    /// not a valid selection over `physical` or the plan fails the physical
    /// DAG validator; [`PhysicalCandidateError::InvalidOrUnsupported`] when a
    /// candidate's operator does not rebuild over its producers.
    fn realize(
        &self,
        physical: &PhysicalASAPMemoCandidates,
        selected: &DagOfSelectedCandidates,
    ) -> Result<RealizedPlan, PhysicalCandidateError> {
        selected
            .validate(physical)
            .map_err(PhysicalCandidateError::RejectedAssignment)?;
        let memo = &physical.logical_candidates.memo;
        let node_count = selected.candidate_of_node.len();
        // Per node, per input: the node producing it; and per node, its consumers.
        let mut producers: Vec<Vec<usize>> = selected
            .candidate_of_node
            .iter()
            .map(|&id| {
                let logical =
                    memo.candidate(physical.physical_candidates[id.0].logical_candidate_id);
                vec![usize::MAX; logical.expression.operator.children().len()]
            })
            .collect();
        let mut consumers: Vec<Vec<usize>> = vec![Vec::new(); node_count];
        for edge in &selected.edges {
            producers[edge.consumer.0][edge.input_index_of_consumer] = edge.producer.0;
            consumers[edge.producer.0].push(edge.consumer.0);
        }
        let mut built: Vec<Option<Rc<OperatorNode>>> = vec![None; node_count];
        let mut assignments: HashMap<*const OperatorNode, ExportedNodeAssignment> = HashMap::new();
        for node in producers_first(&producers) {
            let candidate = &physical.physical_candidates[selected.candidate_of_node[node].0];
            let logical = memo.candidate(candidate.logical_candidate_id);
            let group = &memo.groups()[candidate.logical_candidate_id.group.0 as usize];
            let children: Vec<Rc<OperatorNode>> = producers[node]
                .iter()
                .map(|&producer| Rc::clone(built[producer].as_ref().expect("producers first")))
                .collect();
            let operator = with_confirmed_weights(
                over_children(&logical.expression.operator, &children)?,
                self.data,
            );
            let summary_coverage = group
                .group_properties
                .summary_coverage
                .as_ref()
                .map(ViewCoverage::of);
            let assignment = ExportedNodeAssignment {
                physical_properties: candidate.physical_properties,
                parameters: logical.logical_properties.parameters().cloned(),
                summary_coverage: summary_coverage.clone(),
            };
            let operator_node = match logical.logical_properties.window_layout() {
                None => new_node(operator)?,
                Some(&WindowLayout::Tumbling { width_ms }) => {
                    let chain = new_node(tumbling_window_chain(operator, width_ms)?)?;
                    assignments.insert(Rc::as_ptr(&chain), assignment);
                    let merge_at_ingestion = candidate.physical_properties.timing()
                        == ExecutionTiming::IngestionTime
                        && !consumers[node].is_empty()
                        && consumers[node].iter().all(|&consumer| {
                            physical.physical_candidates[selected.candidate_of_node[consumer].0]
                                .physical_properties
                                .timing()
                                == ExecutionTiming::IngestionTime
                        });
                    let merge = new_node(Operator::ASAP(ASAPOp::SummaryMerge {
                        children: vec![chain],
                        newest_tumbling_windows: None,
                        group_by: None,
                    }))?;
                    assignments.insert(
                        Rc::as_ptr(&merge),
                        ExportedNodeAssignment {
                            physical_properties: PhysicalProperties::new(
                                match merge_at_ingestion {
                                    true => ExecutionTiming::IngestionTime,
                                    false => ExecutionTiming::QueryTime,
                                },
                                Lifetime::None,
                            )
                            .expect("a result that is not materialized is valid at either time"),
                            parameters: None,
                            summary_coverage,
                        },
                    );
                    built[node] = Some(merge);
                    continue;
                }
            };
            assignments.insert(Rc::as_ptr(&operator_node), assignment);
            built[node] = Some(operator_node);
        }
        let operator_node_of_selected_node: Vec<Rc<OperatorNode>> = built
            .into_iter()
            .map(|node| node.expect("every node is built"))
            .collect();
        let mut outputs = selected
            .node_of_output_reference
            .iter()
            .map(|node| Rc::clone(&operator_node_of_selected_node[node.0]));
        let mut next_output = || {
            outputs
                .next()
                .expect("validated: one node per output reference")
        };
        let roots: Vec<QueryRoot> = physical
            .query_roots
            .iter()
            .map(|root| match root {
                QueryRoot::Operator(_) => QueryRoot::Operator(next_output()),
                QueryRoot::Scalar(expression) => QueryRoot::Scalar(
                    expression.map_operator_refs(&mut |_: &MemoGroupId| next_output()),
                ),
            })
            .collect();
        let (dag, _) = physical_asap_dag_with_assignments(&roots, &|node| {
            assignments
                .get(&Rc::as_ptr(node))
                .cloned()
                .expect("every node of the plan is a selected node, a chain or its merge")
        })
        .map_err(|error| PhysicalCandidateError::RejectedAssignment(error.to_string()))?;
        let range_evaluations: Vec<_> = self
            .entries
            .iter()
            .map(|entry| entry.range_evaluation)
            .collect();
        let tumbling_window_retention_ms =
            asap_cost::tumbling_window_retention_ms_of_plan(&dag, &range_evaluations);
        Ok(RealizedPlan {
            plan: PhysicalWorkloadPlan {
                id: "selected".into(),
                from_logical: "memo".into(),
                roots,
                dag,
            },
            tumbling_window_retention_ms,
            operator_node_of_selected_node,
        })
    }
}

/// The nodes of a selected DAG, each after its producers (`producers[n]`:
/// node `n`'s producer per input); the DAG is acyclic (validated).
fn producers_first(producers: &[Vec<usize>]) -> Vec<usize> {
    let mut outstanding: Vec<usize> = producers.iter().map(Vec::len).collect();
    let mut consumers_of = vec![Vec::new(); producers.len()];
    for (consumer, inputs) in producers.iter().enumerate() {
        for &producer in inputs {
            consumers_of[producer].push(consumer);
        }
    }
    let mut ready: Vec<usize> = (0..producers.len())
        .filter(|&node| outstanding[node] == 0)
        .collect();
    let mut order = Vec::with_capacity(producers.len());
    while let Some(node) = ready.pop() {
        order.push(node);
        for &consumer in &consumers_of[node] {
            outstanding[consumer] -= 1;
            if outstanding[consumer] == 0 {
                ready.push(consumer);
            }
        }
    }
    order
}

/// `operator` over `children`, its `i`-th child (in [`Operator::children`]
/// order) replaced by `children[i]`.
fn over_children(
    operator: &Operator<MemoGroupId>,
    children: &[Rc<OperatorNode>],
) -> Result<Operator<Rc<OperatorNode>>, PhysicalCandidateError> {
    let in_order: Vec<MemoGroupId> = operator.children().into_iter().copied().collect();
    let mut visited = Vec::with_capacity(in_order.len());
    let _ = operator.map_children(|group| visited.push(*group));
    if visited != in_order || in_order.len() != children.len() {
        return Err(PhysicalCandidateError::InvalidOrUnsupported(format!(
            "the operator's inputs do not match its selected producers: {operator:?}"
        )));
    }
    let mut next = children.iter();
    Ok(operator.map_children(|_| Rc::clone(next.next().expect("one child per input"))))
}

/// `operator` with a summary build's update weights confirmed by `data`
/// (`bounds::confirmed_weights`): an open non-negativity requirement the
/// data proves becomes a proof the executor runs.
fn with_confirmed_weights(
    mut operator: Operator<Rc<OperatorNode>>,
    data: &DataWorkload,
) -> Operator<Rc<OperatorNode>> {
    if let Operator::ASAP(
        ASAPOp::SummaryAgg { input, .. } | ASAPOp::SummaryTumblingWindowChain { input, .. },
    ) = &mut operator
    {
        input.weight_domain = confirmed_weights(&input.weight_domain, data);
    }
    operator
}

/// The tumbling-window chain of a build `SummaryAgg` over a range
/// `TimeRange(W)`: windows `width_ms` wide, `W / width_ms` of them, over the
/// range's input.
fn tumbling_window_chain(
    build: Operator<Rc<OperatorNode>>,
    width_ms: u64,
) -> Result<Operator<Rc<OperatorNode>>, PhysicalCandidateError> {
    let Operator::ASAP(ASAPOp::SummaryAgg {
        child,
        family,
        input,
        reduction,
        grouping,
        filter,
    }) = build
    else {
        return Err("a tumbling-window layout on an operator other than a summary build".into());
    };
    let Operator::NonASAP(NonASAPOp::TimeRange {
        range,
        kind: TimeRangeKind::Range,
        child: below,
    }) = &child.operator
    else {
        return Err("a tumbling-window layout on a build that does not read a range".into());
    };
    let window_ms = u64::try_from(range.as_millis()).unwrap_or(u64::MAX);
    if width_ms == 0 || window_ms % width_ms != 0 {
        return Err(format!(
            "tumbling windows {width_ms} ms wide do not divide the range of {window_ms} ms"
        )
        .into());
    }
    let tumbling_window_count = u32::try_from(window_ms / width_ms)
        .map_err(|_| PhysicalCandidateError::from("too many tumbling windows"))?;
    Ok(Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child: Rc::clone(below),
        family,
        input,
        reduction,
        grouping,
        filter,
        tumbling_window_width_ms: width_ms,
        tumbling_window_count,
    }))
}

/// The operator node of `operator`, its schema derived from its children.
fn new_node(
    operator: Operator<Rc<OperatorNode>>,
) -> Result<Rc<OperatorNode>, PhysicalCandidateError> {
    OperatorNode::new_shared(operator)
        .map_err(|error| PhysicalCandidateError::InvalidOrUnsupported(error.to_string()))
}
