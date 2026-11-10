//! Stage 3's selection MILP, named after the formulation (concepts doc,
//! sections 3–5).
//!
//! | Family | Variables | Constraints |
//! |---|---|---|
//! | baseline (section 3) | $x_v$, $y_{u,c,i}$, $z_{q,v}$, $t_v$ ([`SelectionVariables`]) | (A) [`input_satisfaction_constraints`], (B) [`producer_selection_constraints`], (C)/(D) [`query_coverage_constraints`], (E) [`acyclicity_constraints`], (F) [`utilization_constraints`]; (G), (H): $y$ exists only on admissible edges |
//! | execution contexts (section 5) | $x^{\mathrm{mat}}_v$, $x_{\kappa,v}$, $y_{\kappa,u,c,i}$, $h_v$ ([`ExecutionContextVariables`]) | (R) [`retention_constraints`] |
//! | accuracy (section 4) | $E_{c,m}$, $F_{c,i,m}$, $D_c$, $H_{c,i}$ ([`AccuracyVariables`]) | (K) [`accuracy_constraints`] |
//!
//! Every function body is `todo!()` until #1207 PRs 6 (baseline), 7
//! (contexts) and 8 (accuracy) fill them in; until then nothing calls
//! them, hence the `dead_code` allowance below.
#![allow(dead_code)]

use std::collections::HashMap;

use good_lp::{Constraint, Variable};

use asap_types::accuracy::AccuracyMetric;
use asap_types::stages::{
    ExecutionContextId, PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdgeId,
};

/// A query, by index in the workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct QueryId(pub usize);

/// The baseline variables (section 3).
struct SelectionVariables {
    /// $x_v$, by [`PhysicalCandidateId`].
    x: Vec<Variable>,
    /// $y_{u,c,i}$, on admissible edges only.
    y: HashMap<PhysicalEdgeId, Variable>,
    /// $z_{q,v}$, for $v \in G_q$.
    z: HashMap<(QueryId, PhysicalCandidateId), Variable>,
    /// $t_v$, by [`PhysicalCandidateId`]: only for candidates on a cycle of
    /// admissible edges (a strongly connected component); `None` elsewhere.
    t: Vec<Option<Variable>>,
}

/// The execution-context variables (section 5), which replace $x$ and the
/// unindexed $y$ of the baseline.
struct ExecutionContextVariables {
    /// $x^{\mathrm{mat}}_v$, for an `AcrossEvaluations` candidate.
    x_materialized: HashMap<PhysicalCandidateId, Variable>,
    /// $x_{\kappa,v}$, for a candidate with $\ell$ = None or `OneEvaluation`.
    x_in_execution_context: HashMap<(ExecutionContextId, PhysicalCandidateId), Variable>,
    /// $y_{\kappa,u,c,i}$, indexed by the consumer's context.
    y_in_execution_context: HashMap<(ExecutionContextId, PhysicalEdgeId), Variable>,
    /// $h_v$, for an `AcrossEvaluations` producer.
    retention_amount: HashMap<PhysicalCandidateId, Variable>,
}

/// The accuracy variables of constraint (K) (section 4), all continuous.
struct AccuracyVariables {
    /// $E_{c,m}$: candidate $c$'s error bound in metric $m$.
    error_bound: HashMap<(PhysicalCandidateId, AccuracyMetric), Variable>,
    /// $F_{c,i,m}$: the chosen producer's error bound in metric $m$ on input
    /// $i$ of $c$.
    error_bound_of_input: HashMap<(PhysicalCandidateId, usize, AccuracyMetric), Variable>,
    /// $D_c$, by [`PhysicalCandidateId`].
    failure_probability: Vec<Variable>,
    /// $H_{c,i}$: the chosen producer's failure probability on input $i$ of $c$.
    failure_probability_of_input: HashMap<(PhysicalCandidateId, usize), Variable>,
}

/// (A) $\sum_{u \in P_{c,i}} y_{u,c,i} = x_c$.
fn input_satisfaction_constraints(
    physical: &PhysicalASAPMemoCandidates,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    let _ = (physical, variables);
    todo!("#1207 PR 6: constraint (A)")
}

/// (B) $y_{u,c,i} \le x_u$.
fn producer_selection_constraints(
    physical: &PhysicalASAPMemoCandidates,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    let _ = (physical, variables);
    todo!("#1207 PR 6: constraint (B)")
}

/// (C) $\sum_{v \in G_q} z_{q,v} = 1$; (D) $z_{q,v} \le x_v$.
fn query_coverage_constraints(
    physical: &PhysicalASAPMemoCandidates,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    let _ = (physical, variables);
    todo!("#1207 PR 6: constraints (C) and (D)")
}

/// (E) $t_u + 1 \le t_c + n(1 - y_{u,c,i})$, only on edges inside a strongly
/// connected component of the admissible edges, $n$ its size.
fn acyclicity_constraints(
    physical: &PhysicalASAPMemoCandidates,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    let _ = (physical, variables);
    todo!("#1207 PR 6: constraint (E)")
}

/// (F) $x_v \le \sum_{(v,c,i)} y_{v,c,i} + \sum_q z_{q,v}$.
fn utilization_constraints(
    physical: &PhysicalASAPMemoCandidates,
    variables: &SelectionVariables,
) -> Vec<Constraint> {
    let _ = (physical, variables);
    todo!("#1207 PR 6: constraint (F)")
}

/// (R) $h_v \ge \operatorname{need}_{\kappa,v,c,i}\, y_{\kappa,v,c,i}$: for a
/// time-indexed producer (raw data, a chain, results stored per evaluation
/// time) the reading path's offset plus range, a constant per edge, so (R)
/// is linear. Query latency is ignored: a result counts as read when its
/// evaluation starts.
fn retention_constraints(
    physical: &PhysicalASAPMemoCandidates,
    variables: &ExecutionContextVariables,
) -> Vec<Constraint> {
    let _ = (physical, variables);
    todo!("#1207 PR 7: constraint (R)")
}

/// (K): $E_{c,m}$ per candidate and metric and $D_c$ per candidate, big-M on
/// selected edges, the metric rule on the selected path, and the root
/// checks $E_{v,m_q} \le \varepsilon_q + M_q(1 - z_{q,v})$,
/// $D_v \le \delta_q + (1 - z_{q,v})$.
fn accuracy_constraints(
    physical: &PhysicalASAPMemoCandidates,
    selection: &SelectionVariables,
    accuracy: &AccuracyVariables,
) -> Vec<Constraint> {
    let _ = (physical, selection, accuracy);
    todo!("#1207 PR 8: constraint (K)")
}
