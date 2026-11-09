//! The execution-context extension of the selection MILP
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! section 5): the retention amount $h_v$ and constraint (R). The
//! executions $x_{\kappa,v}$ and edges $y_{\kappa,u,c,i}$ it selects over
//! are [`crate::selectable::ExecutionContexts`]'s, in [`super::model`].

use std::collections::{BTreeMap, HashMap};

use good_lp::{constraint, variable, Constraint, ProblemVariables, Variable};

use asap_types::stages::PhysicalCandidateId;

use super::model::{QueryId, SelectionInputs};
use crate::retention::{edge_retention_need_ms, output_retention_need_ms};
use crate::selectable::Execution;

/// $h_v$ in seconds (the unit keeps (R)'s coefficients near the others'),
/// for each candidate with a retention cost
/// ([`crate::cost::ExecutionCosts::retention_cost_per_second`]), added to
/// `problem`.
pub(crate) fn retention_variables(
    inputs: &SelectionInputs<'_>,
    problem: &mut ProblemVariables,
) -> BTreeMap<PhysicalCandidateId, Variable> {
    let mut retaining: Vec<PhysicalCandidateId> = inputs
        .costs
        .retention_cost_per_second
        .keys()
        .copied()
        .collect();
    retaining.sort_unstable();
    retaining
        .into_iter()
        .map(|v| (v, problem.add(variable().min(0))))
        .collect()
}

/// (R) $h_v \ge \operatorname{need}_{\kappa,v,c,i}\, y_{\kappa,v,c,i}$ and
/// $h_v \ge \operatorname{span}_q\, z_{q,v}$, for a candidate $v$ with $h_v$:
/// how far back its rows are read through each selectable edge `y` (the
/// reading path's offset plus range, [`edge_retention_need_ms`]) and by
/// each query reading it directly (its range-evaluation span), a constant
/// per edge, so (R) is linear. Query latency is ignored: a result counts as
/// read when its evaluation starts.
pub(crate) fn retention_constraints(
    inputs: &SelectionInputs<'_>,
    h: &BTreeMap<PhysicalCandidateId, Variable>,
    y: &BTreeMap<usize, Variable>,
    z: &BTreeMap<(QueryId, Execution), Variable>,
) -> Vec<Constraint> {
    let mut constraints = Vec::new();
    for (&position, &y) in y {
        let (_, edge, producer) = inputs.contexts.edges()[position];
        let Some(&h) = h.get(&producer.candidate) else {
            continue;
        };
        let need_ms = edge_retention_need_ms(inputs.physical, inputs.entries, edge)
            .expect("a candidate with h_v is read within a bound");
        constraints.push(constraint!(h >= seconds(need_ms) * y));
    }
    for (&(query, execution), &z) in z {
        if let Some(&h) = h.get(&execution.candidate) {
            let need_ms = output_retention_need_ms(&inputs.entries[query.0]);
            constraints.push(constraint!(h >= seconds(need_ms) * z));
        }
    }
    constraints
}

/// The least $h_v$ (R) allows for a selection, in ms, of each candidate
/// with $h_v$ that `edges` (selected, by position in
/// `ExecutionContexts::edges`) or `outputs` (the query outputs and the
/// executions answering them) read: what the MILP's optimum retains when
/// $\pi_v > 0$, and what the plan retains.
pub(crate) fn least_retention_ms(
    inputs: &SelectionInputs<'_>,
    edges: impl IntoIterator<Item = usize>,
    outputs: impl IntoIterator<Item = (QueryId, Execution)>,
) -> HashMap<PhysicalCandidateId, u64> {
    let retaining = &inputs.costs.retention_cost_per_second;
    let mut retention_ms: HashMap<PhysicalCandidateId, u64> = HashMap::new();
    let mut read = |v: PhysicalCandidateId, need_ms: u64| {
        if retaining.contains_key(&v) {
            let most = retention_ms.entry(v).or_default();
            *most = (*most).max(need_ms);
        }
    };
    for position in edges {
        let (_, edge, producer) = inputs.contexts.edges()[position];
        if retaining.contains_key(&producer.candidate) {
            let need_ms = edge_retention_need_ms(inputs.physical, inputs.entries, edge)
                .expect("a candidate with h_v is read within a bound");
            read(producer.candidate, need_ms);
        }
    }
    for (query, execution) in outputs {
        read(
            execution.candidate,
            output_retention_need_ms(&inputs.entries[query.0]),
        );
    }
    retention_ms
}

/// `ms` in seconds, the unit of $h_v$ in the MILP.
fn seconds(ms: u64) -> f64 {
    ms as f64 / 1_000.0
}
