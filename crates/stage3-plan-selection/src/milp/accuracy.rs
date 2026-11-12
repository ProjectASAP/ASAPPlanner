//! The accuracy extension of the selection MILP
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! section 4): its variables and constraint (K), `todo!()` until (K) is
//! added.

use std::collections::HashMap;

use good_lp::{Constraint, Variable};

use asap_types::accuracy::AccuracyMetric;
use asap_types::stages::{PhysicalASAPMemoCandidates, PhysicalCandidateId};

use super::model::SelectionVariables;

/// The accuracy variables of constraint (K) (section 4), all continuous.
#[allow(dead_code)] // Read once constraint (K) is added.
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

/// (K): $E_{c,m}$ per candidate and metric and $D_c$ per candidate, big-M on
/// selected edges, the metric rule on the selected path, and the root
/// checks $E_{v,m_q} \le \varepsilon_q + M_q(1 - z_{q,v})$,
/// $D_v \le \delta_q + (1 - z_{q,v})$.
#[allow(dead_code)] // Called once constraint (K) is added.
fn accuracy_constraints(
    physical: &PhysicalASAPMemoCandidates,
    selection: &SelectionVariables,
    accuracy: &AccuracyVariables,
) -> Vec<Constraint> {
    let _ = (physical, selection, accuracy);
    todo!("#1207 PR 8: constraint (K)")
}
