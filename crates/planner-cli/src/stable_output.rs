//! What keeps `asap_planner`'s JSON document the same across changes that
//! do not change the plan's meaning, so it can be compared as text:
//! canonical memo ids ([`CanonicalIds`]) and numbers rounded to 12
//! significant digits ([`rounded_to_12_significant_digits`]).
//!
//! The planner numbers memo groups and their candidates in insertion
//! order, so a new rule or a reordered loop renumbers every later group,
//! and a query added to a workload renumbers the groups Pass 1 adds for
//! the queries before it.
//! The document renumbers them instead: depth first from the query roots,
//! in query order. The text output keeps the planner's own ids, which
//! logs and error messages use.

use asap_types::stages::{
    CandidateForMemoGroup, CandidateForMemoGroupId, Memo, MemoGroupId, PhysicalASAPMemoCandidates,
    PhysicalCandidateId,
};
use serde_json::{json, Value};

/// The canonical ids of one run's memo groups, logical candidates and
/// physical candidates, from the run's last memo (memo ids only append, so
/// a group or candidate keeps its id in every step).
///
/// - Groups: depth first from each query's root groups, in query order;
///   below a group, its expression's inputs, then each candidate's inputs
///   and fused groups, candidates in canonical order. A group no query
///   reaches comes after, in planner order.
/// - Candidates of a group: ordered by origin, then operator (without its
///   inputs), neither of which depends on ids; candidates equal in both
///   keep planner order, e.g. a sketch's θ list in the order its rule
///   enumerates it (`Kll(k=26)`, `Kll(k=52)`, ...).
/// - Physical candidates: by their logical candidate's canonical id, then
///   in Stage 2's order (one per timing and lifetime it enumerates).
///
/// Example: `quantile_over_time(0.9, m[5m])` gives its `Aggregate
/// quantile` root group id 0, the `TimeRange 300s` below it 1 and the
/// `Scan` 2 (the planner numbers them the other way round, `Scan` first),
/// then the groups its candidates read: the `SummaryEstimate` readout 3
/// and its `SummaryAgg KLL` 4.
pub struct CanonicalIds {
    /// Indexed by the planner's group id.
    canonical_group_of_planner_group: Vec<u32>,
    /// Per planner group, indexed by the planner's candidate index.
    canonical_index_of_planner_index: Vec<Vec<usize>>,
    /// Indexed by the planner's physical candidate id; empty before Stage 2.
    canonical_physical_of_planner_physical: Vec<usize>,
}

impl CanonicalIds {
    /// The canonical ids of `memo` (the run's last memo) and, after Stage
    /// 2, of its `physical` candidates.
    pub fn of(memo: &Memo, physical: Option<&PhysicalASAPMemoCandidates>) -> Self {
        let groups = memo.groups();
        let candidate_order: Vec<Vec<usize>> = groups
            .iter()
            .map(|group| {
                let keys: Vec<String> = group.candidates.iter().map(candidate_order_key).collect();
                let mut order: Vec<usize> = (0..keys.len()).collect();
                // Stable: equal keys keep planner order.
                order.sort_by(|a, b| keys[*a].cmp(&keys[*b]));
                order
            })
            .collect();
        let mut group_order: Vec<MemoGroupId> = Vec::with_capacity(groups.len());
        let mut seen = vec![false; groups.len()];
        let query_roots = memo
            .queries()
            .iter()
            .flat_map(|query| query.root.operator_refs().into_iter().copied());
        let all_groups = (0..groups.len() as u32).map(MemoGroupId);
        for start in query_roots.chain(all_groups) {
            let mut pending = vec![start];
            while let Some(group) = pending.pop() {
                if std::mem::replace(&mut seen[group.0 as usize], true) {
                    continue;
                }
                group_order.push(group);
                let memo_group = &groups[group.0 as usize];
                let mut below: Vec<MemoGroupId> = memo_group
                    .expression
                    .children()
                    .into_iter()
                    .copied()
                    .collect();
                for &index in &candidate_order[group.0 as usize] {
                    let candidate = &memo_group.candidates[index];
                    below.extend(
                        candidate
                            .expression
                            .operator
                            .children()
                            .into_iter()
                            .copied(),
                    );
                    below.extend(candidate.fused_groups.iter().copied());
                }
                // Reversed, so the first input is visited first.
                pending.extend(below.into_iter().rev());
            }
        }
        let mut canonical_group_of_planner_group = vec![0; groups.len()];
        for (canonical, group) in group_order.iter().enumerate() {
            canonical_group_of_planner_group[group.0 as usize] = canonical as u32;
        }
        let canonical_index_of_planner_index = candidate_order
            .iter()
            .map(|order| {
                let mut canonical_of = vec![0; order.len()];
                for (canonical, &index) in order.iter().enumerate() {
                    canonical_of[index] = canonical;
                }
                canonical_of
            })
            .collect();
        let mut ids = Self {
            canonical_group_of_planner_group,
            canonical_index_of_planner_index,
            canonical_physical_of_planner_physical: Vec::new(),
        };
        if let Some(physical) = physical {
            let mut order: Vec<usize> = (0..physical.physical_candidates.len()).collect();
            order.sort_by_key(|&id| {
                ids.candidate(physical.physical_candidates[id].logical_candidate_id)
            });
            let mut canonical_of = vec![0; order.len()];
            for (canonical, &id) in order.iter().enumerate() {
                canonical_of[id] = canonical;
            }
            ids.canonical_physical_of_planner_physical = canonical_of;
        }
        ids
    }

    /// `group`'s canonical id.
    pub fn group(&self, group: MemoGroupId) -> u32 {
        self.canonical_group_of_planner_group[group.0 as usize]
    }

    /// The canonical (group, index in group) of a logical candidate.
    pub fn candidate(&self, id: CandidateForMemoGroupId) -> (u32, usize) {
        (
            self.group(id.group),
            self.canonical_index_of_planner_index[id.group.0 as usize][id.index_in_group],
        )
    }

    /// A physical candidate's canonical id.
    pub fn physical(&self, id: PhysicalCandidateId) -> usize {
        self.canonical_physical_of_planner_physical[id.0]
    }

    /// A logical candidate as the document prints it:
    /// `{"group": .., "index_in_group": ..}`.
    pub fn candidate_json(&self, id: CandidateForMemoGroupId) -> Value {
        let (group, index_in_group) = self.candidate(id);
        json!({ "group": group, "index_in_group": index_in_group })
    }

    /// `value`, a serialized planner value (a `RootPruneReason` or a
    /// `PruneReason`), with each logical candidate id (an object of exactly
    /// `group` and `index_in_group`) and each `dominated_by` physical
    /// candidate id made canonical.
    pub fn in_serialized(&self, value: Value) -> Value {
        match value {
            Value::Object(mut map) => {
                let is_candidate_id = map.len() == 2
                    && map.get("group").is_some_and(Value::is_u64)
                    && map.get("index_in_group").is_some_and(Value::is_u64);
                if is_candidate_id {
                    let id = CandidateForMemoGroupId {
                        group: MemoGroupId(map["group"].as_u64().unwrap_or_default() as u32),
                        index_in_group: map["index_in_group"].as_u64().unwrap_or_default() as usize,
                    };
                    return self.candidate_json(id);
                }
                if let Some(id) = map.get("dominated_by").and_then(Value::as_u64) {
                    map.insert(
                        "dominated_by".into(),
                        json!(self.physical(PhysicalCandidateId(id as usize))),
                    );
                    return Value::Object(map);
                }
                Value::Object(
                    map.into_iter()
                        .map(|(key, value)| (key, self.in_serialized(value)))
                        .collect(),
                )
            }
            Value::Array(items) => {
                Value::Array(items.into_iter().map(|v| self.in_serialized(v)).collect())
            }
            other => other,
        }
    }
}

/// What orders a group's candidates, none of it an id: the origin and the
/// operator without its inputs.
fn candidate_order_key(candidate: &CandidateForMemoGroup) -> String {
    json!([
        candidate.origin,
        candidate.expression.operator.map_children(|_| ())
    ])
    .to_string()
}

/// `value` with every non-integer number rounded to 12 significant digits,
/// e.g. `0.30000000000000004` to `0.3`: the last digits of a cost move
/// when the planner sums the same terms in another order.
pub fn rounded_to_12_significant_digits(value: Value) -> Value {
    match value {
        Value::Number(n) if n.is_f64() => {
            let x = n.as_f64().unwrap_or_default();
            json!(format!("{x:.11e}").parse::<f64>().unwrap_or(x))
        }
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, rounded_to_12_significant_digits(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(rounded_to_12_significant_digits)
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A float keeps 12 significant digits; integers and other values are
    /// untouched.
    #[test]
    fn floats_keep_12_significant_digits() {
        assert_eq!(
            rounded_to_12_significant_digits(json!({
                "cost": 0.1 + 0.2,
                "small": 1.234567890123456e-9,
                "count": 3,
                "list": [2.0000000000001, "x"],
            })),
            json!({
                "cost": 0.3,
                "small": 1.23456789012e-9,
                "count": 3,
                "list": [2.0, "x"],
            })
        );
    }
}
