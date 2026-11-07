//! The planner stages' output as plain text: `asap_planner --format text`.
//! It prints, up to the run's last stage, the frontends' roots and their
//! memo groups (Stage 0), Pass 1's `Memo`, the memo after Pass 2's local
//! window composition and Pass 2's `LogicalASAPMemoCandidates` (Stage 2's
//! input), Stage 2's `PhysicalASAPMemoCandidates` (Stage 3's input: each
//! physical candidate with every edge into each of its inputs and
//! $\alpha$, then the candidates Stage 2 pruned), and Stage 3's `Selection`
//! (the selected DAG, each node's cost, each query's accepted accuracy and
//! the candidates Stage 3 pruned). Planning never reads it.
//!
//! Notation: `G<n>` is memo group `n` and `G<n>[<i>]` its candidate `i`;
//! `C<n>` is physical candidate `n`; `<- ...` lists what a line reads;
//! `new` marks a candidate the step added; `θ` and `ω` are a candidate's
//! parameters and window layout, `floors` its parameter floor per input;
//! an edge is `✓` when admissible, else `✗` with why:
//!
//! ```text
//! G4:  SummaryEstimate Quantile(q=0.99)  <- G3
//!     [0]  Pass1    SummaryEstimate Quantile(q=0.99) <- G3  exact  floors=[Kll(k=269)]
//!
//!     C32 = G4[0] (QueryTime, None)  SummaryEstimate Quantile(q=0.99)
//!         input 0 <- G3, requires θ ⪰ Kll(k=269):
//!             C14 = G3[4] (QueryTime, None)  ✓
//!     C6 = G2[0] (QueryTime, None)  Aggregate quantile(q=0.99)
//!         input 0 <- G1:
//!             C3 = G1[0] (QueryTime, None)       ✓
//!             C5 = G1[0] (IngestionTime, None)   ✗ α^phys = 0: consumer (QueryTime, None) cannot read producer (IngestionTime, None)
//! ```

use std::fmt::Write;

use asap_types::ir::flat::flatten;
use asap_types::ir::QueryRoot;
use asap_types::physical::PhysicalProperties;
use asap_types::stages::{
    producer_satisfies_logical_requirements, CandidateForMemoGroup, LogicalProperties,
    LogicalRequirements, Memo, PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdge,
    WindowLayout,
};

use crate::candidate_labels::{operator_label, parameters_label};
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
            vec![
                format!("n{id}:"),
                operator_label(&node.operator),
                reads(&children),
            ]
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
/// `with_candidates` each group's candidates below it: index, origin, `new`
/// when `previous` (the memo at the step before) has no such candidate, the
/// operator $e_v$ with its input groups, $p^{\log}_v$ (θ, ω), the fused
/// groups, and $r^{\log}_{c,i}$ per input (its parameter floor).
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
            vec![
                format!("G{id}:"),
                operator_label(&group.expression),
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
            .and_then(|memo| memo.groups().get(id))
            .map_or(&[][..], |group| group.candidates.as_slice());
        let rows = memo.groups()[id]
            .candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                let added = previous.is_some()
                    && !earlier
                        .iter()
                        .any(|before| same_candidate(before, candidate));
                vec![
                    format!("[{index}]"),
                    format!("{:?}", candidate.origin),
                    if added {
                        "new".to_owned()
                    } else {
                        String::new()
                    },
                    candidate_text(candidate),
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

/// Whether two candidates of one group are the same $(e_v, p^{\log}_v)$:
/// what marks a candidate `new` (text) or `added` (the viewer document).
pub fn same_candidate(left: &CandidateForMemoGroup, right: &CandidateForMemoGroup) -> bool {
    left.expression == right.expression
        && left.logical_properties.parameters() == right.logical_properties.parameters()
        && left.logical_properties.window_layout() == right.logical_properties.window_layout()
}

/// A candidate on one line: `SummaryAgg Kll <- G1  θ=Kll(k=269)  floors=[-]`.
fn candidate_text(candidate: &CandidateForMemoGroup) -> String {
    let operator = &candidate.expression.operator;
    let inputs: Vec<String> = operator
        .children()
        .into_iter()
        .map(|group| format!("G{}", group.0))
        .collect();
    let mut text = operator_label(operator);
    if !inputs.is_empty() {
        let _ = write!(text, " {}", reads(&inputs));
    }
    let _ = write!(
        text,
        "  {}",
        logical_properties_text(&candidate.logical_properties)
    );
    if !candidate.fused_groups.is_empty() {
        let fused: Vec<String> = candidate
            .fused_groups
            .iter()
            .map(|group| format!("G{}", group.0))
            .collect();
        let _ = write!(text, "  fuses=[{}]", fused.join(", "));
    }
    if candidate.input_requirements.iter().any(|requirement| {
        requirement
            .parameter_floor_for_required_accuracy()
            .is_some()
    }) {
        let floors: Vec<String> = candidate
            .input_requirements
            .iter()
            .map(requirement_text)
            .collect();
        let _ = write!(text, "  floors=[{}]", floors.join(", "));
    }
    text
}

/// $p^{\log}_v$: `θ=Kll(k=269) ω=tumbling 10000ms`, `exact` without θ.
fn logical_properties_text(properties: &LogicalProperties) -> String {
    let mut text = match properties.parameters() {
        Some(parameters) => format!("θ={}", parameters_label(parameters)),
        None => "exact".to_owned(),
    };
    if let Some(WindowLayout::Tumbling { width_ms }) = properties.window_layout() {
        let _ = write!(text, " ω=tumbling {width_ms}ms");
    }
    text
}

/// $r^{\log}_{c,i}$: its parameter floor, `-` without one.
fn requirement_text(requirement: &LogicalRequirements) -> String {
    requirement
        .parameter_floor_for_required_accuracy()
        .map_or("-".to_owned(), parameters_label)
}

/// `(QueryTime, None)`.
fn physical_properties_text(properties: &PhysicalProperties) -> String {
    format!("({:?}, {:?})", properties.timing(), properties.lifetime())
}

/// Why edge `edge` is not admissible, or `None` when it is: $\alpha^{\log}
/// = 0$ (the producer's θ is below the consumer's floor, or else the metric
/// rule: the consumer's error contract cannot carry the producer's error),
/// or $\alpha^{\mathrm{phys}} = 0$ (the producer's timing and lifetime
/// cannot serve the consumer's).
pub fn edge_rejection(
    physical: &PhysicalASAPMemoCandidates,
    edge: &PhysicalEdge,
) -> Option<String> {
    let memo = &physical.logical_candidates.memo;
    let producer = &physical.physical_candidates[edge.producer_candidate_id.0];
    let consumer = &physical.physical_candidates[edge.consumer_candidate_id.0];
    let producer_logical = memo.candidate(producer.logical_candidate_id);
    let requirement = &memo
        .candidate(consumer.logical_candidate_id)
        .input_requirements[edge.input_index_of_consumer];
    if !edge.satisfies_logical_requirement {
        return Some(
            if producer_satisfies_logical_requirements(
                &producer_logical.logical_properties,
                requirement,
            ) {
                format!(
                    "α^log = 0: metric rule, the consumer's error contract cannot carry the \
                     producer's error ({})",
                    logical_properties_text(&producer_logical.logical_properties)
                )
            } else {
                format!(
                    "α^log = 0: requires θ ⪰ {}, offers {}",
                    requirement_text(requirement),
                    logical_properties_text(&producer_logical.logical_properties)
                )
            },
        );
    }
    (!edge.satisfies_physical_requirement).then(|| {
        format!(
            "α^phys = 0: consumer {} cannot read producer {}",
            physical_properties_text(&consumer.physical_properties),
            physical_properties_text(&producer.physical_properties)
        )
    })
}

/// `C3 = G2[0] (QueryTime, None)`.
fn physical_candidate_text(
    physical: &PhysicalASAPMemoCandidates,
    id: PhysicalCandidateId,
) -> String {
    let candidate = &physical.physical_candidates[id.0];
    let logical = candidate.logical_candidate_id;
    format!(
        "C{} = G{}[{}] {}",
        id.0,
        logical.group.0,
        logical.index_in_group,
        physical_properties_text(&candidate.physical_properties)
    )
}

/// Stage 2's output: each physical candidate $v$ with its logical candidate
/// and $p^{\mathrm{phys}}_v$; under it, per input $(c, i)$, every producer
/// $u$ of the input group with $\alpha$ (`✓` admissible, `✗` and why
/// not); then the query roots and the candidates Stage 2 pruned.
fn write_physical(out: &mut String, physical: &PhysicalASAPMemoCandidates) {
    let memo = &physical.logical_candidates.memo;
    let _ = writeln!(
        out,
        "physical candidates ({}):",
        physical.physical_candidates.len()
    );
    let mut edges_into: Vec<Vec<Vec<PhysicalEdge>>> = physical
        .physical_candidates
        .iter()
        .map(|candidate| {
            let inputs = memo
                .candidate(candidate.logical_candidate_id)
                .expression
                .operator
                .children()
                .len();
            vec![Vec::new(); inputs]
        })
        .collect();
    for (_, edge) in physical.edges() {
        edges_into[edge.consumer_candidate_id.0][edge.input_index_of_consumer].push(edge);
    }
    for (id, inputs) in edges_into.iter().enumerate() {
        let candidate = &physical.physical_candidates[id];
        let logical = memo.candidate(candidate.logical_candidate_id);
        let _ = writeln!(
            out,
            "    {}  {}",
            physical_candidate_text(physical, PhysicalCandidateId(id)),
            operator_label(&logical.expression.operator)
        );
        let groups = logical.expression.operator.children();
        for (input, edges) in inputs.iter().enumerate() {
            let floor =
                match logical.input_requirements[input].parameter_floor_for_required_accuracy() {
                    Some(floor) => format!(", requires θ ⪰ {}", parameters_label(floor)),
                    None => String::new(),
                };
            let _ = writeln!(out, "        input {input} <- G{}{floor}:", groups[input].0);
            let rows = edges
                .iter()
                .map(|edge| {
                    let producer = physical_candidate_text(physical, edge.producer_candidate_id);
                    match edge_rejection(physical, edge) {
                        None => vec![producer, "✓".to_owned()],
                        Some(reason) => vec![producer, format!("✗ {reason}")],
                    }
                })
                .collect();
            write_rows(out, "            ", rows);
        }
    }
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
        "pruned by Stage 2 ({}):",
        physical.pruned_candidates.len()
    );
    let rows = physical
        .pruned_candidates
        .iter()
        .map(|pruned| {
            let key = &pruned.candidate;
            vec![
                format!(
                    "G{}[{}]",
                    key.logical_candidate_id.group.0, key.logical_candidate_id.index_in_group
                ),
                physical_properties_text(&key.physical_properties),
                format!("{:?}", pruned.reason),
            ]
        })
        .collect();
    write_rows(out, "    ", rows);
}

/// Stage 3's selection: the search's outcome, the selected DAG (each node's
/// physical candidate, its operator and producer nodes), the plan's cost
/// with each realized node's charge, each query's accepted accuracy, and
/// the candidates Stage 3 pruned; or why none was selected.
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
    let physical = run.physical_asap_candidates.as_ref();
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
            inputs.sort_by_key(|edge| edge.input_index_of_consumer);
            let producers: Vec<String> = inputs
                .iter()
                .map(|edge| format!("node{}", edge.producer.0))
                .collect();
            let (what, operator) = physical.map_or_else(
                || (format!("C{}", candidate.0), String::new()),
                |physical| {
                    let logical = physical.physical_candidates[candidate.0].logical_candidate_id;
                    (
                        physical_candidate_text(physical, *candidate),
                        operator_label(
                            &physical
                                .logical_candidates
                                .memo
                                .candidate(logical)
                                .expression
                                .operator,
                        ),
                    )
                },
            );
            vec![format!("node{node}:"), what, operator, reads(&producers)]
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
    // Keyed by the realized plan's node ids (`#n`), not the selected DAG's
    // (`node<n>`): the realizer expands a windowed build into a chain and
    // its merge.
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
    let _ = writeln!(
        out,
        "pruned by Stage 3 ({}):",
        selection.pruned_candidates.len()
    );
    let rows = selection
        .pruned_candidates
        .iter()
        .map(|pruned| {
            vec![
                physical.map_or_else(
                    || format!("C{}", pruned.candidate.0),
                    |physical| physical_candidate_text(physical, pruned.candidate),
                ),
                format!("{:?}", pruned.reason),
            ]
        })
        .collect();
    write_rows(out, "    ", rows);
}

#[cfg(test)]
mod tests {}
