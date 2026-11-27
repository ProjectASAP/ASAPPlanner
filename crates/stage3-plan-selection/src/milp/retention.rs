//! The retention extension of the selection MILP
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! section 5): the retention amount $h_v$ and constraint (R). The
//! executions $x_{\kappa,v}$ and edges $y_{\kappa,u,c,i}$ it selects over
//! are [`crate::selectable::SelectableExecutions`]'s, in [`super::model`].

use std::collections::{BTreeMap, HashMap, HashSet};

use good_lp::{constraint, variable, Constraint, Expression, ProblemVariables, Variable};

use asap_types::stages::PhysicalCandidateId;

use super::model::SelectionInputs;
use crate::cost::ExecutionCosts;
use crate::retention::ms_to_seconds;
use crate::selectable::QueryId;

/// $h_v$, the seconds candidate $v$ retains, as the share $h_v / H_v \in
/// [0, 1]$ of $H_v$, the longest any read of $v$ needs ((R)'s largest
/// coefficient): $h_v$ is `longest_need_s` times `share`, and (R) reads
/// that product, so its rows keep their tolerance in seconds (a row over
/// the share alone would let HiGHS's absolute tolerance of 1e-6 stand for
/// 1e-6 of $H_v$).
///
/// In seconds, $h_v$ would be unbounded with whole-number row coefficients
/// (60 to 86,400). HiGHS's presolve takes such a column for an integer, and
/// its root reduced-cost fixing then steps through up to 1,024 of its
/// values at every root LP solve, in time quadratic in the steps: 149 such
/// columns took about 80 % of a 12 s solve of a dag-viewer workload.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RetentionShare {
    /// $h_v / H_v$, in $[0, 1]$.
    pub(crate) share: Variable,
    /// $H_v$ in seconds; 1 when no read needs any retention.
    pub(crate) longest_need_s: f64,
}

impl RetentionShare {
    /// $h_v$ in seconds, `longest_need_s` times `share`: what (R) and the
    /// answer horizon read.
    pub(crate) fn seconds(&self) -> Expression {
        self.longest_need_s * self.share
    }
}

/// $h_v$ ([`RetentionShare`]), added to `problem`, for each candidate
/// with a retention cost ([`ExecutionCosts::retention_cost_per_retained_second`])
/// one of whose executions is `kept` (by position in
/// `SelectableExecutions::executions`): one whose executions are all left
/// out retains nothing.
pub(crate) fn retention_variables(
    inputs: &SelectionInputs<'_>,
    kept: &[bool],
    problem: &mut ProblemVariables,
) -> BTreeMap<PhysicalCandidateId, RetentionShare> {
    let retaining = &inputs.costs.retention_cost_per_retained_second;
    let executions = inputs.selectable.executions();
    let mut retained: Vec<PhysicalCandidateId> = executions
        .iter()
        .zip(kept)
        .filter(|(execution, &kept)| kept && retaining.contains_key(&execution.candidate))
        .map(|(execution, _)| execution.candidate)
        .collect();
    retained.sort_unstable();
    retained.dedup();
    // Per candidate, the longest need of (R)'s rows, in ms.
    let mut longest_need_ms: HashMap<PhysicalCandidateId, u64> = HashMap::new();
    let reads_back = inputs
        .selectable
        .reads_back(&inputs.costs.retention_need_ms_per_edge)
        .filter_map(|(index, _, need_ms)| Some((index, need_ms?)));
    let least = (inputs
        .costs
        .least_retention_ms_per_execution
        .iter()
        .enumerate())
    .filter_map(|(index, need_ms)| Some((index, (*need_ms)?)));
    for (index, need_ms) in reads_back.chain(least) {
        let v = executions[index].candidate;
        if retaining.contains_key(&v) {
            let longest = longest_need_ms.entry(v).or_default();
            *longest = (*longest).max(need_ms);
        }
    }
    retained
        .into_iter()
        .map(|v| {
            let longest_need_s = match longest_need_ms.get(&v) {
                Some(&ms) if ms > 0 => ms_to_seconds(ms),
                _ => 1.0,
            };
            let share = problem.add(variable().min(0).max(1));
            (
                v,
                RetentionShare {
                    share,
                    longest_need_s,
                },
            )
        })
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
/// reads (`ExecutionCosts::least_retention_ms_per_execution`): (F) gives a
/// selected execution a use, and (R) then retains at least the least
/// need. It is implied for integer $x$, $y$,
/// $z$, but tightens the relaxation where fractional $y$ leave $h_v$ low.
pub(crate) fn retention_constraints(
    inputs: &SelectionInputs<'_>,
    h: &BTreeMap<PhysicalCandidateId, RetentionShare>,
    x: &[Variable],
    y: &BTreeMap<usize, Variable>,
    z: &BTreeMap<(QueryId, usize), Variable>,
) -> Vec<Constraint> {
    let executions = inputs.selectable.executions();
    let mut constraints = Vec::new();
    for (index, reader, need_ms) in inputs
        .selectable
        .reads_back(&inputs.costs.retention_need_ms_per_edge)
    {
        let Some(&h) = h.get(&executions[index].candidate) else {
            continue;
        };
        if let Some(read) = reader.variable(index, y, z) {
            let need_ms = need_ms.expect("a candidate with h_v is read within a bound");
            constraints.push(constraint!(h.seconds() >= ms_to_seconds(need_ms) * read));
        }
    }
    for (index, need_ms) in inputs
        .costs
        .least_retention_ms_per_execution
        .iter()
        .enumerate()
    {
        if let (Some(&h), Some(need_ms)) = (h.get(&executions[index].candidate), *need_ms) {
            if need_ms > 0 {
                constraints.push(constraint!(
                    h.seconds() >= ms_to_seconds(need_ms) * x[index]
                ));
            }
        }
    }
    constraints
}

/// The least $h_v$ (R) allows for a selection, in ms, of each candidate
/// with a retention cost that `edges` (selected, by position in
/// `SelectableExecutions::edges`) or `outputs` (the query outputs and the
/// executions answering them, by position) read, each read's need at
/// least its newest output (`ExecutionCosts::retention_ms_for_need`): what
/// the MILP's optimum retains when $\pi_v > 0$, and what the plan retains.
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
        .reads_back(&inputs.costs.retention_need_ms_per_edge)
    {
        let selected = reader.is_selected(index, &edges, &outputs);
        let v = executions[index].candidate;
        if let (true, Some(need_ms), true) = (selected, need_ms, retaining.contains_key(&v)) {
            let need_ms = inputs.costs.retention_ms_for_need(v, need_ms);
            let most = retention_ms.entry(v).or_default();
            *most = (*most).max(need_ms);
        }
    }
    retention_ms
}

/// $r_v$, the windows a view under the data plane's window cache keeps
/// resident in memory (`docs/design_docs/proposals/cost-terms-from-calibration.md`,
/// section 1), one per view with a read kept, as the share $s_v = r_v /
/// R_v \in [0, 1]$ of $R_v$, the most windows any of its kept reads keeps:
/// (W) $R_v s_v \ge \operatorname{res}_{e}\, \mathrm{read}_e$ for each
/// read $e$ of its executions (`ExecutionCosts::resident_reads`: an edge
/// $y$ or a query output $z$; the read's mean windows, its most when
/// `costs` is peak memory). Its quantity per window is positive, so at the
/// optimum $R_v s_v$ is the most over the selected reads, what `plan_cost`
/// charges. The variables are added to `problem` and (W) to `rows`; returns
/// each view's term ($R_v$ times its quantity per window, $s_v$), unscaled.
///
/// $r_v$ itself, in windows, would be unbounded with whole-number row
/// coefficients, the shape [`RetentionShare`] avoids for $h_v$.
pub(crate) fn resident_window_terms(
    costs: &ExecutionCosts,
    selectable: &crate::selectable::SelectableExecutions,
    y: &BTreeMap<usize, Variable>,
    z: &BTreeMap<(QueryId, usize), Variable>,
    problem: &mut ProblemVariables,
    rows: &mut Vec<Constraint>,
) -> Vec<(f64, Variable)> {
    // Each read kept: (view, its windows, its read variable).
    let kept_reads: Vec<(PhysicalCandidateId, f64, Variable)> = costs
        .resident_reads
        .iter()
        .filter_map(|read| {
            let v = selectable.executions()[read.execution].candidate;
            let per_window = *costs.per_resident_window.get(&v)?;
            let read_variable = read.reader.variable(read.execution, y, z)?;
            let windows = read.resident_windows;
            (per_window > 0.0 && windows > 0.0 && windows.is_finite()).then_some((
                v,
                windows,
                read_variable,
            ))
        })
        .collect();
    let mut most_resident_windows: BTreeMap<PhysicalCandidateId, f64> = BTreeMap::new();
    for &(v, read_windows, _) in &kept_reads {
        let most = most_resident_windows.entry(v).or_default();
        *most = most.max(read_windows);
    }
    let mut resident: BTreeMap<PhysicalCandidateId, Variable> = BTreeMap::new();
    for (v, read_windows, read_variable) in kept_reads {
        let share = *resident
            .entry(v)
            .or_insert_with(|| problem.add(variable().min(0).max(1)));
        rows.push(constraint!(
            most_resident_windows[&v] * share >= read_windows * read_variable
        ));
    }
    resident
        .into_iter()
        .map(|(v, share)| {
            (
                costs.per_resident_window[&v] * most_resident_windows[&v],
                share,
            )
        })
        .collect()
}

/// The quantity of the windows the selected reads keep resident: per view,
/// its quantity per window times the most windows any selected read keeps
/// (`edges` by position in `SelectableExecutions::edges`, `outputs` the
/// query outputs and the executions answering them): $\sum_v$ of its
/// $r_v$ term at the optimum.
pub(crate) fn resident_windows_quantity(
    costs: &ExecutionCosts,
    selectable: &crate::selectable::SelectableExecutions,
    edges: &[usize],
    outputs: &[(QueryId, usize)],
) -> f64 {
    let edges: HashSet<usize> = edges.iter().copied().collect();
    let outputs: HashSet<(QueryId, usize)> = outputs.iter().copied().collect();
    let mut most: HashMap<PhysicalCandidateId, f64> = HashMap::new();
    for read in &costs.resident_reads {
        if !read.reader.is_selected(read.execution, &edges, &outputs) {
            continue;
        }
        let v = selectable.executions()[read.execution].candidate;
        let windows = read.resident_windows;
        let kept = most.entry(v).or_default();
        *kept = kept.max(windows);
    }
    most.into_iter()
        .map(|(v, windows)| costs.per_resident_window.get(&v).copied().unwrap_or(0.0) * windows)
        .sum()
}
