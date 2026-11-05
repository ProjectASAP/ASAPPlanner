//! The planner stages' output as plain text: `asap_planner --format text`.
//! It prints, up to the run's last stage, the frontends' roots and their
//! memo groups (Stage 0), Pass 1's `PerComputationLogicalMemoCandidates`,
//! the memo after Pass 2's local window composition and Pass 2's
//! `LogicalASAPMemoCandidates` (Stage 2's input), Stage 2's
//! `PhysicalASAPMemoCandidates` (Stage 3's input), and Stage 3's
//! `Selection` with the selected memo choice, each node's cost and each
//! query's accepted accuracy. Planning never reads it.
//!
//! Notation: `G<n>` is memo group `n` and `G<n>[<i>]` its candidate `i`;
//! `O<n>` is Stage 2's operator node `n` and `C<n>` physical candidate `n`;
//! `<- ...` lists what a line reads; `new` marks a candidate the step
//! added. A candidate's chain lists its own nodes root first, down to its
//! input groups:
//!
//! ```text
//! G1:  TimeRange 60s     <- G0
//! G2:  Aggregate rate()  <- G1
//!     [0]  Pass1       FinalizeExactAccumulator ← SummaryAgg                                       inputs=[G1]
//!     [1]  TumblingWindows  new  FinalizeExactAccumulator ← SummaryMerge ← SummaryTumblingWindowChain tumbling_window_count=6 tumbling_window_width_ms=10000  inputs=[G0]
//! ```

use std::collections::HashSet;
use std::fmt::Write;
use std::rc::Rc;

use asap_planner::selected_chosen_candidates;
use asap_types::ir::flat::flatten;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::stages::{Memo, MemoGroupId, PhysicalASAPMemoCandidates};

use crate::candidate_labels::node_label;
use crate::planning_stages::{PlanningStagesRun, RunUntil};

/// The output of every stage `stages` ran, as text, one section per stage
/// boundary. A failure ends the text with its error.
pub fn planning_stages_text(stages: &PlanningStagesRun) -> String {
    let mut out = String::new();
    let run = &stages.run;
    let until = stages.run_until;
    // Why the next stage has no output (a failure, not the stop point).
    let failure = |out: &mut String| {
        if let Err(error) = &run.selection {
            let _ = writeln!(out, "\nerror: {error}");
        }
    };

    section(
        &mut out,
        "Stage 0 output: the frontends' roots (Vec<QueryRoot>)",
    );
    write_dag(&mut out, &stages.stage0_query_language_frontends);
    section(
        &mut out,
        "Stage 0 → Stage 1: DagOfLogicalMemoGroups (the roots hash-consed)",
    );
    if let Some(memo) = &stages.stage0_memo_groups {
        write_memo(&mut out, memo, None, false);
    }
    if until == RunUntil::Stage0 {
        return out;
    }

    let Some(pass1) = &run.per_computation_candidates else {
        failure(&mut out);
        return out;
    };
    section(
        &mut out,
        "Stage 1 Pass 1 output: PerComputationLogicalMemoCandidates",
    );
    write_memo(&mut out, &pass1.memo, None, true);
    let _ = writeln!(
        out,
        "pruned_candidates ({}):",
        pass1.pruned_candidates.len()
    );
    let rows = pass1
        .pruned_candidates
        .iter()
        .map(|pruned| {
            vec![
                format!("G{}:", pruned.group.0),
                format!("{:?}", pruned.candidate),
                format!("{:?}", pruned.reason),
            ]
        })
        .collect();
    write_rows(&mut out, "    ", rows);
    if until == RunUntil::Pass1 {
        return out;
    }

    let (Some(pass2), Some(logical)) = (
        &stages.pass2_local_window_composition,
        &run.logical_asap_candidates,
    ) else {
        failure(&mut out);
        return out;
    };
    section(
        &mut out,
        "Stage 1 Pass 2, after local window composition (tumbling-window candidates)",
    );
    write_memo(&mut out, pass2, Some(&pass1.memo), true);
    section(
        &mut out,
        "Stage 1 Pass 2 output = Stage 2 input: LogicalASAPMemoCandidates",
    );
    write_memo(&mut out, &logical.memo, Some(pass2), true);
    if until == RunUntil::Pass2 {
        return out;
    }

    let Some(physical) = &run.physical_asap_candidates else {
        failure(&mut out);
        return out;
    };
    section(
        &mut out,
        "Stage 2 output = Stage 3 input: PhysicalASAPMemoCandidates",
    );
    write_physical(&mut out, physical);
    if until == RunUntil::Stage2 {
        return out;
    }

    section(&mut out, "Stage 3 output: Selection");
    let chosen = match &run.selection {
        Ok(selection) => {
            selected_chosen_candidates(&logical.memo, physical, &selection.selected_dag)
        }
        Err(_) => Err("no plan was selected".into()),
    };
    match &chosen {
        Ok(choice) => {
            let _ = writeln!(out, "selected choice: {}", choice_text(choice));
        }
        Err(reason) => {
            let _ = writeln!(out, "selected choice unavailable: {reason}");
        }
    }
    write_selection(&mut out, stages);
    out
}

fn section(out: &mut String, title: &str) {
    let _ = writeln!(out, "\n==================== {title} ====================");
}

/// One line per node of the DAG of `roots` (children before parents), then
/// the query roots.
fn write_dag(out: &mut String, roots: &[QueryRoot]) {
    let (flat, _) = flatten(roots);
    let rows = flat
        .nodes
        .iter()
        .enumerate()
        .map(|(id, node)| {
            let children: Vec<String> = node
                .operator
                .children()
                .into_iter()
                .map(|child| format!("n{child}"))
                .collect();
            vec![format!("n{id}:"), node_label(node), reads(&children)]
        })
        .collect();
    write_rows(out, "", rows);
    let roots: Vec<String> = flat
        .roots
        .iter()
        .map(|root| match root {
            QueryRoot::Operator(id) => format!("n{id}"),
            scalar => format!("{scalar:?}"),
        })
        .collect();
    let _ = writeln!(out, "query_roots: [{}]", roots.join(", "));
}

/// `rows` with each column but the last padded to its widest cell, two
/// spaces apart, each line prefixed by `indent`; trailing empty cells are
/// dropped.
fn write_rows(out: &mut String, indent: &str, rows: Vec<Vec<String>>) {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns)
        .map(|column| {
            rows.iter()
                .filter_map(|row| row.get(column))
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    for row in rows {
        let last = row.iter().rposition(|cell| !cell.is_empty()).unwrap_or(0);
        let mut line = indent.to_owned();
        for (column, cell) in row.iter().enumerate().take(last + 1) {
            line.push_str(cell);
            if column < last {
                let padding = widths[column] - cell.chars().count() + 2;
                line.push_str(&" ".repeat(padding));
            }
        }
        let _ = writeln!(out, "{line}");
    }
}

/// `<- a, b`, or nothing when there is nothing read.
fn reads(inputs: &[String]) -> String {
    if inputs.is_empty() {
        String::new()
    } else {
        format!("<- {}", inputs.join(", "))
    }
}

/// `G0, G1`.
fn group_list(groups: &[MemoGroupId]) -> String {
    groups
        .iter()
        .map(|group| format!("G{}", group.0))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `G2:c0 G3:c0`: each chosen group's candidate.
fn choice_text(choice: &asap_logical_asap_optimization::ChosenCandidatePerMemoGroup) -> String {
    choice
        .iter()
        .map(|(group, candidate)| format!("G{}:c{candidate}", group.0))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `memo`'s groups, one line each with the child groups it reads, and with
/// `with_candidates` each group's candidates below it: index, origin,
/// `new` when `previous` (the memo at the step before) has no candidate of
/// that group with the same node and inputs, chain and input groups (fused
/// groups, and accuracy budget parts when more than one).
fn write_memo(out: &mut String, memo: &Memo, previous: Option<&Memo>, with_candidates: bool) {
    // The group lines share one set of column widths, with each group's
    // candidates printed under its line.
    let group_rows: Vec<Vec<String>> = memo
        .groups()
        .iter()
        .enumerate()
        .map(|(id, group)| {
            let children: Vec<String> = group
                .child_groups
                .iter()
                .map(|child| format!("G{}", child.0))
                .collect();
            vec![
                format!("G{id}:"),
                root_label(&group.logical_node),
                reads(&children),
            ]
        })
        .collect();
    let mut group_lines = String::new();
    write_rows(&mut group_lines, "", group_rows);
    for (id, line) in group_lines.lines().enumerate() {
        let _ = writeln!(out, "{line}");
        if !with_candidates {
            continue;
        }
        let earlier = previous
            .and_then(|memo| memo.candidates.get(id))
            .map_or(&[][..], Vec::as_slice);
        let rows = memo.candidates[id]
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                let added = previous.is_some()
                    && !earlier.iter().any(|before| {
                        Rc::ptr_eq(&before.root_node, &candidate.root_node)
                            && before.input_groups == candidate.input_groups
                    });
                let mut inputs = format!("inputs=[{}]", group_list(&candidate.input_groups));
                if !candidate.fused_groups.is_empty() {
                    let _ = write!(inputs, " fuses=[{}]", group_list(&candidate.fused_groups));
                }
                if candidate.accuracy_budget_parts != 1 {
                    let _ = write!(
                        inputs,
                        " accuracy_budget_parts={}",
                        candidate.accuracy_budget_parts
                    );
                }
                vec![
                    format!("[{index}]"),
                    format!("{:?}", candidate.origin),
                    if added {
                        "new".to_owned()
                    } else {
                        String::new()
                    },
                    chain_label(memo, &candidate.root_node, &candidate.input_groups),
                    inputs,
                ]
            })
            .collect();
        write_rows(out, "    ", rows);
    }
    let roots: Vec<String> = memo
        .query_roots
        .iter()
        .map(|root| match root {
            QueryRoot::Operator(group) => format!("G{}", group.0),
            scalar => format!("{scalar:?}"),
        })
        .collect();
    let _ = writeln!(out, "query_roots: [{}]", roots.join(", "));
}

fn write_physical(out: &mut String, physical: &PhysicalASAPMemoCandidates) {
    let nodes = &physical.operator_nodes_of_candidates_for_memo_groups;
    let _ = writeln!(
        out,
        "operator_nodes_of_candidates_for_memo_groups ({}):",
        nodes.len()
    );
    let rows = nodes
        .iter()
        .enumerate()
        .map(|(id, node)| {
            let implements: Vec<String> = node
                .candidates_for_memo_group
                .iter()
                .map(|candidate| format!("G{}[{}]", candidate.group.0, candidate.index))
                .collect();
            let mut role = if node.is_implementation_root {
                "implementation root".to_owned()
            } else {
                "private".to_owned()
            };
            if node.accuracy_budget_parts != 1 {
                let _ = write!(
                    role,
                    " accuracy_budget_parts={}",
                    node.accuracy_budget_parts
                );
            }
            vec![
                format!("O{id}:"),
                root_label(&node.physical_node),
                format!("implements=[{}]", implements.join(", ")),
                role,
            ]
        })
        .collect();
    write_rows(out, "    ", rows);

    let _ = writeln!(out, "candidates ({}):", physical.candidates.len());
    let rows = physical
        .candidates
        .iter()
        .enumerate()
        .map(|(id, candidate)| {
            // One `{...}` per input: the candidates that may produce it.
            let producers: Vec<String> = candidate
                .producer_candidates_per_input
                .iter()
                .map(|producers| {
                    let ids: Vec<String> = producers.iter().map(|p| format!("C{}", p.0)).collect();
                    format!("{{{}}}", ids.join(", "))
                })
                .collect();
            vec![
                format!("C{id}:"),
                format!("O{}", candidate.operator_node.0),
                format!("{:?}", candidate.timing),
                format!("{:?}", candidate.materialization),
                // It may produce a query's output.
                if candidate.may_produce_query_output {
                    "output".to_owned()
                } else {
                    String::new()
                },
                reads(&producers),
            ]
        })
        .collect();
    write_rows(out, "    ", rows);

    let output_groups: Vec<String> = physical
        .query_roots
        .iter()
        .map(|root| match root {
            QueryRoot::Operator(group) => format!("G{}", group.0),
            scalar => format!("{scalar:?}"),
        })
        .collect();
    let _ = writeln!(out, "query_roots: [{}]", output_groups.join(", "));
    let _ = writeln!(
        out,
        "reader_constraints ({}):",
        physical.reader_constraints.len()
    );
    for constraint in &physical.reader_constraints {
        let _ = writeln!(out, "    {constraint:?}");
    }
    let _ = writeln!(
        out,
        "pruned_candidates ({}):",
        physical.pruned_candidates.len()
    );
    let rows = physical
        .pruned_candidates
        .iter()
        .map(|pruned| {
            let key = &pruned.candidate;
            vec![
                format!("G{}", pruned.group.0),
                format!("O{}", key.operator_node.0),
                format!("{:?}", key.timing),
                format!("{:?}", key.materialization),
                format!("{:?}", pruned.reason),
            ]
        })
        .collect();
    write_rows(out, "    ", rows);
}

/// Stage 3's selection: the search's outcome, the selected DAG (each node's
/// candidate, operator and producer nodes), the plan's cost with each plan
/// node's charge, and each query's accepted accuracy; or why none was
/// selected.
fn write_selection(out: &mut String, stages: &PlanningStagesRun) {
    let run = &stages.run;
    let selection = match &run.selection {
        Ok(selection) => selection,
        Err(error) => {
            let _ = writeln!(out, "error: {error}");
            return;
        }
    };
    let report = &selection.search_report;
    let _ = writeln!(
        out,
        "search: {:?}, {} MILP variables, {} constraints, {} ms solving",
        report.status, report.milp_variables, report.milp_constraints, report.solver_ms
    );
    let dag = &selection.selected_dag;
    let rows = dag
        .candidate_of_node
        .iter()
        .enumerate()
        .map(|(node, candidate)| {
            let mut inputs: Vec<_> = dag
                .edges
                .iter()
                .filter(|edge| edge.consumer.0 == node)
                .collect();
            inputs.sort_by_key(|edge| edge.input);
            let producers: Vec<String> = inputs
                .iter()
                .map(|edge| format!("node{}", edge.producer.0))
                .collect();
            // The operator the candidate runs.
            let operator =
                run.physical_asap_candidates
                    .as_ref()
                    .map_or(String::new(), |physical| {
                        let node = physical.candidates[candidate.0].operator_node;
                        root_label(
                            &physical.operator_nodes_of_candidates_for_memo_groups[node.0]
                                .physical_node,
                        )
                    });
            vec![
                format!("node{node}:"),
                format!("C{}", candidate.0),
                operator,
                reads(&producers),
            ]
        })
        .collect();
    write_rows(out, "", rows);
    let outputs: Vec<String> = dag
        .node_of_output_reference
        .iter()
        .map(|node| format!("node{}", node.0))
        .collect();
    let _ = writeln!(out, "node_of_output_reference: [{}]", outputs.join(", "));
    let cost = &selection.cost;
    let _ = writeln!(
        out,
        "cost_per_second: {}",
        cost.cost_per_second
            .map_or("none".to_owned(), |cost| format!("{cost:.6}"))
    );
    // Keyed by the selected plan's node ids, as Stage 2 built it.
    let rows = cost
        .per_node
        .iter()
        .map(|(node, charge)| {
            vec![
                format!("#{node}"),
                format!("{:?}", charge.operator),
                format!("{:?}", charge.timing),
                charge
                    .cost_per_second
                    .map_or("none".to_owned(), |cost| format!("{cost:.6}")),
                charge.detail.clone(),
            ]
        })
        .collect();
    write_rows(out, "    ", rows);
    let _ = writeln!(out, "accepted accuracy:");
    for (query, accepted) in selection.accepted_accuracy_per_query.iter().enumerate() {
        let _ = writeln!(out, "    q{}: {accepted}", query + 1);
    }
}

/// The label of `node`'s own operator, without its children.
fn root_label(node: &Rc<OperatorNode>) -> String {
    let (flat, _) = flatten(&[QueryRoot::Operator(node.clone())]);
    flat.nodes.last().map(node_label).unwrap_or_default()
}

/// A candidate's own nodes, from `root` down to its `input_groups`' nodes
/// (excluded), root first, joined by " ← ".
fn chain_label(memo: &Memo, root: &Rc<OperatorNode>, input_groups: &[MemoGroupId]) -> String {
    let (flat, originals) = flatten(&[QueryRoot::Operator(root.clone())]);
    let is_input = |id: usize| {
        input_groups.iter().any(|group| {
            Rc::ptr_eq(
                &memo.groups()[group.0 as usize].logical_node,
                &originals[id],
            )
        })
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
    use crate::planning_stages::{lower_promql_roots, run_stages_until};
    use asap_planner::PlanningModels;
    use asap_types::workload::Query;

    /// One repeating `sum(rate(...[1m]))` query: every section is printed,
    /// Pass 2 adds tumbling-window candidates with their layout, Stage 2 offers
    /// the tumbling-window chain at ingestion time, and Stage 3 names the selected choice.
    #[test]
    fn prints_every_stage_of_a_repeating_rate_query() {
        let mut workload = example1();
        let queries = workload.query_workload.repeating_queries.as_mut().unwrap();
        queries.truncate(1);
        queries[0].query = Query("sum by (job) (rate(http_requests_total[1m]))".into());
        let roots = lower_promql_roots(&workload).unwrap();
        let entries: Vec<_> = workload.query_workload.entries().collect();
        let stages = run_stages_until(
            roots,
            &entries,
            &workload.data_workload.clone().unwrap(),
            PlanningModels::builtin(),
            RunUntil::Stage3,
        )
        .unwrap();

        let text = planning_stages_text(&stages);

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
        // A group line names its operator and the groups it reads.
        assert!(text.contains("G1:  TimeRange 60s     <- G0\n"), "{text}");
        // The line containing every one of `parts`, columns aside.
        let has_line = |parts: &[&str]| {
            text.lines()
                .any(|line| parts.iter().all(|part| line.contains(part)))
        };
        // Pass 2 adds a tumbling-window candidate reading the scan, its layout in its chain.
        assert!(
            has_line(&[
                "TumblingWindows  new  FinalizeExactAccumulator ← SummaryMerge ← SummaryTumblingWindowChain tumbling_window_count=6 tumbling_window_width_ms=10000",
                "inputs=[G0]",
            ]),
            "{text}"
        );
        // Stage 2 offers the tumbling-window chain at ingestion time, read from the
        // ingestion-time scan.
        assert!(
            has_line(&["IngestionTime  MatAtIngestion(Memory)", "<- {C1}"]),
            "{text}"
        );
        assert!(
            text.contains("selected choice: G0:c0 G1:c0 G2:c0 G3:c0\n"),
            "{text}"
        );
        assert!(text.contains("accepted accuracy:\n    q1: "), "{text}");
    }
}
