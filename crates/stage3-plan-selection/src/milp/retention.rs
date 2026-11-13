//! The retention extension of the selection MILP
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! section 5): the retention amount $h_v$ and constraint (R). The
//! executions $x_{\kappa,v}$ and edges $y_{\kappa,u,c,i}$ it selects over
//! are [`crate::selectable::SelectableExecutions`]'s, in [`super::model`].

use std::collections::{BTreeMap, HashMap};

use good_lp::{constraint, variable, Constraint, ProblemVariables, Variable};

use asap_types::stages::PhysicalCandidateId;

use super::model::SelectionInputs;
use crate::retention::output_retention_need_ms;
use crate::selectable::QueryId;

/// $h_v$ in seconds retained (the unit keeps (R)'s coefficients near the
/// others'), added to `problem`, for each candidate with a retention cost
/// ([`crate::cost::ExecutionCosts::retention_cost_per_retained_second`])
/// one of whose executions is `kept` (by position in
/// `SelectableExecutions::executions`): one whose executions are all left
/// out retains nothing.
pub(crate) fn retention_variables(
    inputs: &SelectionInputs<'_>,
    kept: &[bool],
    problem: &mut ProblemVariables,
) -> BTreeMap<PhysicalCandidateId, Variable> {
    let retaining = &inputs.costs.retention_cost_per_retained_second;
    let mut retained: Vec<PhysicalCandidateId> = inputs
        .selectable
        .executions()
        .iter()
        .zip(kept)
        .filter(|(execution, &kept)| kept && retaining.contains_key(&execution.candidate))
        .map(|(execution, _)| execution.candidate)
        .collect();
    retained.sort_unstable();
    retained.dedup();
    retained
        .into_iter()
        .map(|v| (v, problem.add(variable().min(0))))
        .collect()
}

/// (R) $h_v \ge \operatorname{need}_{\kappa,v,c,i}\, y_{\kappa,v,c,i}$ and
/// $h_v \ge \operatorname{span}_q\, z_{q,v}$, for a candidate $v$ with $h_v$:
/// how far back its rows are read through each selectable edge `y` (the
/// reading path's offset plus range,
/// [`crate::cost::ExecutionCosts::retention_need_ms_per_edge`]) and by each
/// query reading it directly (its range-evaluation span), a constant per
/// edge, so (R) is linear. Query latency is ignored: a result counts as
/// read when its evaluation starts.
pub(crate) fn retention_constraints(
    inputs: &SelectionInputs<'_>,
    h: &BTreeMap<PhysicalCandidateId, Variable>,
    y: &BTreeMap<usize, Variable>,
    z: &BTreeMap<(QueryId, usize), Variable>,
) -> Vec<Constraint> {
    let executions = inputs.selectable.executions();
    let mut constraints = Vec::new();
    for (&position, &y) in y {
        let (_, _, producer) = inputs.selectable.edges()[position];
        let Some(&h) = h.get(&executions[producer].candidate) else {
            continue;
        };
        let need_ms = inputs.costs.retention_need_ms_per_edge[position]
            .expect("a candidate with h_v is read within a bound");
        constraints.push(constraint!(h >= seconds(need_ms) * y));
    }
    for (&(query, execution), &z) in z {
        if let Some(&h) = h.get(&executions[execution].candidate) {
            let need_ms = output_retention_need_ms(&inputs.entries[query.0]);
            constraints.push(constraint!(h >= seconds(need_ms) * z));
        }
    }
    constraints
}

/// The least $h_v$ (R) allows for a selection, in ms, of each candidate
/// with a retention cost that `edges` (selected, by position in
/// `SelectableExecutions::edges`) or `outputs` (the query outputs and the
/// executions answering them, by position) read: what the MILP's optimum
/// retains when $\pi_v > 0$, and what the plan retains.
pub(crate) fn least_retention_ms(
    inputs: &SelectionInputs<'_>,
    edges: impl IntoIterator<Item = usize>,
    outputs: impl IntoIterator<Item = (QueryId, usize)>,
) -> HashMap<PhysicalCandidateId, u64> {
    let executions = inputs.selectable.executions();
    let retaining = &inputs.costs.retention_cost_per_retained_second;
    let reads = edges
        .into_iter()
        .filter_map(|position| {
            let (_, _, producer) = inputs.selectable.edges()[position];
            Some((
                executions[producer].candidate,
                inputs.costs.retention_need_ms_per_edge[position]?,
            ))
        })
        .chain(outputs.into_iter().map(|(query, execution)| {
            (
                executions[execution].candidate,
                output_retention_need_ms(&inputs.entries[query.0]),
            )
        }));
    let mut retention_ms: HashMap<PhysicalCandidateId, u64> = HashMap::new();
    for (v, need_ms) in reads.filter(|(v, _)| retaining.contains_key(v)) {
        let most = retention_ms.entry(v).or_default();
        *most = (*most).max(need_ms);
    }
    retention_ms
}

/// `ms` in seconds, the unit of $h_v$ in the MILP.
fn seconds(ms: u64) -> f64 {
    ms as f64 / 1_000.0
}
