//! The retention extension of the selection MILP
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! section 5): the retention amount $h_v$ and constraint (R). The
//! executions $x_{\kappa,v}$ and edges $y_{\kappa,u,c,i}$ it selects over
//! are [`crate::selectable::SelectableExecutions`]'s, in [`super::model`].

use std::collections::{BTreeMap, HashMap, HashSet};

use good_lp::{constraint, variable, Constraint, ProblemVariables, Variable};

use asap_types::stages::PhysicalCandidateId;

use super::model::SelectionInputs;
use crate::retention::ms_to_seconds;
use crate::selectable::QueryId;
use crate::selectable::Reader;

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
///
/// With them, per execution $u$ of such a $v$, $h_v \ge
/// \min_{\text{uses}} \operatorname{need} \cdot x_u$ over its costed
/// reads (`ExecutionCosts::least_need_ms_per_execution`): (F) gives a
/// selected execution a use, and (R) then retains at least the least
/// need. It is implied for integer $x$, $y$,
/// $z$, but tightens the relaxation where fractional $y$ leave $h_v$ low.
pub(crate) fn retention_constraints(
    inputs: &SelectionInputs<'_>,
    h: &BTreeMap<PhysicalCandidateId, Variable>,
    x: &[Variable],
    y: &BTreeMap<usize, Variable>,
    z: &BTreeMap<(QueryId, usize), Variable>,
) -> Vec<Constraint> {
    let executions = inputs.selectable.executions();
    let mut constraints = Vec::new();
    for (index, reader, need_ms) in inputs
        .selectable
        .reads_back(&inputs.costs.retention_need_ms_per_edge, inputs.entries)
    {
        let Some(&h) = h.get(&executions[index].candidate) else {
            continue;
        };
        let read = match reader {
            Reader::Edge(position) => y.get(&position),
            Reader::Output(query) => z.get(&(query, index)),
        };
        if let Some(&read) = read {
            let need_ms = need_ms.expect("a candidate with h_v is read within a bound");
            constraints.push(constraint!(h >= ms_to_seconds(need_ms) * read));
        }
    }
    for (index, need_ms) in inputs.costs.least_need_ms_per_execution.iter().enumerate() {
        if let (Some(&h), Some(need_ms)) = (h.get(&executions[index].candidate), *need_ms) {
            if need_ms > 0 {
                constraints.push(constraint!(h >= ms_to_seconds(need_ms) * x[index]));
            }
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
    let edges: HashSet<usize> = edges.into_iter().collect();
    let outputs: HashSet<(QueryId, usize)> = outputs.into_iter().collect();
    let mut retention_ms: HashMap<PhysicalCandidateId, u64> = HashMap::new();
    for (index, reader, need_ms) in inputs
        .selectable
        .reads_back(&inputs.costs.retention_need_ms_per_edge, inputs.entries)
    {
        let selected = match reader {
            Reader::Edge(position) => edges.contains(&position),
            Reader::Output(query) => outputs.contains(&(query, index)),
        };
        let v = executions[index].candidate;
        if let (true, Some(need_ms), true) = (selected, need_ms, retaining.contains_key(&v)) {
            let most = retention_ms.entry(v).or_default();
            *most = (*most).max(need_ms);
        }
    }
    retention_ms
}
