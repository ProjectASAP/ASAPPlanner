//! The memo (Stage 1): memo groups, each query's root over them, and each
//! group's candidates `v = (e_v, p^log_v)` with a requirement `r^log_{v,i}`
//! per input.

use std::collections::{BinaryHeap, HashMap, HashSet};
use std::rc::Rc;

use super::hash_consing::same_value;
use super::{MemoGroup, MemoGroupId};
use crate::ir::schema::sketch::{ExactKind, SketchAlgorithm, SketchKind};
use crate::ir::{Operator, OperatorNode, QueryRoot, SchemaDerivationError};

/// The memo: the memo groups, each query's root over them, and each group's
/// candidates.
#[derive(Debug, Clone, Default)]
pub struct Memo {
    /// The groups, by id.
    pub groups: Vec<MemoGroup>,
    /// Each query's root over groups, by index in the workload.
    pub query_roots: Vec<QueryRoot<MemoGroupId>>,
    /// Each group's candidates, by group index.
    pub candidates: Vec<Vec<CandidateForMemoGroup>>,
}

/// One candidate: index `index` in group `group`'s candidates
/// ([`Memo::candidates`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CandidateForMemoGroupId {
    pub group: MemoGroupId,
    pub index: usize,
}

/// `v = (e_v, p^log_v)`: one candidate of a memo group.
#[derive(Debug, Clone)]
pub struct CandidateForMemoGroup {
    /// `e_v`.
    pub expression: CandidateExpression,
    /// `p^log_v`.
    pub logical_properties: LogicalProperties,
    /// `r^log_{v,i}`, one per child group of `expression.operator`, in
    /// [`Operator::map_children`] order.
    pub input_requirements: Vec<LogicalRequirement>,
    /// The groups a fused computation also implements, beyond the group
    /// whose candidate it is; empty otherwise.
    pub fused_groups: Vec<MemoGroupId>,
    /// Which rule added it. Not in the formulation: kept for
    /// `stage_pipeline --text` and the viewer.
    pub origin: OriginOfCandidateForMemoGroup,
    /// The number of equal shares of its queries' accuracy budget its
    /// summary is sized for: 1 for the whole budget, `n` for one of `n`
    /// approximate inputs of a query. Only a candidate whose representation
    /// is a sketch has more than 1. Deleted by #1207 step 6 (the θ list and
    /// row (K)).
    pub accuracy_budget_parts: usize,
}

/// `e_v = op_v(G_{v,1}, …, G_{v,n})`: one operator, with its arguments,
/// whose children are groups.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateExpression {
    /// `op_v`; its children are `G_{v,i}`.
    pub operator: Operator<MemoGroupId>,
}

/// `p^log_v`: one field per dimension.
#[derive(Debug, Clone, PartialEq)]
pub struct LogicalProperties {
    /// The family and `θ_v`.
    representation: Representation,
    /// `ω_v`.
    window_layout: Option<WindowLayout>,
}

/// Why a [`LogicalProperties`] is invalid.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum InvalidLogicalProperties {
    /// Only summary or exact-accumulator state is laid out in windows.
    #[error("a plain value has no window layout")]
    WindowLayoutOfPlainValue,
}

impl LogicalProperties {
    /// The properties `(representation, window_layout)`; a plain value has
    /// no window layout.
    pub fn new(
        representation: Representation,
        window_layout: Option<WindowLayout>,
    ) -> Result<Self, InvalidLogicalProperties> {
        if matches!(representation, Representation::Plain) && window_layout.is_some() {
            return Err(InvalidLogicalProperties::WindowLayoutOfPlainValue);
        }
        Ok(Self {
            representation,
            window_layout,
        })
    }

    /// The family and `θ_v`.
    pub fn representation(&self) -> &Representation {
        &self.representation
    }

    /// `ω_v`.
    pub fn window_layout(&self) -> Option<&WindowLayout> {
        self.window_layout.as_ref()
    }
}

/// What a candidate's output holds: exact-accumulator state of a kind,
/// summary state of a family with its parameters `θ`, or plain values.
#[derive(Debug, Clone, PartialEq)]
pub enum Representation {
    Exact(ExactKind),
    Summary(SketchKind),
    /// Values: no state, so no `θ` (a readout, a projection, a scan).
    Plain,
}

/// `ω_v`: how the state is laid out in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowLayout {
    /// `count` back-to-back tumbling windows of `width_ms`.
    Tumbling { width_ms: u64, count: u32 },
}

/// `r^log_{c,i}`: one optional constraint per dimension; `None` is
/// unconstrained.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LogicalRequirement {
    /// The summary family the input must hold.
    pub family: Option<SketchAlgorithm>,
    /// The tumbling windows the input must be laid out in.
    pub window: Option<WindowRequirement>,
}

/// The tumbling windows a consumer reads: their width, and how many there
/// must be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowRequirement {
    pub width_ms: u64,
    pub min_count: u32,
    pub max_count: u32,
}

/// `α^log_{u,c,i}`: whether a producer with properties `producer`
/// satisfies `requirement`, one check per dimension.
pub fn satisfies(producer: &LogicalProperties, requirement: &LogicalRequirement) -> bool {
    let family = requirement.family.as_ref().is_none_or(|family| {
        matches!(&producer.representation, Representation::Summary(kind) if kind.algorithm() == family)
    });
    let window = requirement.window.is_none_or(|required| {
        matches!(
            producer.window_layout,
            Some(WindowLayout::Tumbling { width_ms, count })
                if width_ms == required.width_ms
                    && (required.min_count..=required.max_count).contains(&count)
        )
    });
    family && window
}

/// Where a candidate came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginOfCandidateForMemoGroup {
    /// Pass 1: exact, exact accumulator, or a summary family and size, and
    /// the intermediate results of each.
    Pass1,
    /// Tumbling windows (rule 1).
    TumblingWindows,
    /// A coarse group regrouping a finer group (rules 5–7).
    Rollup,
    /// A top-k read from another group's ranking (rule 4).
    TopKReuse,
    /// Its metric's scans widened to a shared label set, read through a
    /// `Project` (rule 2).
    WidenedScans,
}

impl Memo {
    /// Hash-consing: the group with this expression, added if new. Two
    /// expressions are one group when they are equal without the
    /// candidates' properties: sketch parameters (and the failure
    /// probability they record) and tumbling-window width and count are
    /// left out. An expression whose operator is not legal to share (a
    /// non-ASAP operator that may return other rows for the same input
    /// rows) is always a new group. A new group's logical node is the
    /// operator over its child groups' logical nodes.
    ///
    /// Panics when the new group's schema does not derive: a rule adds only
    /// expressions it built from nodes.
    pub fn find_or_add_group(&mut self, expression: Operator<MemoGroupId>) -> MemoGroupId {
        let children: Vec<MemoGroupId> = expression.children().into_iter().copied().collect();
        let operator =
            expression.map_children(|g| Rc::clone(&self.groups[g.0 as usize].logical_node));
        let shareable = match &operator {
            Operator::ASAP(_) => true,
            Operator::NonASAP(op) => op.is_shareable_when_equal(),
        };
        if shareable {
            let key = group_key(&expression);
            let found = (0..self.groups.len()).find(|&g| {
                self.groups[g].child_groups == children
                    && group_key(&self.group_expression(MemoGroupId(g as u32))) == key
            });
            if let Some(g) = found {
                return MemoGroupId(g as u32);
            }
        }
        let id = MemoGroupId(
            u32::try_from(self.groups.len()).expect("a memo has at most u32::MAX groups"),
        );
        let logical_node = Rc::new(
            OperatorNode::new(operator).expect("a rule adds only expressions whose schema derives"),
        );
        for &child in &children {
            let parents = &mut self.groups[child.0 as usize].parents;
            if !parents.contains(&id) {
                parents.push(id);
            }
        }
        self.groups.push(MemoGroup {
            logical_node,
            child_groups: children,
            parents: Vec::new(),
            queries: Vec::new(),
        });
        self.candidates.push(Vec::new());
        id
    }

    /// Adds `candidate` to `group` and returns its id, or the id of the
    /// candidate there already with the same expression, properties,
    /// requirements, fused groups and accuracy budget parts: a group has a
    /// candidate once. The groups the candidate reads, and every group
    /// below them, are reached by `group`'s queries, and `group` is their
    /// parent.
    pub fn add_candidate(
        &mut self,
        group: MemoGroupId,
        candidate: CandidateForMemoGroup,
    ) -> CandidateForMemoGroupId {
        let candidates = &self.candidates[group.0 as usize];
        if let Some(index) = candidates.iter().position(|known| same_candidate(known, &candidate)) {
            return CandidateForMemoGroupId { group, index };
        }
        let inputs: Vec<MemoGroupId> = candidate
            .expression
            .operator
            .children()
            .into_iter()
            .copied()
            .collect();
        self.candidates[group.0 as usize].push(candidate);
        let index = self.candidates[group.0 as usize].len() - 1;
        for &input in &inputs {
            let parents = &mut self.groups[input.0 as usize].parents;
            if !parents.contains(&group) {
                parents.push(group);
            }
        }
        let queries = self.groups[group.0 as usize].queries.clone();
        for input in inputs {
            self.add_queries(input, &queries);
        }
        CandidateForMemoGroupId { group, index }
    }

    /// The candidate `id` names.
    pub fn candidate(&self, id: CandidateForMemoGroupId) -> &CandidateForMemoGroup {
        &self.candidates[id.group.0 as usize][id.index]
    }

    /// Candidate `id`'s operator node: its operator over its input groups'
    /// logical nodes. The group's logical node itself when the candidate's
    /// expression is the group's own (with its schema and guarantee);
    /// otherwise the schema is derived, with the group's field names and
    /// qualifiers when it has as many fields, and no guarantee.
    pub fn operator_node_of(
        &self,
        id: CandidateForMemoGroupId,
    ) -> Result<Rc<OperatorNode>, SchemaDerivationError> {
        let group = &self.groups[id.group.0 as usize];
        let operator = &self.candidate(id).expression.operator;
        let by_index = |operator: &Operator<MemoGroupId>| operator.map_children(|g| g.0);
        if same_value(&by_index(operator), &by_index(&self.group_expression(id.group))) {
            return Ok(Rc::clone(&group.logical_node));
        }
        let mut node = OperatorNode::new(
            operator.map_children(|g| Rc::clone(&self.groups[g.0 as usize].logical_node)),
        )?;
        let own = &group.logical_node.schema.fields;
        if node.schema.fields.len() == own.len() {
            for (field, own) in node.schema.fields.iter_mut().zip(own) {
                field.name = own.name.clone();
                field.table = own.table.clone();
            }
        }
        Ok(Rc::new(node))
    }

    /// Group `group`'s expression: its logical node's operator over its
    /// child groups.
    fn group_expression(&self, group: MemoGroupId) -> Operator<MemoGroupId> {
        let memo_group = &self.groups[group.0 as usize];
        let node = &memo_group.logical_node;
        let group_of: HashMap<*const OperatorNode, MemoGroupId> = node
            .children()
            .into_iter()
            .zip(&memo_group.child_groups)
            .map(|(child, &g)| (Rc::as_ptr(child), g))
            .collect();
        node.operator.map_children(|child| group_of[&Rc::as_ptr(child)])
    }

    /// Adds `queries` to `group` and to every group below it, through
    /// logical child groups and candidates' input groups.
    fn add_queries(&mut self, group: MemoGroupId, queries: &[usize]) {
        let mut pending = vec![group];
        let mut seen = HashSet::new();
        while let Some(next) = pending.pop() {
            if !seen.insert(next) {
                continue;
            }
            let reached = &mut self.groups[next.0 as usize].queries;
            if next != group && queries.iter().all(|query| reached.contains(query)) {
                continue;
            }
            reached.extend_from_slice(queries);
            reached.sort_unstable();
            reached.dedup();
            pending.extend(self.groups_read_by(next));
        }
    }

    /// The groups `group` reads: its logical child groups and its
    /// candidates' input groups.
    fn groups_read_by(&self, group: MemoGroupId) -> Vec<MemoGroupId> {
        self.groups[group.0 as usize]
            .child_groups
            .iter()
            .copied()
            .chain(
                self.candidates
                    .get(group.0 as usize)
                    .into_iter()
                    .flatten()
                    .flat_map(|candidate| candidate.expression.operator.children())
                    .copied(),
            )
            .collect()
    }

    /// The groups, each after every group it or one of its candidates
    /// reads, the lower index first among those ready: a dependency order
    /// for candidate traversal. `None` when those reads form a cycle, a
    /// group reading itself included.
    pub fn topological_order(&self) -> Option<Vec<MemoGroupId>> {
        let count = self.groups.len();
        let reads: Vec<HashSet<usize>> = (0..count)
            .map(|g| {
                self.groups_read_by(MemoGroupId(g as u32))
                    .into_iter()
                    .map(|child| child.0 as usize)
                    .collect()
            })
            .collect();
        let mut consumers: Vec<Vec<usize>> = vec![Vec::new(); count];
        let mut waiting: Vec<usize> = reads.iter().map(HashSet::len).collect();
        for (g, read) in reads.iter().enumerate() {
            for &child in read {
                consumers[child].push(g);
            }
        }
        let mut ready: BinaryHeap<std::cmp::Reverse<usize>> = (0..count)
            .filter(|&g| waiting[g] == 0)
            .map(std::cmp::Reverse)
            .collect();
        let mut order = Vec::with_capacity(count);
        while let Some(std::cmp::Reverse(g)) = ready.pop() {
            order.push(MemoGroupId(g as u32));
            for &consumer in &consumers[g] {
                waiting[consumer] -= 1;
                if waiting[consumer] == 0 {
                    ready.push(std::cmp::Reverse(consumer));
                }
            }
        }
        (order.len() == count).then_some(order)
    }
}

/// Whether two candidates are one: equal expressions (with their
/// arguments, bit for bit), properties, requirements, fused groups and
/// accuracy budget parts.
fn same_candidate(left: &CandidateForMemoGroup, right: &CandidateForMemoGroup) -> bool {
    let by_index = |operator: &Operator<MemoGroupId>| operator.map_children(|g| g.0);
    left.fused_groups == right.fused_groups
        && left.accuracy_budget_parts == right.accuracy_budget_parts
        && left.input_requirements == right.input_requirements
        && left.logical_properties.window_layout == right.logical_properties.window_layout
        && match (
            &left.logical_properties.representation,
            &right.logical_properties.representation,
        ) {
            (Representation::Summary(a), Representation::Summary(b)) => same_value(a, b),
            (a, b) => a == b,
        }
        && same_value(
            &by_index(&left.expression.operator),
            &by_index(&right.expression.operator),
        )
}

/// The group key of `expression`: its canonical JSON without the
/// candidates' properties (sketch parameters, tumbling-window width and
/// count).
fn group_key(expression: &Operator<MemoGroupId>) -> String {
    let mut value = serde_json::to_value(expression.map_children(|g| g.0))
        .expect("an operator over group ids serializes");
    without_properties(&mut value);
    value.to_string()
}

/// `value` with every sketch's parameters and every tumbling-window width
/// and count removed.
fn without_properties(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            if map.contains_key("algorithm") && map.contains_key("params") && map.len() == 2 {
                map.remove("params");
            }
            map.remove("tumbling_window_width_ms");
            map.remove("tumbling_window_count");
            map.values_mut().for_each(without_properties);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(without_properties),
        _ => {}
    }
}
