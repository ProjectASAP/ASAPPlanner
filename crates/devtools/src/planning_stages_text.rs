//! The planner stages' output as plain text: `asap_planner --format text`.
//! It prints, up to the run's last stage, the frontends' roots and their
//! memo groups (Stage 0), Pass 1's `Memo`, Pass 2's
//! `LogicalASAPMemoCandidates` (Stage 2's input: each candidate with its
//! logical edges and $\alpha^{\log}$, then the candidates Pass 2 pruned
//! with their causes), Stage 2's `PhysicalASAPMemoCandidates` (Stage 3's
//! input: each physical candidate with its physical edges and
//! $\alpha^{\mathrm{phys}}$, then the candidates Stage 2 pruned), and
//! Stage 3's plan (each node with its candidate, the plan's cost, each
//! query's accepted accuracy and the candidates Stage 3 pruned). Planning
//! never reads it.
//!
//! Notation: `G<n>` is memo group `n` and `G<n>[<i>]` its candidate `i`;
//! `C<n>` is physical candidate `n` and `n<k>` node `k` of the plan; `<-
//! ...` lists what a line reads; `new` marks a candidate the step added
//! and `pruned` one Pass 2 eliminated; `θ` is a candidate's parameters and
//! `requires` its required accuracy per input. An edge is `✓` when its
//! layer's requirement holds, else `✗` with why; a logical edge's producer
//! is any candidate of the input group (eliminated ones included), a
//! physical edge's a physical candidate whose logical edge holds:
//!
//! ```text
//! G4:  SummaryEstimate Quantile(q=0.99)  <- G3
//!     [0]  Pass1  SummaryEstimate Quantile(q=0.99) <- G3  exact  requires=[Rank ≤ 0.01 (δ 0.01)]
//!         input 0 <- G3:
//!             G3[0]  ✗ α^log = 0: the producer's guarantee exceeds Rank ≤ 0.01 (δ 0.01)
//!             G3[4]  ✓
//!
//!     C6 = G2[0] (QueryTime, None)  Aggregate quantile(q=0.99)
//!         input 0 <- G1:
//!             C3 = G1[0] (QueryTime, None)       ✓  selected
//!             C5 = G1[0] (IngestionTime, None)   ✗ α^phys = 0: consumer (QueryTime, None) cannot read producer (IngestionTime, None)
//! ```

use std::collections::BTreeSet;
use std::fmt::Write;

use asap_accuracy::{producer_satisfies_logical_requirements, AccuracyEvidence};
use asap_cost::PlanCost;
use asap_types::ir::flat::flatten;
use asap_types::ir::physical_export::PhysicalASAPDAG;
use asap_types::ir::QueryRoot;
use asap_types::physical::PhysicalProperties;
use asap_types::stages::{
    CandidateForMemoGroup, CandidateForMemoGroupId, LogicalASAPMemoCandidates, Memo, MemoGroupId,
    PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdge, RequiredAccuracy,
    RootPruneReason,
};

use crate::candidate_labels::{operator_label, parameters_label};
use crate::planning_stages::{PlanningStagesRun, RunUntil};

/// The output of every stage `stages` ran, as text, one section per stage
/// boundary. `evidence` is the accuracy evidence the run planned with
/// (each failing logical edge's reason is computed from it), and
/// `plan_cost` the selected plan's `asap_cost::plan_cost`, printed with
/// the plan when given. A failure ends the text with its error.
pub fn planning_stages_text(
    stages: &PlanningStagesRun,
    evidence: &dyn AccuracyEvidence,
    plan_cost: Option<&PlanCost>,
) -> String {
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
        write_memo(&mut out, memo, None, None);
    }
    if until == RunUntil::Stage0 {
        return out;
    }

    let Some(pass1) = &run.pass1_memo else {
        failure(&mut out);
        return out;
    };
    section(&mut out, "Stage 1 Pass 1 output: Memo");
    write_memo(&mut out, pass1, Some(MemoStep::Pass1), None);
    if until == RunUntil::Pass1 {
        return out;
    }

    let Some(logical) = &run.logical_asap_candidates else {
        failure(&mut out);
        return out;
    };
    section(
        &mut out,
        "Stage 1 Pass 2 output = Stage 2 input: LogicalASAPMemoCandidates",
    );
    write_memo(
        &mut out,
        &logical.memo,
        Some(MemoStep::Pass2 { pass1, logical }),
        Some(evidence),
    );
    write_pass2_pruned(&mut out, logical);
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
    let plan = run.selection.as_ref().ok().map(|(plan, _)| plan);
    write_physical(&mut out, physical, plan);
    if until == RunUntil::Stage2 {
        return out;
    }

    section(&mut out, "Stage 3 output: PhysicalASAPDAG and Selection");
    write_selection(&mut out, stages, plan_cost);
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

/// The step a memo with candidates is printed at: Pass 1, or Pass 2 with
/// the memo at Pass 1 (to mark the candidates Pass 2 added) and its output
/// (its logical edges and pruned candidates).
#[derive(Clone, Copy)]
enum MemoStep<'a> {
    Pass1,
    Pass2 {
        pass1: &'a Memo,
        logical: &'a LogicalASAPMemoCandidates,
    },
}

/// `memo`'s groups, one line each with the child groups it reads, and at a
/// `step` each group's candidates below it: index, origin, `new` (Pass 2
/// added it: the memo only appends, so a candidate is new when its index is
/// past the group's Pass 1 candidates), `pruned` (Pass 2 eliminated it),
/// the operator $e_v$ with its input groups, θ, the fused groups and the
/// required accuracy per input; at Pass 2, under each candidate, the
/// logical edges into each input with $\alpha^{\log}$ (from `evidence`).
fn write_memo(
    out: &mut String,
    memo: &Memo,
    step: Option<MemoStep<'_>>,
    evidence: Option<&dyn AccuracyEvidence>,
) {
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
    let (earlier, logical) = match step {
        Some(MemoStep::Pass2 { pass1, logical }) => (Some(pass1), Some(logical)),
        _ => (None, None),
    };
    let pruned: BTreeSet<CandidateForMemoGroupId> = logical
        .map(|logical| logical.pruned.iter().map(|p| p.candidate).collect())
        .unwrap_or_default();
    for (id, line) in group_lines.lines().enumerate() {
        let _ = writeln!(out, "{line}");
        if step.is_none() {
            continue;
        }
        let group = MemoGroupId(id as u32);
        let first_added = earlier.map_or(usize::MAX, |memo| {
            memo.groups().get(id).map_or(0, |g| g.candidates.len())
        });
        for (index, candidate) in memo.groups()[id].candidates.iter().enumerate() {
            let candidate_id = CandidateForMemoGroupId {
                group,
                index_in_group: index,
            };
            let flag = |set: bool, name: &str| if set { name.to_owned() } else { String::new() };
            write_rows(
                out,
                "    ",
                vec![vec![
                    format!("[{index}]"),
                    format!("{:?}", candidate.origin),
                    flag(index >= first_added, "new"),
                    flag(pruned.contains(&candidate_id), "pruned"),
                    candidate_text(candidate),
                ]],
            );
            if let (Some(logical), Some(evidence)) = (logical, evidence) {
                write_logical_edges(out, &logical.memo, candidate_id, evidence);
            }
        }
    }
    let roots: Vec<String> = memo
        .queries
        .iter()
        .map(|query| match &query.root {
            QueryRoot::Operator(group) => format!("G{}", group.0),
            scalar => format!("{scalar:?}"),
        })
        .collect();
    let _ = writeln!(out, "query_roots: [{}]", roots.join(", "));
}

/// The logical edges into each input of `consumer`: each producer of the
/// input group with `✓`, or `✗` and why ([`logical_edges_into`]).
fn write_logical_edges(
    out: &mut String,
    memo: &Memo,
    consumer: CandidateForMemoGroupId,
    evidence: &dyn AccuracyEvidence,
) {
    let groups = memo.candidate(consumer).expression.operator.children();
    for ((input, group), edges) in groups
        .into_iter()
        .enumerate()
        .zip(logical_edges_into(memo, consumer, evidence))
    {
        let rows = edges
            .into_iter()
            .enumerate()
            .map(|(index_in_group, rejection)| {
                let producer = candidate_id_text(CandidateForMemoGroupId {
                    group: *group,
                    index_in_group,
                });
                match rejection {
                    None => vec![producer, "✓".into()],
                    Some(reason) => vec![producer, format!("✗ {reason}")],
                }
            })
            .collect();
        let _ = writeln!(out, "        input {input} <- G{}:", group.0);
        write_rows(out, "            ", rows);
    }
}

/// Per input $i$ of `consumer` $c$, per candidate $u$ of the input group
/// $G_{c,i}$ (eliminated ones included): why $\alpha^{\log}_{u,c,i} = 0$,
/// `α^log = 0: ` and [`root_prune_reason_text`] of the reason
/// `producer_satisfies_logical_requirements` gives with `evidence` (the
/// evidence Pass 2 read), or `None` when it holds.
pub fn logical_edges_into(
    memo: &Memo,
    consumer: CandidateForMemoGroupId,
    evidence: &dyn AccuracyEvidence,
) -> Vec<Vec<Option<String>>> {
    let groups = memo.candidate(consumer).expression.operator.children();
    groups
        .into_iter()
        .enumerate()
        .map(|(input, group)| {
            (0..memo.groups()[group.0 as usize].candidates.len())
                .map(|index_in_group| {
                    let producer = CandidateForMemoGroupId {
                        group: *group,
                        index_in_group,
                    };
                    producer_satisfies_logical_requirements(
                        memo, producer, consumer, input, evidence,
                    )
                    .err()
                    .map(|reason| format!("α^log = 0: {}", root_prune_reason_text(&reason)))
                })
                .collect()
        })
        .collect()
}

/// `G3[4]`.
fn candidate_id_text(id: CandidateForMemoGroupId) -> String {
    format!("G{}[{}]", id.group.0, id.index_in_group)
}

/// A candidate on one line: `SummaryAgg Kll <- G1  θ=Kll(k=269)`, then
/// `fuses=[..]` and `requires=[..]` when it has them.
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
    let _ =
        write!(
            text,
            "  {}",
            candidate.logical_properties.parameters().map_or(
                "exact".to_owned(),
                |parameters| format!("θ={}", parameters_label(parameters))
            )
        );
    if !candidate.fused_groups.is_empty() {
        let fused: Vec<String> = candidate
            .fused_groups
            .iter()
            .map(|group| format!("G{}", group.0))
            .collect();
        let _ = write!(text, "  fuses=[{}]", fused.join(", "));
    }
    if candidate
        .input_requirements
        .iter()
        .any(|requirement| requirement.required_accuracy.is_some())
    {
        let required: Vec<String> = candidate
            .input_requirements
            .iter()
            .map(|requirement| {
                requirement
                    .required_accuracy
                    .as_ref()
                    .map_or("-".to_owned(), required_accuracy_text)
            })
            .collect();
        let _ = write!(text, "  requires=[{}]", required.join(", "));
    }
    text
}

/// `Rank ≤ 0.01 (δ 0.01)`.
fn required_accuracy_text(required: &RequiredAccuracy) -> String {
    format!(
        "{:?} ≤ {} (δ {})",
        required.metric, required.max_error, required.failure_probability
    )
}

/// Why an edge fails, or a candidate has none, without where:
/// `the producer's guarantee exceeds Rank ≤ 0.01 (δ 0.01)`.
pub fn root_prune_reason_text(reason: &RootPruneReason) -> String {
    match reason {
        RootPruneReason::BelowRequiredAccuracy { required, .. } => format!(
            "the producer's guarantee exceeds {}",
            required_accuracy_text(required)
        ),
        RootPruneReason::MetricRule {
            producer_metric,
            consumer_carries,
            ..
        } => format!(
            "metric rule: the consumer's error contract carries {consumer_carries:?}, not the \
             producer's {producer_metric:?}"
        ),
        RootPruneReason::HeapSmallerThanTopK { k, .. } => {
            format!("the producer's heap is smaller than the top-{k}")
        }
        RootPruneReason::NoConsumer => "no consumer, and not a query output".to_owned(),
    }
}

/// The candidates Pass 2 eliminated, each with its θ and one line per
/// cause: the failing edge (`at G4[0] input 0`) or none, the eliminated
/// consumers the cause comes through, and why.
fn write_pass2_pruned(out: &mut String, logical: &LogicalASAPMemoCandidates) {
    let _ = writeln!(out, "pruned by Pass 2 ({}):", logical.pruned.len());
    for pruned in &logical.pruned {
        let candidate = logical.memo.candidate(pruned.candidate);
        let _ = writeln!(
            out,
            "    {}  {}",
            candidate_id_text(pruned.candidate),
            candidate_text(candidate)
        );
        for cause in &pruned.causes {
            let mut line = String::new();
            if !cause.through_pruned_consumers.is_empty() {
                let through: Vec<String> = cause
                    .through_pruned_consumers
                    .iter()
                    .map(|id| candidate_id_text(*id))
                    .collect();
                let _ = write!(line, "through {}, ", through.join(" → "));
            }
            match &cause.root {
                RootPruneReason::BelowRequiredAccuracy {
                    consumer, input, ..
                }
                | RootPruneReason::MetricRule {
                    consumer, input, ..
                }
                | RootPruneReason::HeapSmallerThanTopK {
                    consumer, input, ..
                } => {
                    let _ = write!(line, "at {} input {input}: ", candidate_id_text(*consumer));
                }
                RootPruneReason::NoConsumer => {}
            }
            line.push_str(&root_prune_reason_text(&cause.root));
            let _ = writeln!(out, "        {line}");
        }
    }
}

/// `(QueryTime, None)`.
fn physical_properties_text(properties: &PhysicalProperties) -> String {
    format!("({:?}, {:?})", properties.timing(), properties.lifetime())
}

/// Why physical edge `edge` fails $\alpha^{\mathrm{phys}}$ (the producer's
/// timing and lifetime cannot serve the consumer's), or `None` when it
/// holds.
pub fn physical_edge_rejection(
    physical: &PhysicalASAPMemoCandidates,
    edge: &PhysicalEdge,
) -> Option<String> {
    (!edge.satisfies_physical_requirement).then(|| {
        let properties = |id: PhysicalCandidateId| {
            physical_properties_text(&physical.physical_candidates[id.0].physical_properties)
        };
        format!(
            "α^phys = 0: consumer {} cannot read producer {}",
            properties(edge.consumer_candidate_id),
            properties(edge.producer_candidate_id)
        )
    })
}

/// The physical edges $(u, c, i)$ of `plan`: for each node, its candidate
/// and each input's producer node's candidate. Empty without a plan.
pub fn selected_edges(
    plan: Option<&PhysicalASAPDAG>,
) -> BTreeSet<(PhysicalCandidateId, PhysicalCandidateId, usize)> {
    let Some(plan) = plan else {
        return BTreeSet::new();
    };
    let candidate_of = |node: usize| plan.nodes[node].selected_physical_candidate;
    plan.nodes
        .iter()
        .filter_map(|node| Some((node, node.selected_physical_candidate?)))
        .flat_map(|(node, consumer)| {
            node.payload
                .children()
                .into_iter()
                .enumerate()
                .filter_map(move |(input, &producer)| {
                    Some((candidate_of(producer)?, consumer, input))
                })
        })
        .collect()
}

/// Stage 2's edges by consumer and input: `edges[c][i]` lists the edges
/// $(u, c, i)$ in Stage 2's order.
pub fn edges_per_input(physical: &PhysicalASAPMemoCandidates) -> Vec<Vec<Vec<PhysicalEdge>>> {
    let memo = &physical.logical_candidates.memo;
    let mut edges: Vec<Vec<Vec<PhysicalEdge>>> = physical
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
        edges[edge.consumer_candidate_id.0][edge.input_index_of_consumer].push(edge);
    }
    edges
}

/// `C3 = G2[0] (QueryTime, None)`.
fn physical_candidate_text(
    physical: &PhysicalASAPMemoCandidates,
    id: PhysicalCandidateId,
) -> String {
    let candidate = &physical.physical_candidates[id.0];
    format!(
        "C{} = {} {}",
        id.0,
        candidate_id_text(candidate.logical_candidate_id),
        physical_properties_text(&candidate.physical_properties)
    )
}

/// Stage 2's output: each physical candidate $v$ with its logical
/// candidate and $p^{\mathrm{phys}}_v$; under it, per input $(c, i)$, the
/// physical edges whose logical edge holds, with $\alpha^{\mathrm{phys}}$
/// (`✓`, or `✗` and why not) and `selected` for an edge of `plan`; then the
/// candidates Stage 2 pruned.
fn write_physical(
    out: &mut String,
    physical: &PhysicalASAPMemoCandidates,
    plan: Option<&PhysicalASAPDAG>,
) {
    let memo = &physical.logical_candidates.memo;
    let selected = selected_edges(plan);
    let _ = writeln!(
        out,
        "physical candidates ({}):",
        physical.physical_candidates.len()
    );
    let edges_into = edges_per_input(physical);
    for (id, candidate) in physical.physical_candidates.iter().enumerate() {
        let operator = &memo
            .candidate(candidate.logical_candidate_id)
            .expression
            .operator;
        let _ = writeln!(
            out,
            "    {}  {}",
            physical_candidate_text(physical, PhysicalCandidateId(id)),
            operator_label(operator)
        );
        for ((input, group), edges) in operator
            .children()
            .into_iter()
            .enumerate()
            .zip(&edges_into[id])
        {
            let rows = edges
                .iter()
                .filter(|edge| edge.satisfies_logical_requirement)
                .map(|edge| {
                    let producer = physical_candidate_text(physical, edge.producer_candidate_id);
                    let is_selected = selected.contains(&(
                        edge.producer_candidate_id,
                        PhysicalCandidateId(id),
                        input,
                    ));
                    match physical_edge_rejection(physical, edge) {
                        None => vec![
                            producer,
                            "✓".to_owned(),
                            if is_selected {
                                "selected".into()
                            } else {
                                String::new()
                            },
                        ],
                        Some(reason) => vec![producer, format!("✗ {reason}")],
                    }
                })
                .collect();
            let _ = writeln!(out, "        input {input} <- G{}:", group.0);
            write_rows(out, "            ", rows);
        }
    }
    let _ = writeln!(
        out,
        "pruned by Stage 2 ({}):",
        physical.pruned_candidates.len()
    );
    let rows = physical
        .pruned_candidates
        .iter()
        .map(|pruned| {
            vec![
                candidate_id_text(pruned.candidate.logical_candidate_id),
                physical_properties_text(&pruned.candidate.physical_properties),
                format!("{:?}", pruned.reason),
            ]
        })
        .collect();
    write_rows(out, "    ", rows);
}

/// Stage 3's plan, each node with its physical candidate, operator and
/// input nodes; the plan's cost (`plan_cost`) with each node's charge;
/// each query's accepted accuracy; the selection MILP's size; and the
/// candidates Stage 3 pruned. Or why no plan was selected.
fn write_selection(out: &mut String, stages: &PlanningStagesRun, plan_cost: Option<&PlanCost>) {
    let run = &stages.run;
    let (plan, selection) = match &run.selection {
        Ok(selected) => selected,
        Err(error) => {
            let _ = writeln!(out, "error: {error}");
            return;
        }
    };
    let _ = writeln!(
        out,
        "selection MILP: {} variables, {} constraints, {} ms building, {} ms solving",
        selection.milp_variables,
        selection.milp_constraints,
        selection.milp_construction_ms,
        selection.milp_solve_ms
    );
    let physical = run.physical_asap_candidates.as_ref();
    let rows = plan
        .nodes
        .iter()
        .map(|node| {
            let inputs: Vec<String> = node
                .payload
                .children()
                .into_iter()
                .map(|input| format!("n{input}"))
                .collect();
            let candidate = match (physical, node.selected_physical_candidate) {
                (Some(physical), Some(id)) => physical_candidate_text(physical, id),
                _ => format!("({:?}, {:?})", node.output_state.timing, node.lifetime),
            };
            let parameters = node
                .parameters
                .as_ref()
                .map_or(String::new(), |parameters| {
                    format!("θ={}", parameters_label(parameters))
                });
            vec![
                format!("n{}:", node.id),
                candidate,
                operator_label(&node.payload),
                parameters,
                reads(&inputs),
            ]
        })
        .collect();
    write_rows(out, "", rows);
    let roots: Vec<String> = plan.roots.iter().map(|node| format!("n{node}")).collect();
    let _ = writeln!(out, "roots: [{}]", roots.join(", "));
    if let Some(cost) = plan_cost {
        let _ = writeln!(
            out,
            "cost_per_second: {}",
            cost.cost_per_second
                .map_or("none".to_owned(), |cost| format!("{cost:.6}"))
        );
        let rows = cost
            .per_node
            .iter()
            .map(|(node, charge)| {
                vec![
                    format!("n{node}"),
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
    }
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
                candidate_id_text(pruned.candidate.logical_candidate_id),
                physical_properties_text(&pruned.candidate.physical_properties),
                format!("{:?}", pruned.reason),
            ]
        })
        .collect();
    write_rows(out, "    ", rows);
}
