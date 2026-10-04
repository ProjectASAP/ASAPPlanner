//! [`StagePipeline`] — the #509 planner stages behind the
//! [`OptimizationPass`](super::OptimizationPass) trait.
//!
//! [`plan_stages`] runs them: Stage 1 lists each target's local alternatives
//! (Pass 1) with and without identical sub-DAGs shared across queries (Pass
//! 2's identical-expression rule), Stage 2 implements a candidate physically
//! (everything at query time), and Stage 3 checks accuracy, prices it and
//! chooses. Pass 2's other rules are not planned yet.

use asap_types::ir::schema_support::with_promql_series_identity;
use asap_types::ir::QueryRoot;
use asap_types::workload::QueryLanguage;

use super::{OptimizationInput, OptimizationPass, OptimizeError, PlanOutput, QueryPlan};
use asap_plan_selection::plan_stages;

#[derive(Debug, Default, Clone, Copy)]
pub struct StagePipeline;

impl OptimizationPass for StagePipeline {
    fn name(&self) -> &'static str {
        "stage-pipeline"
    }

    fn optimize(&self, input: OptimizationInput<'_>) -> Result<PlanOutput, OptimizeError> {
        let workload = input.workload;
        let mut output = PlanOutput::new(Vec::new());
        output.scalar_roots = workload.scalar_roots().to_vec();
        if workload.exprs().is_empty() {
            return Ok(output);
        }
        let promql = workload.query_workload().language == QueryLanguage::PromQL;
        // PromQL rows carry each series' full identity as a column: the row
        // representation per-series state needs at runtime. A query with no
        // such representation is planned over its labels alone.
        let roots: Vec<(usize, QueryRoot)> = workload
            .operator_indices()
            .iter()
            .copied()
            .zip(workload.exprs())
            .map(|(index, root)| {
                let root = match promql {
                    true => with_promql_series_identity(root).unwrap_or_else(|_| root.clone()),
                    false => root.clone(),
                };
                (index, QueryRoot::Operator(root))
            })
            .collect();
        let targets: Vec<_> = workload
            .entries()
            .map(|(entry, _)| Some(entry.requirements.accuracy.target()))
            .collect();
        let data = workload.data_workload().cloned().unwrap_or_default();
        let plan = plan_stages(roots, &targets, &data, input.models, 0)?.plan;

        output.plans = plan
            .logical
            .iter()
            .zip(plan.physical.roots)
            .map(|((entry_index, _), root)| QueryPlan {
                entry_index: *entry_index,
                root,
            })
            .collect();
        output.selection = Some(plan.selection);
        Ok(output)
    }
}
