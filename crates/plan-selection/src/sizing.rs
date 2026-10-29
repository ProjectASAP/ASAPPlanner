//! Sizing summaries for their queries' failure probabilities before pricing.

use std::collections::HashMap;
use std::rc::Rc;

use asap_accuracy::estimators::{hll, with_failure_probability};
use asap_logical_asap_optimization::pass1::per_computation_candidates::{
    approximate_inputs, distinct_bound_source, with_resized_summaries,
};
use asap_physical_asap_optimization::PhysicalWorkloadPlan;
use asap_types::ir::physical_export::compile_physical_asap_workload_with_node_ids;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::schema::SketchParams;
use asap_types::ir::{OperatorNode, QueryRoot, SchemaDerivationError};
use asap_types::workload::DataWorkload;

use crate::statistics::{series_read, values_per_result};

/// `candidate` with each summary's per-value failure probability enlarged
/// for its queries' targets (#669b), before pricing, so that cost reflects
/// the deeper sketch. A query with a target with failure probability `δ`,
/// over `n` approximate inputs ([`approximate_inputs`]) and covering `V`
/// values per evaluation ([`AccuracyScope::max_values_covered`](asap_types::ir::properties::AccuracyScope::max_values_covered): 1 for a
/// per-value target, at most `⌊1 / share⌋` for one over the groups holding
/// a record share, `G` ([`values_per_result`]) for the whole result), needs
/// each input to fail with probability at most `δ / (n · V)`: `n · V` such
/// failures union-bound to `δ` over the values the target covers. A summary
/// several queries read takes the smallest `δ / (n · V)` among them. The
/// parameters that depend on it are enlarged ([`with_failure_probability`];
/// classic HLL's precision from its source's distinct bound), and the
/// guarantees above are composed again. A query whose `V` is unbound (a
/// whole-result target of an unbound `G`) enlarges nothing: its target
/// stays unmet. Stage 1 sizes for ε and reads no data workload; only this
/// stage binds `G`. `Err` is the reason the candidate is rejected when its
/// resized summaries do not rebuild into a plan.
pub(crate) fn sized_for_failure_probability(
    candidate: &PhysicalWorkloadPlan,
    targets: &[AccuracyTarget],
    data: &DataWorkload,
) -> Result<PhysicalWorkloadPlan, String> {
    let resize_failed = |error: &dyn std::fmt::Display| {
        format!("resizing for the failure probability failed: {error}")
    };
    let roots = resize_for_failure_probability(&candidate.roots, targets, data)
        .map_err(|error| resize_failed(&error))?;
    if roots
        .iter()
        .zip(&candidate.roots)
        .all(|(sized, root)| sized == root)
    {
        return Ok(candidate.clone());
    }
    let mut dag = compile_physical_asap_workload_with_node_ids(&roots)
        .map_err(|error| resize_failed(&error))?
        .dag;
    // The DAG has the same nodes, so each keeps the placement Stage 2 gave it.
    if dag.nodes.len() == candidate.dag.nodes.len() {
        for (node, placed) in dag.nodes.iter_mut().zip(&candidate.dag.nodes) {
            if node.id == placed.id {
                node.output_state = placed.output_state;
                node.materialization = placed.materialization;
            }
        }
    }
    Ok(PhysicalWorkloadPlan {
        roots,
        dag,
        ..candidate.clone()
    })
}

/// `roots` with their summaries enlarged as [`sized_for_failure_probability`]
/// says: the plan plan selection prices and runs, for the logical roots of
/// the selected candidate too.
pub fn resize_for_failure_probability(
    roots: &[QueryRoot],
    targets: &[AccuracyTarget],
    data: &DataWorkload,
) -> Result<Vec<QueryRoot>, SchemaDerivationError> {
    let mut required: HashMap<*const OperatorNode, f64> = HashMap::new();
    for (root, target) in roots.iter().zip(targets) {
        let Some((target, delta)) = target
            .failure_probability
            .map(|delta| (target, delta))
            .filter(|(t, delta)| !t.is_exact() && *delta > 0.0)
        else {
            continue;
        };
        let values = target.accuracy_scope.max_values_covered(values_per_result(
            root,
            series_read(root, data),
            data,
        ));
        let Some(values) = values else { continue };
        let inputs = approximate_inputs(root);
        let per_input_failure_probability = delta / (inputs.len() as f64 * values as f64);
        for state in &inputs {
            let entry = required
                .entry(Rc::as_ptr(state))
                .or_insert(per_input_failure_probability);
            *entry = entry.min(per_input_failure_probability);
        }
    }
    if required.is_empty() {
        return Ok(roots.to_vec());
    }
    let resize = |state: &OperatorNode, params: &SketchParams| {
        let delta = *required.get(&std::ptr::from_ref(state))?;
        Some(match params {
            SketchParams::Hll {
                precision,
                relative_error,
            } => {
                let source = distinct_bound_source(state)?;
                let bound = data.sources.get(&source)?.max_distinct_per_evaluation?;
                // The guarantee keeps the relative error Pass 1 sized for.
                let relative_error = hll::classic_relative_error(*precision, *relative_error);
                SketchParams::Hll {
                    precision: hll::min_precision(relative_error, delta, bound, *precision),
                    relative_error: Some(relative_error),
                }
            }
            params => with_failure_probability(params, delta),
        })
    };
    with_resized_summaries(roots, &resize)
}
