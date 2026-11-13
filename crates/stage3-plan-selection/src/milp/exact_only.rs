//! Exact-only reads per query
//! (`docs/design_docs/concepts/accuracy-models.md`, "the groups it may read
//! only exactly"): a query reads the groups Pass 2 lists as exact-only for
//! it (`LogicalASAPMemoCandidates::exact_only_groups_of_query`) through
//! exact candidates only, in its own selection context, while another query
//! may still read a sketch of those groups through its own path.
//!
//! $a_{q,e} \in [0, 1]$ marks the executions $e$ query $q$'s selected path
//! reads, from its output down; constraint (X) keeps it at 0 on every
//! execution $q$ must not read.

use std::collections::{BTreeMap, BTreeSet};

use good_lp::{constraint, variable, Constraint, ProblemVariables, Variable};

use asap_types::stages::{PhysicalASAPMemoCandidates, PhysicalCandidateId};

use super::model::SelectionInputs;
use crate::selectable::QueryId;

/// Whether query `query` must not read candidate `v`: it is not exact (it
/// has sketch parameters θ) and its group is one the query may read only
/// exactly. A readout's own candidate has no θ, but the sketch states it
/// reads are in the same list, as every group below a listed one is.
///
/// Example: `quantile_over_time(0.9, m[5m])` at a relative target lists
/// the quantile and every group below it; its KLL state builds (θ) are
/// candidates it must not read, the exact quantile one it may.
pub(crate) fn must_not_read(
    physical: &PhysicalASAPMemoCandidates,
    query: QueryId,
    v: PhysicalCandidateId,
) -> bool {
    let logical = &physical.logical_candidates;
    let id = physical.physical_candidates[v.0].logical_candidate_id;
    logical
        .exact_only_groups_of_query
        .get(query.0)
        .is_some_and(|groups| groups.contains(&id.group))
        && logical
            .memo
            .candidate(id)
            .logical_properties
            .parameters()
            .is_some()
}

/// $a_{q,e}$, added to `problem`, for each query $q$ and each execution $e$
/// (by position in `SelectableExecutions::executions`) it may reach from
/// its output executions over selectable edges (`y`) from which an
/// execution it must not read ([`must_not_read`]) is reachable, less those
/// themselves. None for a query with no exact-only group.
pub(crate) fn exact_only_variables(
    inputs: &SelectionInputs<'_>,
    y: &BTreeMap<usize, Variable>,
    problem: &mut ProblemVariables,
) -> BTreeMap<(QueryId, usize), Variable> {
    let selectable = inputs.selectable;
    let count = selectable.executions().len();
    // The selectable edges, as producers per consumer and back.
    let mut producers = vec![Vec::new(); count];
    let mut consumers = vec![Vec::new(); count];
    for &position in y.keys() {
        let (c, _, u) = selectable.edges()[position];
        producers[c].push(u);
        consumers[u].push(c);
    }
    let forbidden = |query: QueryId, e: usize| {
        must_not_read(inputs.physical, query, selectable.executions()[e].candidate)
    };
    let queries: BTreeSet<QueryId> = selectable.answering.iter().map(|(q, _)| *q).collect();
    let mut tracked = BTreeMap::new();
    for query in queries {
        let mut reached = vec![false; count];
        let mut pending: Vec<usize> = selectable
            .answering
            .iter()
            .filter(|(q, _)| *q == query)
            .flat_map(|(_, executions)| executions.iter().copied())
            .collect();
        while let Some(e) = pending.pop() {
            if !std::mem::replace(&mut reached[e], true) {
                pending.extend(&producers[e]);
            }
        }
        let mut reaches_forbidden = vec![false; count];
        let mut pending: Vec<usize> = (0..count)
            .filter(|&e| reached[e] && forbidden(query, e))
            .collect();
        while let Some(e) = pending.pop() {
            if reached[e] && !std::mem::replace(&mut reaches_forbidden[e], true) {
                pending.extend(&consumers[e]);
            }
        }
        for e in (0..count).filter(|&e| reaches_forbidden[e] && !forbidden(query, e)) {
            tracked.insert((query, e), problem.add(variable().min(0).max(1)));
        }
    }
    tracked
}

/// (X) Query $q$ reads the groups it may read only exactly through exact
/// candidates: $a_{q,e} \ge z_{q,e}$ for an output execution,
/// $a_{q,p} \ge y_{\kappa,p,c,i} + a_{q,c} - 1$ along a selectable edge, so
/// $a_{q,e} = 1$ on every execution $q$'s selected path reads; along an
/// edge into an execution $p$ $q$ must not read ([`must_not_read`]),
/// $y_{\kappa,p,c,i} + a_{q,c} \le 1$. No $z_{q,e}$ exists for an output
/// execution $q$ must not read (`crate::select_plan` leaves it out). Other
/// queries may still read it, through their own paths. $a$ exists only
/// where [`exact_only_variables`] tracks it, and is not in the objective.
pub(crate) fn exact_only_constraints(
    inputs: &SelectionInputs<'_>,
    a: &BTreeMap<(QueryId, usize), Variable>,
    y: &BTreeMap<usize, Variable>,
    z: &BTreeMap<(QueryId, usize), Variable>,
) -> Vec<Constraint> {
    let selectable = inputs.selectable;
    let mut constraints: Vec<Constraint> = z
        .iter()
        .filter_map(|(key, &z)| Some(constraint!(*a.get(key)? >= z)))
        .collect();
    // The tracked queries, per execution.
    let mut readers: BTreeMap<usize, Vec<(QueryId, Variable)>> = BTreeMap::new();
    for (&(query, execution), &a) in a {
        readers.entry(execution).or_default().push((query, a));
    }
    for (&position, &y) in y {
        let (consumer, _, producer) = selectable.edges()[position];
        for &(query, a_consumer) in readers.get(&consumer).into_iter().flatten() {
            let v = selectable.executions()[producer].candidate;
            if must_not_read(inputs.physical, query, v) {
                constraints.push(constraint!(y + a_consumer <= 1));
            } else if let Some(&a_producer) = a.get(&(query, producer)) {
                constraints.push(constraint!(a_producer >= y + a_consumer - 1));
            }
        }
    }
    constraints
}
