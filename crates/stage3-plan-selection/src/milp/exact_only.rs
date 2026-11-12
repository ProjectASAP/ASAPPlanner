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

use super::model::{QueryId, SelectionInputs};
use crate::selectable::Execution;

/// Whether query `query` must not read `execution`: its candidate is not
/// exact (it has sketch parameters θ) and its group is one the query may
/// read only exactly. A readout's own candidate has no θ, but the sketch
/// states it reads are in the same list, as every group below a listed one
/// is.
///
/// Example: `quantile_over_time(0.9, m[5m])` at a relative target lists
/// the quantile and every group below it; its KLL state builds (θ) are
/// executions it must not read, the exact quantile one it may.
pub(crate) fn must_not_read(
    inputs: &SelectionInputs<'_>,
    query: QueryId,
    execution: Execution,
) -> bool {
    let logical = &inputs.physical.logical_candidates;
    let id = inputs.physical.physical_candidates[execution.candidate.0].logical_candidate_id;
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
/// it may reach from its output executions over selectable edges (`y`)
/// from which an execution it must not read ([`must_not_read`]) is
/// reachable, less those themselves. None for a query with no exact-only
/// group.
pub(crate) fn exact_only_variables(
    inputs: &SelectionInputs<'_>,
    y: &BTreeMap<usize, Variable>,
    problem: &mut ProblemVariables,
) -> BTreeMap<(QueryId, Execution), Variable> {
    let contexts = inputs.contexts;
    let count = contexts.executions().len();
    // The selectable edges, as producers per consumer and back.
    let mut producers = vec![Vec::new(); count];
    let mut consumers = vec![Vec::new(); count];
    for &position in y.keys() {
        let (consumer, _, producer) = contexts.edges()[position];
        let (c, u) = (contexts.index_of(consumer), contexts.index_of(producer));
        producers[c].push(u);
        consumers[u].push(c);
    }
    let execution = |e: usize| contexts.executions()[e];
    let queries: BTreeSet<QueryId> = inputs.query_outputs.iter().map(|(q, _)| *q).collect();
    let mut tracked = BTreeMap::new();
    for query in queries {
        let mut reached = vec![false; count];
        let mut pending: Vec<usize> = inputs
            .query_outputs
            .iter()
            .filter(|(q, _)| *q == query)
            .flat_map(|(_, executions)| executions.iter().map(|&e| contexts.index_of(e)))
            .collect();
        while let Some(e) = pending.pop() {
            if !std::mem::replace(&mut reached[e], true) {
                pending.extend(&producers[e]);
            }
        }
        let mut reaches_forbidden = vec![false; count];
        let mut pending: Vec<usize> = (0..count)
            .filter(|&e| reached[e] && must_not_read(inputs, query, execution(e)))
            .collect();
        while let Some(e) = pending.pop() {
            if reached[e] && !std::mem::replace(&mut reaches_forbidden[e], true) {
                pending.extend(&consumers[e]);
            }
        }
        for e in (0..count)
            .filter(|&e| reaches_forbidden[e] && !must_not_read(inputs, query, execution(e)))
        {
            tracked.insert((query, execution(e)), problem.add(variable().min(0).max(1)));
        }
    }
    tracked
}

/// (X) Query $q$ reads the groups it may read only exactly through exact
/// candidates: $a_{q,e} \ge z_{q,e}$ for an output execution,
/// $a_{q,p} \ge y_{\kappa,p,c,i} + a_{q,c} - 1$ along a selectable edge, so
/// $a_{q,e} = 1$ on every execution $q$'s selected path reads; an execution
/// $q$ must not read ([`must_not_read`]) has $a_{q,e} = 0$: $z_{q,e} = 0$
/// and $y_{\kappa,p,c,i} + a_{q,c} \le 1$. Other queries may still read it,
/// through their own paths. $a$ exists only where [`exact_only_variables`]
/// tracks it, and is not in the objective.
pub(crate) fn exact_only_constraints(
    inputs: &SelectionInputs<'_>,
    a: &BTreeMap<(QueryId, Execution), Variable>,
    y: &BTreeMap<usize, Variable>,
    z: &BTreeMap<(QueryId, Execution), Variable>,
) -> Vec<Constraint> {
    let mut constraints = Vec::new();
    for (&(query, execution), &z) in z {
        if must_not_read(inputs, query, execution) {
            constraints.push(constraint!(z <= 0));
        } else if let Some(&a) = a.get(&(query, execution)) {
            constraints.push(constraint!(a >= z));
        }
    }
    // The tracked queries, per execution.
    let mut readers: BTreeMap<Execution, Vec<(QueryId, Variable)>> = BTreeMap::new();
    for (&(query, execution), &a) in a {
        readers.entry(execution).or_default().push((query, a));
    }
    for (&position, &y) in y {
        let (consumer, _, producer) = inputs.contexts.edges()[position];
        for &(query, a_consumer) in readers.get(&consumer).into_iter().flatten() {
            if must_not_read(inputs, query, producer) {
                constraints.push(constraint!(y + a_consumer <= 1));
            } else if let Some(&a_producer) = a.get(&(query, producer)) {
                constraints.push(constraint!(a_producer >= y + a_consumer - 1));
            }
        }
    }
    constraints
}

/// Per execution, by position in `ExecutionContexts::executions`, whether
/// some query must not read it ([`must_not_read`]): the greedy bound
/// ([`super::upper_bound`]) leaves them out, so the plan it finds meets (X)
/// whichever query reads what.
pub(crate) fn not_read_by_some_query(inputs: &SelectionInputs<'_>) -> Vec<bool> {
    let queries: BTreeSet<QueryId> = inputs.query_outputs.iter().map(|(q, _)| *q).collect();
    inputs
        .contexts
        .executions()
        .iter()
        .map(|&execution| {
            queries
                .iter()
                .any(|&query| must_not_read(inputs, query, execution))
        })
        .collect()
}
