//! The planner stages' output as plain text: `asap_planner --format text`.
//! It prints, up to the run's last stage, the frontends' roots and their
//! memo groups (Stage 0), Pass 1's `Memo`,
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

use std::fmt::Write;

use asap_types::ir::flat::flatten;
use asap_types::ir::QueryRoot;
use asap_types::stages::{Memo, MemoGroup, PhysicalASAPMemoCandidates};

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
        "Stage 0 → Stage 1: Memo (the roots hash-consed, without candidates)",
    );
    if let Some(memo) = &stages.stage0_memo_groups {
        write_memo(&mut out, memo, None, false);
    }
    if until == RunUntil::Stage0 {
        return out;
    }

    let Some(pass1) = &run.pass1_memo else {
        failure(&mut out);
        return out;
    };
    section(&mut out, "Stage 1 Pass 1 output: Memo");
    write_memo(&mut out, pass1, None, true);
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
    write_memo(&mut out, pass2, Some(pass1), true);
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

/// `memo`'s groups, one line each with the child groups it reads, and with
/// `with_candidates` each group's candidates below it, marked `new` when
/// `previous` (the memo at the step before) has no such candidate.
fn write_memo(out: &mut String, memo: &Memo, previous: Option<&Memo>, with_candidates: bool) {
    // The group lines share one set of column widths, with each group's
    // candidates printed under its line.
    let group_rows: Vec<Vec<String>> = memo
        .groups()
        .iter()
        .enumerate()
        .map(|(id, group)| {
            let children: Vec<String> = group
                .expression
                .children()
                .into_iter()
                .map(|child| format!("G{}", child.0))
                .collect();
            vec![format!("G{id}:"), group_label(group), reads(&children)]
        })
        .collect();
    let mut group_lines = String::new();
    write_rows(&mut group_lines, "", group_rows);
    for (id, line) in group_lines.lines().enumerate() {
        let _ = writeln!(out, "{line}");
        if !with_candidates {
            continue;
        }
        let _ = (previous, &memo.groups()[id].candidates);
        todo!("#1207 PR 9: print each candidate's expression, logical properties and input requirements")
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
    let _ = (out, physical);
    todo!("#1207 PR 9: print each physical candidate's properties and its edges with α")
}

/// Stage 3's selection: the search's outcome, the selected DAG (each node's
/// candidate, operator and producer nodes), the plan's cost with each plan
/// node's charge, and each query's accepted accuracy; or why none was
/// selected.
fn write_selection(out: &mut String, stages: &PlanningStagesRun) {
    let _ = (out, stages);
    todo!("#1207 PR 9: print the selected DAG over the new physical candidates")
}

/// The label of `group`'s own operator, without its input groups.
fn group_label(group: &MemoGroup) -> String {
    let _ = group;
    todo!("#1207 PR 9: label a group by its expression")
}

#[cfg(test)]
mod tests {}
