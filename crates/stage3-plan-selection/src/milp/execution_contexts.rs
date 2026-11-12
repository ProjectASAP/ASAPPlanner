//! The execution-context extension of the selection MILP
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! section 5): its variables and constraint (R), `todo!()` until execution
//! contexts are selected.

use std::collections::HashMap;

use good_lp::{Constraint, Variable};

use asap_types::stages::{
    ExecutionContextId, PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdgeId,
};

/// The execution-context variables (section 5), which replace $x$ and the
/// unindexed $y$ of the baseline.
#[allow(dead_code)] // Read once execution contexts are selected.
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

/// (R) $h_v \ge \operatorname{need}_{\kappa,v,c,i}\, y_{\kappa,v,c,i}$: for a
/// time-indexed producer (raw data, a chain, results stored per evaluation
/// time) the reading path's offset plus range, a constant per edge, so (R)
/// is linear. Query latency is ignored: a result counts as read when its
/// evaluation starts.
#[allow(dead_code)] // Called once execution contexts are selected.
fn retention_constraints(
    physical: &PhysicalASAPMemoCandidates,
    variables: &ExecutionContextVariables,
) -> Vec<Constraint> {
    let _ = (physical, variables);
    todo!("#1207 PR 7: constraint (R)")
}
