//! Stage 3's selection MILP, named after the formulation (concepts doc,
//! sections 3–5).
//!
//! | Family | Variables | Constraints |
//! |---|---|---|
//! | baseline (section 3) | $x_v$, $y_{u,c,i}$, $z_{q,v}$, $t_v$ ([`SelectionVariables`]) | (A) [`input_satisfaction_constraints`], (B) [`producer_selection_constraints`], (C)/(D) [`query_coverage_constraints`], (E) [`acyclicity_constraints`], (F) [`utilization_constraints`]; (G), (H): $y$ exists only on admissible edges |
//! | execution contexts (section 5) | $x^{\mathrm{mat}}_v$, $x_{\kappa,v}$, $y_{\kappa,u,c,i}$, $h_v$ ([`ExecutionContextVariables`]) | (R) [`retention_constraints`] |
//! | accuracy (section 4) | $E_c$, $F_{c,i}$, $D_c$, $H_{c,i}$ ([`AccuracyVariables`]) | (K) [`accuracy_constraints`] |
//!
//! Every function body is `todo!()` until #1207 PRs 6 (baseline), 7
//! (contexts) and 8 (accuracy) fill them in; until then nothing calls
//! them, hence the `dead_code` allowance below.
#![allow(dead_code)]

use std::collections::HashMap;

use good_lp::{Constraint, Variable};

use asap_types::physical::EvaluationSchedule;
use asap_types::stages::{PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdgeId};

/// A query, by index in the workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct QueryId(pub usize);

/// A refresh window of the workload's ingestion pipelines, by index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RefreshWindowId(pub usize);

/// Index of a [`ExecutionContext`] among the workload's contexts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExecutionContextId(pub usize);

/// $\kappa$: a class of executions with a common owner and schedule, where
/// a candidate with $\ell$ = None or `OneEvaluation` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExecutionContext {
    /// The evaluation of every query with this schedule: queries with the
    /// same (interval, phase) are evaluated together and share its
    /// `OneEvaluation` results. A and B both every 10 s at phase 0 give one
    /// context. A non-periodic query has no context yet (out of scope).
    Evaluation(EvaluationSchedule),
    /// $I_r$: the ingestion pipeline of a refresh window.
    IngestionPipeline(RefreshWindowId),
    /// $\kappa_m$: inside an `AcrossEvaluations` candidate's own computation.
    Materialized(PhysicalCandidateId),
}

impl ExecutionContext {
    /// $T_\kappa$: an evaluation context's schedule; `None` for an ingestion
    /// pipeline or a materialized candidate's own computation, which is not
    /// an implicit compatibility pass.
    pub fn evaluation_schedule(&self) -> Option<EvaluationSchedule> {
        match self {
            ExecutionContext::Evaluation(schedule) => Some(*schedule),
            ExecutionContext::IngestionPipeline(_) | ExecutionContext::Materialized(_) => {
                todo!("#1207 PR 7: whether an ingestion or materialized context has times")
            }
        }
    }
}

/// The baseline variables (section 3).
struct SelectionVariables {
    /// $x_v$, by [`PhysicalCandidateId`].
    x: Vec<Variable>,
    /// $y_{u,c,i}$, on admissible edges only.
    y: HashMap<PhysicalEdgeId, Variable>,
    /// $z_{q,v}$, for $v \in G_q$.
    z: HashMap<(QueryId, PhysicalCandidateId), Variable>,
    /// $t_v$, by [`PhysicalCandidateId`].
    t: Vec<Variable>,
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
    /// $E_c$, by [`PhysicalCandidateId`].
    error_bound: Vec<Variable>,
    /// $F_{c,i}$: the chosen producer's error bound on input $i$ of $c$.
    error_bound_of_input: HashMap<(PhysicalCandidateId, usize), Variable>,
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

/// (E) $t_u + 1 \le t_c + |\mathcal C|(1 - y_{u,c,i})$.
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

/// (R) $h_v \ge \operatorname{need}_{\kappa,v,c,i}\, y_{\kappa,v,c,i}$.
///
/// Assumptions (decided 2026-11-06): an edge's need is the time horizon
/// the queries it serves read, a constant per edge (a 5m range over 10 s
/// windows needs 30 windows), so (R) is linear; query latency is ignored
/// until the latency design: a result counts as read when its evaluation
/// starts.
fn retention_constraints(
    physical: &PhysicalASAPMemoCandidates,
    variables: &ExecutionContextVariables,
) -> Vec<Constraint> {
    let _ = (physical, variables);
    todo!("#1207 PR 7: constraint (R)")
}

/// (K): $E_c$ and $D_c$ per candidate, big-M on selected edges, and the
/// root checks $E_v \le \varepsilon_q + M_q(1 - z_{q,v})$,
/// $D_v \le \delta_q + (1 - z_{q,v})$.
fn accuracy_constraints(
    physical: &PhysicalASAPMemoCandidates,
    selection: &SelectionVariables,
    accuracy: &AccuracyVariables,
) -> Vec<Constraint> {
    let _ = (physical, selection, accuracy);
    todo!("#1207 PR 8: constraint (K)")
}
