//! Adding an operator tree that Pass 1's composition or a rule's IR rewrite
//! builds to the memo, one operator per candidate: every node beneath the
//! tree's root is found or added as a group ([`Memo::find_or_add_group`])
//! and gets its operator as a candidate ([`Memo::add_candidate`]); the
//! root's candidate is returned for the caller to add to the group it
//! computes.

use std::collections::HashMap;
use std::rc::Rc;

use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator, OperatorNode};
use asap_types::stages::{
    CandidateExpression, CandidateForMemoGroup, LogicalProperties, LogicalRequirement, Memo,
    MemoGroupId, OriginOfCandidateForMemoGroup, Representation, WindowLayout, WindowRequirement,
};

/// `root`'s candidate: its operator over the groups of its children, each
/// found or added with every node beneath it and given its operator as a
/// candidate made by `origin`. A node that is a group's logical node (the
/// same `Rc`) is that group, and gets no candidate. A candidate whose output
/// is a sketch is sized for `accuracy_budget_parts`, every other for 1.
pub(crate) fn add_operator_tree(
    memo: &mut Memo,
    root: &Rc<OperatorNode>,
    origin: &OriginOfCandidateForMemoGroup,
    accuracy_budget_parts: usize,
) -> CandidateForMemoGroup {
    let mut group_of: HashMap<*const OperatorNode, MemoGroupId> = memo
        .groups
        .iter()
        .enumerate()
        .map(|(g, group)| (Rc::as_ptr(&group.logical_node), MemoGroupId(g as u32)))
        .collect();
    let expression = root
        .operator
        .map_children(|child| group_of_node(memo, child, origin, accuracy_budget_parts, &mut group_of));
    candidate_of(root, expression, origin, accuracy_budget_parts)
}

/// The group of `node`: the group whose logical node it is, else the group
/// of its operator over its children's groups, found or added, with that
/// operator as a candidate.
fn group_of_node(
    memo: &mut Memo,
    node: &Rc<OperatorNode>,
    origin: &OriginOfCandidateForMemoGroup,
    accuracy_budget_parts: usize,
    group_of: &mut HashMap<*const OperatorNode, MemoGroupId>,
) -> MemoGroupId {
    if let Some(&group) = group_of.get(&Rc::as_ptr(node)) {
        return group;
    }
    let expression = node
        .operator
        .map_children(|child| group_of_node(memo, child, origin, accuracy_budget_parts, group_of));
    let known = memo.groups.len();
    let group = memo.find_or_add_group(expression.clone());
    if group.0 as usize >= known {
        // A new group's logical node is this node over its children's
        // groups' nodes: its field names and guarantee as composed.
        let logical_node = Rc::new(OperatorNode {
            operator: node
                .operator
                .map_children(|child| Rc::clone(&memo.groups[group_of[&Rc::as_ptr(child)].0 as usize].logical_node)),
            ..(**node).clone()
        });
        group_of.insert(Rc::as_ptr(&logical_node), group);
        memo.groups[group.0 as usize].logical_node = logical_node;
    }
    memo.add_candidate(
        group,
        candidate_of(node, expression, origin, accuracy_budget_parts),
    );
    group_of.insert(Rc::as_ptr(node), group);
    group
}

/// The candidate of `node`, whose operator over its children's groups is
/// `expression`: its representation and window layout read off `node`, and
/// each input's requirement off the child it reads.
pub(crate) fn candidate_of(
    node: &OperatorNode,
    expression: Operator<MemoGroupId>,
    origin: &OriginOfCandidateForMemoGroup,
    accuracy_budget_parts: usize,
) -> CandidateForMemoGroup {
    let representation = representation_of(node);
    let accuracy_budget_parts = match representation {
        Representation::Summary(_) => accuracy_budget_parts,
        Representation::Exact(_) | Representation::Plain => 1,
    };
    let window_layout = window_layout_of(node);
    let mut input_requirements = Vec::new();
    node.operator.map_children(|child| {
        input_requirements.push(LogicalRequirement {
            family: match representation_of(child) {
                Representation::Summary(kind) => Some(kind.algorithm().clone()),
                Representation::Exact(_) | Representation::Plain => None,
            },
            window: window_requirement(node, child),
        })
    });
    CandidateForMemoGroup {
        expression: CandidateExpression {
            operator: expression,
        },
        logical_properties: LogicalProperties::new(representation, window_layout)
            .expect("only state is laid out in windows"),
        input_requirements,
        fused_groups: Vec::new(),
        origin: origin.clone(),
        accuracy_budget_parts,
    }
}

/// What `node` outputs: its first state field's family (a sketch with its
/// parameters, or an exact accumulator), else plain values.
fn representation_of(node: &OperatorNode) -> Representation {
    node.schema
        .fields
        .iter()
        .find_map(|field| match &field.dtype {
            FieldDataType::Sketch(kind, _) => Some(Representation::Summary(kind.clone())),
            FieldDataType::ExactAggregate(kind, _) => Some(Representation::Exact(*kind)),
            _ => None,
        })
        .unwrap_or(Representation::Plain)
}

/// `node`'s tumbling windows: a chain's own, or those a merge reads of the
/// chain below it.
fn window_layout_of(node: &OperatorNode) -> Option<WindowLayout> {
    match node.asap()? {
        ASAPOp::SummaryTumblingWindowChain {
            tumbling_window_width_ms,
            tumbling_window_count,
            ..
        } => Some(WindowLayout::Tumbling {
            width_ms: *tumbling_window_width_ms,
            count: *tumbling_window_count,
        }),
        ASAPOp::SummaryMerge {
            children,
            newest_tumbling_windows,
            ..
        } => match window_layout_of(children.first()?)? {
            WindowLayout::Tumbling { width_ms, count } => Some(WindowLayout::Tumbling {
                width_ms,
                count: newest_tumbling_windows.unwrap_or(count),
            }),
        },
        _ => None,
    }
}

/// The tumbling windows `consumer` reads of `input`: a merge of a chain's
/// windows needs the chain's width, and exactly its count when it merges
/// every window, or more than the newest it merges.
fn window_requirement(consumer: &OperatorNode, input: &OperatorNode) -> Option<WindowRequirement> {
    let Some(ASAPOp::SummaryMerge {
        newest_tumbling_windows,
        ..
    }) = consumer.asap()
    else {
        return None;
    };
    let Some(ASAPOp::SummaryTumblingWindowChain {
        tumbling_window_width_ms,
        tumbling_window_count,
        ..
    }) = input.asap()
    else {
        return None;
    };
    Some(match newest_tumbling_windows {
        None => WindowRequirement {
            width_ms: *tumbling_window_width_ms,
            min_count: *tumbling_window_count,
            max_count: *tumbling_window_count,
        },
        Some(newest) => WindowRequirement {
            width_ms: *tumbling_window_width_ms,
            min_count: newest + 1,
            max_count: u32::MAX,
        },
    })
}
