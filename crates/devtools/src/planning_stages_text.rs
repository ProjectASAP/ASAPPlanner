//! Every planner stage's output as plain text, for reading one workload's
//! planning end to end (`print_planning_stages`): the frontends' roots
//! (Stage 0), the memo groups (Stage 0 → Stage 1), Pass 1's
//! `PerComputationLogicalMemoCandidates`, the memo after Pass 2's local
//! window composition, Pass 2's `LogicalASAPMemoCandidates` (Stage 2's
//! input), Stage 2's `PhysicalASAPMemoCandidates` (Stage 3's input) and
//! Stage 3's `Selection`. Planning never reads it.
//!
//! Notation: `G<n>` is memo group `n`; `G<n>[<i>]` its candidate `i`;
//! `O<n>` Stage 2's operator node `n`; `C<n>` physical candidate `n`; a
//! candidate's chain lists its own nodes root first, down to its input
//! groups.

use std::collections::HashSet;
use std::fmt::Write;
use std::rc::Rc;

use asap_planner::{PlanSearchSettings, PlanningModels};
use asap_types::ir::flat::flatten;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::stages::{MemoCandidates, MemoGroupId, PhysicalASAPMemoCandidates};
use asap_types::workload::PlanningWorkload;

use crate::candidate_labels::node_label;
use crate::planning_stages::{
    lower_promql_roots, run_search_over, PlanningStagesError, PlanningStagesRun,
};

/// Plans the PromQL `workload` as the facade does, keeping every stage's
/// memo (`PlanSearchSettings::inspect`), without the reference enumeration.
pub fn plan_promql_with_stage_outputs(
    workload: &PlanningWorkload,
) -> Result<PlanningStagesRun, String> {
    let roots = lower_promql_roots(workload)?;
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap_or_default();
    run_search_over(
        roots,
        &entries,
        &data,
        PlanningModels::builtin(),
        PlanSearchSettings {
            inspect: true,
            ..PlanSearchSettings::default()
        },
        None,
    )
    .map_err(|error: PlanningStagesError| error.to_string())
}

/// Every stage's output of `stages` (planned with
/// `PlanSearchSettings::inspect`) as text, one section per stage boundary.
/// A stage that did not run is reported as missing.
pub fn planning_stages_text(stages: &PlanningStagesRun) -> String {
    let mut out = String::new();
    let run = &stages.run;

    section(
        &mut out,
        "Stage 0 output: the frontends' roots (Vec<QueryRoot>)",
    );
    let (flat, _) = flatten(&stages.stage0_query_language_frontends);
    for (id, node) in flat.nodes.iter().enumerate() {
        let children: Vec<String> = node
            .operator
            .children()
            .into_iter()
            .map(|child| format!("n{child}"))
            .collect();
        let _ = writeln!(out, "  n{id}: {}  <- {children:?}", node_label(node));
    }
    let _ = writeln!(out, "  roots: {:?}", flat.roots);

    let Some(pass1) = &run.per_computation_candidates else {
        let _ = writeln!(out, "\n(no Pass 1 output)");
        return out;
    };
    section(
        &mut out,
        "Stage 0 → Stage 1: DagOfLogicalMemoGroups (the roots hash-consed)",
    );
    write_memo(&mut out, &pass1.memo, false);

    section(
        &mut out,
        "Stage 1 Pass 1 output: PerComputationLogicalMemoCandidates",
    );
    write_memo(&mut out, &pass1.memo, true);
    let _ = writeln!(out, "  computation_targets:");
    for target in &pass1.computation_targets {
        let _ = writeln!(
            out,
            "    G{} measure {}: ways_to_compute={:?} fused={:?}",
            target.aggregate_group.0,
            target.measure_index,
            target.ways_to_compute,
            target
                .fused_computation
                .as_ref()
                .map(|fused| &fused.ways_to_compute)
        );
    }
    let _ = writeln!(out, "  pruned_candidates:");
    for pruned in &pass1.pruned_candidates {
        let _ = writeln!(
            out,
            "    G{}: {:?}  reason={:?}",
            pruned.group.0, pruned.candidate, pruned.reason
        );
    }

    if let Some(steps) = &run.memo_steps {
        section(
            &mut out,
            "Stage 1 Pass 2, after local window composition (pane candidates)",
        );
        write_memo(&mut out, &steps.pass2, true);
    }
    match &run.logical_asap_candidates {
        Some(logical) => {
            section(
                &mut out,
                "Stage 1 Pass 2 output = Stage 2 input: LogicalASAPMemoCandidates",
            );
            write_memo(&mut out, &logical.memo, true);
        }
        None => {
            let _ = writeln!(out, "\n(no Pass 2 output)");
        }
    }
    match &run.physical_asap_candidates {
        Some(physical) => {
            section(
                &mut out,
                "Stage 2 output = Stage 3 input: PhysicalASAPMemoCandidates",
            );
            write_physical(&mut out, physical);
        }
        None => {
            let _ = writeln!(out, "\n(no Stage 2 output)");
        }
    }

    section(&mut out, "Stage 3 output: Selection");
    match &run.selection {
        Ok(selection) => {
            let dag = &selection.selected_dag;
            for (node, candidate) in dag.candidate_of_node.iter().enumerate() {
                let inputs: Vec<String> = dag
                    .edges
                    .iter()
                    .filter(|edge| edge.consumer.0 == node)
                    .map(|edge| format!("input {} <- node{}", edge.input, edge.producer.0))
                    .collect();
                let _ = writeln!(out, "  node{node}: runs C{}  {inputs:?}", candidate.0);
            }
            let outputs: Vec<usize> = dag.node_of_output_reference.iter().map(|n| n.0).collect();
            let _ = writeln!(out, "  node_of_output_reference: {outputs:?}");
            let _ = writeln!(
                out,
                "  cost_per_second: {:?}",
                selection.cost.cost_per_second
            );
        }
        Err(error) => {
            let _ = writeln!(out, "  error: {error}");
        }
    }
    out
}

fn section(out: &mut String, title: &str) {
    let _ = writeln!(out, "\n==================== {title} ====================");
}

fn group_names(groups: &[MemoGroupId]) -> Vec<String> {
    groups.iter().map(|group| format!("G{}", group.0)).collect()
}

/// `memo`'s groups, and with `with_candidates` each group's candidates.
fn write_memo(out: &mut String, memo: &MemoCandidates, with_candidates: bool) {
    for (id, group) in memo.groups.iter().enumerate() {
        let _ = writeln!(
            out,
            "  G{id}: {}   children={:?} parents={:?} queries={:?}",
            root_label(&group.logical_node),
            group_names(&group.child_groups),
            group_names(group.parent_groups()),
            group.reached_by_queries()
        );
        if !with_candidates {
            continue;
        }
        for (index, candidate) in memo.candidates[id].iter().enumerate() {
            let fuses = if candidate.fused_groups.is_empty() {
                String::new()
            } else {
                format!(" fuses={:?}", group_names(&candidate.fused_groups))
            };
            let _ = writeln!(
                out,
                "      [{index}] {:<12} {}   inputs={:?}{fuses} accuracy_budget_parts={}",
                format!("{:?}", candidate.origin),
                chain_label(memo, &candidate.root_node, &candidate.input_groups),
                group_names(&candidate.input_groups),
                candidate.accuracy_budget_parts
            );
        }
    }
    let roots: Vec<String> = memo
        .query_roots
        .iter()
        .map(|root| match root {
            QueryRoot::Operator(group) => format!("G{}", group.0),
            scalar => format!("{scalar:?}"),
        })
        .collect();
    let _ = writeln!(out, "  query_roots: {roots:?}");
}

fn write_physical(out: &mut String, physical: &PhysicalASAPMemoCandidates) {
    let nodes = &physical.operator_nodes_of_candidates_for_memo_groups;
    let _ = writeln!(
        out,
        "  operator_nodes_of_candidates_for_memo_groups ({}):",
        nodes.len()
    );
    for (id, node) in nodes.iter().enumerate() {
        let implements: Vec<String> = node
            .candidates_for_memo_group
            .iter()
            .map(|candidate| format!("G{}[{}]", candidate.group.0, candidate.index))
            .collect();
        let _ = writeln!(
            out,
            "    O{id}: {}   implements={implements:?} is_implementation_root={} accuracy_budget_parts={}",
            root_label(&node.physical_node),
            node.is_implementation_root,
            node.accuracy_budget_parts
        );
    }
    let _ = writeln!(out, "  candidates ({}):", physical.candidates.len());
    for (id, candidate) in physical.candidates.iter().enumerate() {
        let producers: Vec<String> = candidate
            .producer_candidates_per_input
            .iter()
            .map(|producers| {
                let ids: Vec<String> = producers.iter().map(|p| format!("C{}", p.0)).collect();
                format!("{{{}}}", ids.join(","))
            })
            .collect();
        let _ = writeln!(
            out,
            "    C{id}: O{} {:?} {:?}  producers per input: [{}]{}",
            candidate.operator_node.0,
            candidate.timing,
            candidate.materialization,
            producers.join(" "),
            if candidate.may_produce_query_output {
                "  (may produce a query output)"
            } else {
                ""
            }
        );
    }
    let _ = writeln!(out, "  query_roots: {:?}", physical.query_roots);
    let _ = writeln!(
        out,
        "  reader_constraints ({}):",
        physical.reader_constraints.len()
    );
    for constraint in &physical.reader_constraints {
        let _ = writeln!(out, "    {constraint:?}");
    }
    let _ = writeln!(
        out,
        "  pruned_candidates ({}):",
        physical.pruned_candidates.len()
    );
    for pruned in &physical.pruned_candidates {
        let key = &pruned.candidate;
        let _ = writeln!(
            out,
            "    G{} O{} {:?} {:?}  reason={:?}",
            pruned.group.0, key.operator_node.0, key.timing, key.materialization, pruned.reason
        );
    }
}

/// The label of `node`'s own operator, without its children.
fn root_label(node: &Rc<OperatorNode>) -> String {
    let (flat, _) = flatten(&[QueryRoot::Operator(node.clone())]);
    flat.nodes.last().map(node_label).unwrap_or_default()
}

/// A candidate's own nodes, from `root` down to its `input_groups`' nodes
/// (excluded), root first, joined by " ← ".
fn chain_label(
    memo: &MemoCandidates,
    root: &Rc<OperatorNode>,
    input_groups: &[MemoGroupId],
) -> String {
    let (flat, originals) = flatten(&[QueryRoot::Operator(root.clone())]);
    let is_input = |id: usize| {
        input_groups
            .iter()
            .any(|group| Rc::ptr_eq(&memo.groups[group.0 as usize].logical_node, &originals[id]))
    };
    let mut labels = Vec::new();
    let mut pending = vec![flat.nodes.len() - 1];
    let mut seen = HashSet::new();
    while let Some(id) = pending.pop() {
        if is_input(id) || !seen.insert(id) {
            continue;
        }
        labels.push(node_label(&flat.nodes[id]));
        pending.extend(
            flat.nodes[id]
                .operator
                .children()
                .into_iter()
                .rev()
                .copied(),
        );
    }
    labels.join(" ← ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner_layering_examples::example1;
    use asap_types::workload::Query;

    /// One repeating `sum(rate(...[1m]))` query: every section is printed,
    /// Pass 2 adds pane candidates with their layout, and Stage 2 offers the
    /// pane chain at ingestion time.
    #[test]
    fn prints_every_stage_of_a_repeating_rate_query() {
        let mut workload = example1();
        let queries = workload.query_workload.repeating_queries.as_mut().unwrap();
        queries.truncate(1);
        queries[0].query = Query("sum by (job) (rate(http_requests_total[1m]))".into());

        let text = planning_stages_text(&plan_promql_with_stage_outputs(&workload).unwrap());

        for heading in [
            "Stage 0 output",
            "Stage 0 → Stage 1: DagOfLogicalMemoGroups",
            "Stage 1 Pass 1 output: PerComputationLogicalMemoCandidates",
            "Stage 1 Pass 2, after local window composition",
            "Stage 2 input: LogicalASAPMemoCandidates",
            "Stage 3 input: PhysicalASAPMemoCandidates",
            "Stage 3 output: Selection",
        ] {
            assert!(text.contains(heading), "missing {heading}:\n{text}");
        }
        assert!(text.contains(
            "Panes        FinalizeExactAccumulator ← SummaryMerge ← PaneChain pane_count=6 pane_width_ms=10000"
        ));
        assert!(text.contains("IngestionTime MatAtIngestion(Memory)"));
        assert!(text.contains("cost_per_second: Some("));
    }
}
