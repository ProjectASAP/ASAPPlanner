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
//! physical edge's a physical candidate whose logical edge holds. With
//! `--verbose`:
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

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use asap_accuracy::{accepted_metrics, producer_satisfies_logical_requirements, AccuracyEvidence};
use asap_cost::PlanCost;
use asap_types::ir::flat::flatten;
use asap_types::ir::physical_export::PhysicalASAPDAG;
use asap_types::ir::properties::AccuracyMetric;
use asap_types::ir::schema::SketchParams;
use asap_types::ir::QueryRoot;
use asap_types::physical::PhysicalProperties;
use asap_types::stages::{
    CandidateForMemoGroup, CandidateForMemoGroupId, ColumnLineage, EvaluationGrid,
    LogicalASAPMemoCandidates, LogicalEdge, Memo, MemoGroup, MemoGroupId,
    PhysicalASAPMemoCandidates, PhysicalCandidate, PhysicalCandidateId, PhysicalEdge,
    PrunedCandidate, RequiredAccuracy, RootPruneReason,
};

use crate::candidate_labels::{operator_label, parameters_label};
use crate::planning_stages::{PlanningStagesRun, RunUntil};

/// The output of every stage `stages` ran, as text, one section per stage
/// boundary. `evidence` is the accuracy evidence the run planned with
/// (each failing logical edge's reason is computed from it), and
/// `plan_cost` the selected plan's `asap_cost::plan_cost`, printed with
/// the plan when given. A failure ends the text with its error.
///
/// Each section starts with a legend of the labels it prints. Unless
/// `verbose`: the candidates of a memo group that differ only in θ are
/// folded ([`write_memo`]); groups omit their properties line; Pass 2's
/// edges are the stored ones as producer ids by $\alpha^{\log}$, without
/// reasons; Stage 2 is printed per logical candidate
/// ([`write_physical_by_logical_candidate`]); and the pruned lists are
/// counts per cause.
pub fn planning_stages_text(
    stages: &PlanningStagesRun,
    evidence: &dyn AccuracyEvidence,
    plan_cost: Option<&PlanCost>,
    verbose: bool,
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
        &["n<k> = node k", "<- = the nodes it reads"],
    );
    write_dag(&mut out, &stages.stage0_query_language_frontends);
    section(
        &mut out,
        "Stage 0 → Stage 1: Memo (the roots hash-consed, without candidates)",
        MEMO_LEGEND,
    );
    if let Some(memo) = &stages.stage0_memo_groups {
        write_memo(&mut out, memo, None, None, verbose);
    }
    if until == RunUntil::Stage0 {
        return out;
    }

    let Some(pass1) = &run.pass1_memo else {
        failure(&mut out);
        return out;
    };
    section(
        &mut out,
        "Stage 1 Pass 1 output: Memo",
        &[MEMO_LEGEND, CANDIDATE_LEGEND].concat(),
    );
    write_memo(&mut out, pass1, Some(MemoStep::Pass1), None, verbose);
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
        &[
            MEMO_LEGEND,
            CANDIDATE_LEGEND,
            PASS2_LEGEND,
            if verbose {
                PASS2_VERBOSE_EDGES_LEGEND
            } else {
                PASS2_EDGES_LEGEND
            },
        ]
        .concat(),
    );
    write_memo(
        &mut out,
        &logical.memo,
        Some(MemoStep::Pass2 { pass1, logical }),
        Some(evidence),
        verbose,
    );
    write_pass2_pruned(&mut out, logical, verbose);
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
        if verbose {
            STAGE2_VERBOSE_LEGEND
        } else {
            STAGE2_LEGEND
        },
    );
    let plan = run.selection.as_ref().ok().map(|(plan, _)| plan);
    write_physical(&mut out, physical, plan, verbose);
    if until == RunUntil::Stage2 {
        return out;
    }

    section(
        &mut out,
        "Stage 3 output: PhysicalASAPDAG and Selection",
        &[
            "n<k> = plan node k",
            "C<n> = the physical candidate it runs",
            "<- = the nodes it reads",
        ],
    );
    write_selection(&mut out, stages, plan_cost, verbose);
    out
}

const MEMO_LEGEND: &[&str] = &[
    "G<n> = memo group n",
    "<- = the groups it reads",
    "used by = the groups that read it",
    "reached by = the queries whose roots reach it",
    "every <p> ms = the grid its result is evaluated on (from Pass 2)",
];
const CANDIDATE_LEGEND: &[&str] = &[
    "[i] = candidate i, then the rule that added it",
    "exact = no sketch",
    "θ = sketch parameters",
    "[a..b] = candidates that differ only in θ, folded (--verbose lists each)",
];
const PASS2_LEGEND: &[&str] = &[
    "new = added by Pass 2",
    "pruned = eliminated by Pass 2",
    "requires = the accuracy required per input",
];
const PASS2_EDGES_LEGEND: &[&str] = &[
    "input i <- G<k>: ✓ / ✗ = the edges Pass 2 kept, from producers in G<k> that do / do not meet the input's requirement",
    "N other(s) = candidates of G<k> with no edge: pruned, or of another θ (--verbose lists each, with why)",
    "a pruned candidate lists no inputs: Pass 2 keeps no edges into it",
];
const PASS2_VERBOSE_EDGES_LEGEND: &[&str] = &[
    "input i <- G<k>: one row per candidate of G<k>, pruned ones included:",
    "    ✓ = meets the input's requirement · ✗ = does not, and why · no edge = another θ",
];
const STAGE2_LEGEND: &[&str] = &[
    "G<g>[<i>] = a logical candidate",
    "C<n> = one of its physical candidates",
    "(timing, lifetime) = when C runs, how long its result is kept",
    "reads = the producers C can read on that input (--verbose: also those it cannot, and why)",
    "★ = in Stage 3's plan",
    "uses = the producer the plan reads",
];
const STAGE2_VERBOSE_LEGEND: &[&str] = &[
    "C<n> = G<g>[<i>] (timing, lifetime) = physical candidate n: a logical candidate, when it runs and how long its result is kept",
    "input i <- G<k>: one row per producer whose logical edge holds:",
    "    ✓ = the consumer can read it · ✗ = it cannot, and why",
    "selected = the edge Stage 3's plan uses",
];

/// A section header, then `legend` (the labels the section prints).
fn section(out: &mut String, title: &str, legend: &[&str]) {
    let _ = writeln!(out, "\n==================== {title} ====================");
    for (index, line) in legend.iter().enumerate() {
        let _ = writeln!(
            out,
            "{}{line}",
            if index == 0 { "legend: " } else { "        " }
        );
    }
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

/// Each output column's lineage (`source.column`, or `computed`), one per
/// schema field.
pub fn column_lineage_text(group: &MemoGroup) -> Vec<String> {
    group
        .group_properties
        .column_lineage
        .iter()
        .map(|lineage| match lineage {
            ColumnLineage::SourceColumn(column) => format!("{}.{}", column.source, column.column),
            ColumnLineage::Computed => "computed".to_owned(),
        })
        .collect()
}

/// A group's properties on one line: each column's name, lineage and sign,
/// whether it reads only time series, and (from Pass 2 on, `grid`) the grid
/// a result of it is evaluated on:
/// `ts=latency.ts:Unknown value=latency.value:NonNegative · reads only time
/// series · evaluated every 60000 ms at 0`.
fn group_properties_text(group: &MemoGroup, grid: Option<Option<&EvaluationGrid>>) -> String {
    let properties = &group.group_properties;
    let columns: Vec<String> = properties
        .schema
        .fields
        .iter()
        .zip(column_lineage_text(group))
        .zip(&properties.column_sign)
        .map(|((field, lineage), sign)| format!("{}={lineage}:{sign:?}", field.name))
        .collect();
    let mut text = columns.join(" ");
    text += match properties.reads_only_time_series {
        true => " · reads only time series",
        false => " · reads a table",
    };
    if let Some(grid) = grid {
        let _ = write!(text, " · {}", grid_text(grid));
    }
    text
}

/// `every 60000 ms`, `every 60000 ms at 15000`, or `no known evaluation grid`.
fn grid_text(grid: Option<&EvaluationGrid>) -> String {
    match grid {
        Some(grid) if grid.phase_ms == 0 => format!("every {} ms", grid.period_ms),
        Some(grid) => format!("every {} ms at {}", grid.period_ms, grid.phase_ms),
        None => "no known evaluation grid".to_owned(),
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
///
/// Unless `verbose`, candidates that differ only in θ (same origin, `new`,
/// operator, fused groups and requirements) are folded when there are
/// several: at Pass 1 into one line, at Pass 2 when Pass 2 pruned some of
/// them, into one line followed by those it kept, in full. Unless
/// `verbose`, each kept candidate's inputs are the edges Pass 2 stored
/// ([`write_stored_logical_edges`]), and a pruned one lists none:
///
/// ```text
/// G9:  SummaryAgg CmsWithHeap  <- G1
///     [0..279]  Pass1  SummaryAgg CmsWithHeap <- G1  fuses=[G4]
///         θ: depth ∈ {5, 7, 10, 12}, heap_size ∈ {10..10000} (7 values), width ∈ {28..2719} (10 values)
///         280 candidates: 278 pruned (BelowRequiredAccuracy 270, MetricRule 8), 2 kept
///     [57]  Pass1  SummaryAgg CmsWithHeap <- G1  θ=CmsWithHeap(depth=7, heap_size=10, width=544)  fuses=[G4]
///         input 0 <- G1: ✓ G1[0]
/// ```
fn write_memo(
    out: &mut String,
    memo: &Memo,
    step: Option<MemoStep<'_>>,
    evidence: Option<&dyn AccuracyEvidence>,
    verbose: bool,
) {
    // The group lines share one set of column widths, with each group's
    // candidates printed under its line.
    let (earlier, logical) = match step {
        Some(MemoStep::Pass2 { pass1, logical }) => (Some(pass1), Some(logical)),
        _ => (None, None),
    };
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
            let mut context = Vec::new();
            if !group.parent_groups().is_empty() {
                let parents: Vec<String> = group
                    .parent_groups()
                    .iter()
                    .map(|g| format!("G{}", g.0))
                    .collect();
                context.push(format!("used by {}", parents.join(", ")));
            }
            if !group.reached_by_queries().is_empty() {
                let queries: Vec<String> = group
                    .reached_by_queries()
                    .iter()
                    .map(|query| format!("q{}", query + 1))
                    .collect();
                context.push(format!("reached by {}", queries.join(", ")));
            }
            // Without `verbose` the properties line is not printed, but the
            // grid is: Pass 2 derives it, so it is new at that step.
            if let (false, Some(logical)) = (verbose, logical) {
                context.push(grid_text(logical.evaluation_grid_of_group[id].as_ref()));
            }
            vec![
                format!("G{id}:"),
                operator_label(&group.expression),
                reads(&children),
                context.join(" · "),
            ]
        })
        .collect();
    let mut group_lines = String::new();
    write_rows(&mut group_lines, "", group_rows);
    // Only `verbose` recomputes the logical edges, from the metric rule's
    // accepted metrics; otherwise the stored edges are printed, by
    // (consumer, input).
    let accepted = logical
        .filter(|_| verbose)
        .map(|logical| accepted_metrics(&logical.memo));
    let mut stored_edges: BTreeMap<(CandidateForMemoGroupId, usize), Vec<&LogicalEdge>> =
        BTreeMap::new();
    for edge in logical
        .filter(|_| !verbose)
        .iter()
        .flat_map(|logical| &logical.edges)
    {
        stored_edges
            .entry((edge.consumer_candidate_id, edge.input_index_of_consumer))
            .or_default()
            .push(edge);
    }
    let pruned: BTreeMap<CandidateForMemoGroupId, &'static str> = logical
        .map(|logical| {
            logical
                .pruned
                .iter()
                .map(|p| {
                    let cause = p
                        .causes
                        .first()
                        .map_or("no cause", |cause| root_prune_reason_name(&cause.root));
                    (p.candidate, cause)
                })
                .collect()
        })
        .unwrap_or_default();
    for (id, line) in group_lines.lines().enumerate() {
        let _ = writeln!(out, "{line}");
        if verbose {
            let grid = logical.map(|logical| logical.evaluation_grid_of_group[id].as_ref());
            let _ = writeln!(
                out,
                "    {}",
                group_properties_text(&memo.groups()[id], grid)
            );
        }
        if step.is_none() {
            continue;
        }
        let group = MemoGroupId(id as u32);
        let first_added = earlier.map_or(usize::MAX, |memo| {
            memo.groups().get(id).map_or(0, |g| g.candidates.len())
        });
        let candidates = &memo.groups()[id].candidates;
        let id_of = |index_in_group| CandidateForMemoGroupId {
            group,
            index_in_group,
        };
        let flag = |set: bool, name: &str| if set { name.to_owned() } else { String::new() };
        let row = |index: String, candidate: &CandidateForMemoGroup, new: bool, text: String| {
            vec![
                index,
                format!("{:?}", candidate.origin),
                flag(new, "new"),
                text,
            ]
        };
        // The candidates printed in full, in index order; unless `verbose`,
        // each class of candidates differing only in θ that is folded
        // prints its line where its first candidate is.
        let mut classes: Vec<(String, Vec<usize>)> = Vec::new();
        for (index, candidate) in candidates.iter().enumerate() {
            let key = format!(
                "{:?} {} {}",
                candidate.origin,
                index >= first_added,
                candidate_text(candidate, false)
            );
            match classes.iter_mut().find(|(known, _)| *known == key) {
                Some((_, members)) => members.push(index),
                None => classes.push((key, vec![index])),
            }
        }
        let mut printed = Vec::new();
        for (_, members) in &classes {
            let pruned_members = members
                .iter()
                .filter(|&&index| pruned.contains_key(&id_of(index)))
                .count();
            let folded = !verbose && members.len() > 1 && (logical.is_none() || pruned_members > 0);
            if !folded {
                printed.extend(members.iter().map(|&index| (index, None)));
                continue;
            }
            printed.push((members[0], Some(members)));
            printed.extend(
                members
                    .iter()
                    .filter(|&&index| logical.is_some() && !pruned.contains_key(&id_of(index)))
                    .map(|&index| (index, None)),
            );
        }
        printed.sort_by_key(|&(index, fold)| (index, fold.is_none()));
        for (index, fold) in printed {
            let candidate = &candidates[index];
            let candidate_id = id_of(index);
            if let Some(members) = fold {
                write_rows(
                    out,
                    "    ",
                    vec![row(
                        format!("[{}]", index_ranges(members)),
                        candidate,
                        index >= first_added,
                        candidate_text(candidate, false),
                    )],
                );
                let parameters: Vec<&SketchParams> = members
                    .iter()
                    .filter_map(|&index| candidates[index].logical_properties.parameters())
                    .collect();
                if !parameters.is_empty() {
                    let _ = writeln!(out, "        θ: {}", parameter_values_text(&parameters));
                }
                let mut counts = format!("{} candidates", members.len());
                if logical.is_some() {
                    let causes: Vec<&str> = members
                        .iter()
                        .filter_map(|&index| pruned.get(&id_of(index)).copied())
                        .collect();
                    let _ = write!(
                        counts,
                        ": {} pruned ({}), {} kept",
                        causes.len(),
                        counts_text(causes.iter().copied()),
                        members.len() - causes.len()
                    );
                }
                let _ = writeln!(out, "        {counts}");
                continue;
            }
            let mut cells = row(
                format!("[{index}]"),
                candidate,
                index >= first_added,
                candidate_text(candidate, true),
            );
            cells.insert(3, flag(pruned.contains_key(&candidate_id), "pruned"));
            write_rows(out, "    ", vec![cells]);
            match (logical, &accepted, evidence) {
                (Some(logical), Some(accepted), Some(evidence)) => {
                    write_logical_edges(out, &logical.memo, accepted, candidate_id, evidence)
                }
                // Pass 2 keeps no edges into a pruned candidate.
                (Some(logical), ..) if !pruned.contains_key(&candidate_id) => {
                    write_stored_logical_edges(out, &logical.memo, &stored_edges, candidate_id)
                }
                _ => {}
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

/// The logical edges Pass 2 stored into each input of `consumer`
/// (`stored_edges`, from `LogicalASAPMemoCandidates::edges` by (consumer,
/// input): between surviving candidates only), as producer indices by
/// $\alpha^{\log}$, then how many candidates of the input group have no
/// edge: `input 0 <- G7: ✓ G7[0..11] ✗ G7[12] · 270 others`.
fn write_stored_logical_edges(
    out: &mut String,
    memo: &Memo,
    stored_edges: &BTreeMap<(CandidateForMemoGroupId, usize), Vec<&LogicalEdge>>,
    consumer: CandidateForMemoGroupId,
) {
    let groups = memo.candidate(consumer).expression.operator.children();
    for (input, group) in groups.into_iter().enumerate() {
        let (mut satisfied, mut failed) = (Vec::new(), Vec::new());
        for edge in stored_edges.get(&(consumer, input)).into_iter().flatten() {
            let index = edge.producer_candidate_id.index_in_group;
            match edge.satisfies_logical_requirement {
                true => satisfied.push(index),
                false => failed.push(index),
            }
        }
        let others =
            memo.groups()[group.0 as usize].candidates.len() - satisfied.len() - failed.len();
        let mut line = format!("        input {input} <- G{}:", group.0);
        for (mark, indices) in [("✓", satisfied), ("✗", failed)] {
            if !indices.is_empty() {
                let _ = write!(line, " {mark} G{}[{}]", group.0, index_ranges(&indices));
            }
        }
        if others > 0 {
            let plural = if others == 1 { "" } else { "s" };
            let _ = write!(line, " · {others} other{plural}");
        }
        let _ = writeln!(out, "{line}");
    }
}

/// The logical edges into each input of `consumer`, recomputed
/// (`verbose`): each candidate of the input group with `✓`, or `✗` and why
/// ([`logical_edges_into`]).
fn write_logical_edges(
    out: &mut String,
    memo: &Memo,
    accepted: &[Option<Vec<AccuracyMetric>>],
    consumer: CandidateForMemoGroupId,
    evidence: &dyn AccuracyEvidence,
) {
    let groups = memo.candidate(consumer).expression.operator.children();
    for ((input, group), edges) in groups
        .into_iter()
        .enumerate()
        .zip(logical_edges_into(memo, accepted, consumer, evidence))
    {
        let rows: Vec<Vec<String>> = edges
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
/// `no edge` when it cannot read the producer at all (`Memo::producers`: a
/// different θ), else `α^log = 0: ` and [`root_prune_reason_text`] of the reason
/// `producer_satisfies_logical_requirements` gives with `accepted`
/// (`accepted_metrics` of `memo`) and `evidence` (the evidence Pass 2
/// read), or `None` when it holds.
pub fn logical_edges_into(
    memo: &Memo,
    accepted: &[Option<Vec<AccuracyMetric>>],
    consumer: CandidateForMemoGroupId,
    evidence: &dyn AccuracyEvidence,
) -> Vec<Vec<Option<String>>> {
    let groups = memo.candidate(consumer).expression.operator.children();
    groups
        .into_iter()
        .enumerate()
        .map(|(input, group)| {
            let readable = memo.producers(consumer, input);
            (0..memo.groups()[group.0 as usize].candidates.len())
                .map(|index_in_group| {
                    let producer = CandidateForMemoGroupId {
                        group: *group,
                        index_in_group,
                    };
                    if !readable.contains(&producer) {
                        return Some(NO_EDGE_OTHER_PARAMETERS.to_owned());
                    }
                    producer_satisfies_logical_requirements(
                        memo, accepted, producer, consumer, input, evidence,
                    )
                    .err()
                    .map(|reason| format!("α^log = 0: {}", root_prune_reason_text(&reason)))
                })
                .collect()
        })
        .collect()
}

/// A logical edge's rejection when the consumer cannot read the producer
/// (`Memo::producers`: another θ).
const NO_EDGE_OTHER_PARAMETERS: &str = "no edge: the producer's θ differs";

/// `0..279`, or `0..3, 7, 9..12`: sorted indices as ranges.
fn index_ranges(indices: &[usize]) -> String {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &index in indices {
        match ranges.last_mut() {
            Some((_, end)) if *end + 1 == index => *end = index,
            _ => ranges.push((index, index)),
        }
    }
    ranges
        .iter()
        .map(|&(start, end)| match start == end {
            true => start.to_string(),
            false => format!("{start}..{end}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Each θ parameter's values among `parameters`, sorted:
/// `depth ∈ {5, 7}, width ∈ {28..2719} (10 values)` (a range past four
/// values).
fn parameter_values_text(parameters: &[&SketchParams]) -> String {
    let mut values: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
    for parameters in parameters {
        let value = serde_json::to_value(parameters).unwrap_or_default();
        let fields = value
            .as_object()
            .and_then(|algorithm| algorithm.values().next())
            .and_then(|fields| fields.as_object());
        for (name, value) in fields.into_iter().flatten() {
            let known = values.entry(name.clone()).or_default();
            if !known.contains(value) {
                known.push(value.clone());
            }
        }
    }
    values
        .into_iter()
        .map(|(name, mut known)| {
            known.sort_by(|a, b| match (a.as_f64(), b.as_f64()) {
                (Some(a), Some(b)) => a.total_cmp(&b),
                _ => a.to_string().cmp(&b.to_string()),
            });
            let set = match known.len() {
                0..=4 => format!(
                    "{{{}}}",
                    known
                        .iter()
                        .map(|value| value.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                n => format!("{{{}..{}}} ({n} values)", known[0], known[n - 1]),
            };
            format!("{name} ∈ {set}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `BelowRequiredAccuracy 270, MetricRule 8`: each name with how often it
/// occurs, most frequent first.
fn counts_text<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for name in names {
        *counts.entry(name).or_default() += 1;
    }
    let mut counts: Vec<(&str, usize)> = counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    counts
        .iter()
        .map(|(name, count)| format!("{name} {count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A root cause's variant name: `BelowRequiredAccuracy`.
fn root_prune_reason_name(reason: &RootPruneReason) -> &'static str {
    match reason {
        RootPruneReason::BelowRequiredAccuracy { .. } => "BelowRequiredAccuracy",
        RootPruneReason::MetricRule { .. } => "MetricRule",
        RootPruneReason::NoGuarantee { .. } => "NoGuarantee",
        RootPruneReason::ValuesPerEvaluationUnknown { .. } => "ValuesPerEvaluationUnknown",
        RootPruneReason::HeapSmallerThanTopK { .. } => "HeapSmallerThanTopK",
        RootPruneReason::NoConsumer => "NoConsumer",
    }
}

/// `G3[4]`.
fn candidate_id_text(id: CandidateForMemoGroupId) -> String {
    format!("G{}[{}]", id.group.0, id.index_in_group)
}

/// A candidate on one line: `SummaryAgg Kll <- G1  θ=Kll(k=269)` (θ only
/// with `parameters`), then `fuses=[..]` and `requires=[..]` when it has
/// them.
fn candidate_text(candidate: &CandidateForMemoGroup, parameters: bool) -> String {
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
    match candidate.logical_properties.parameters() {
        None => text += "  exact",
        Some(theta) if parameters => {
            let _ = write!(text, "  θ={}", parameters_label(theta));
        }
        Some(_) => {}
    }
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
            accepted_metrics,
            ..
        } => format!(
            "metric rule: the consumer accepts {accepted_metrics:?} on this input, not the \
             producer's {producer_metric:?}"
        ),
        RootPruneReason::NoGuarantee { .. } => {
            "the producer's summary type has no guarantee for the readout's statistic".to_owned()
        }
        RootPruneReason::ValuesPerEvaluationUnknown { query, .. } => format!(
            "query {query}'s failure probability covers its whole result, and the number of \
             values one evaluation returns is unknown"
        ),
        RootPruneReason::HeapSmallerThanTopK { k, .. } => {
            format!("the producer's heap is smaller than the top-{k}")
        }
        RootPruneReason::NoConsumer => "no consumer, and not a query output".to_owned(),
    }
}

/// The candidates Pass 2 eliminated, each with its θ and one line per
/// cause: the failing edge (`at G4[0] input 0`) or none, the eliminated
/// consumers the cause comes through, and why. Unless `verbose`, per group
/// the count of its pruned candidates by their first cause.
fn write_pass2_pruned(out: &mut String, logical: &LogicalASAPMemoCandidates, verbose: bool) {
    let _ = writeln!(out, "pruned by Pass 2 ({}):", logical.pruned.len());
    if !verbose {
        let mut causes: BTreeMap<MemoGroupId, Vec<&str>> = BTreeMap::new();
        for pruned in &logical.pruned {
            causes.entry(pruned.candidate.group).or_default().push(
                pruned
                    .causes
                    .first()
                    .map_or("no cause", |cause| root_prune_reason_name(&cause.root)),
            );
        }
        for (group, causes) in causes {
            let _ = writeln!(
                out,
                "    G{}: {} ({})",
                group.0,
                causes.len(),
                counts_text(causes)
            );
        }
        return;
    }
    for pruned in &logical.pruned {
        let candidate = logical.memo.candidate(pruned.candidate);
        let _ = writeln!(
            out,
            "    {}  {}",
            candidate_id_text(pruned.candidate),
            candidate_text(candidate, true)
        );
        for cause in &pruned.causes {
            let mut line = String::new();
            if !cause.through_pruned_candidates.is_empty() {
                let through: Vec<String> = cause
                    .through_pruned_candidates
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
                }
                | RootPruneReason::NoGuarantee { consumer, input } => {
                    let _ = write!(line, "at {} input {input}: ", candidate_id_text(*consumer));
                }
                RootPruneReason::ValuesPerEvaluationUnknown { consumer, .. } => {
                    let _ = write!(line, "at {}: ", candidate_id_text(*consumer));
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
/// candidates Stage 2 pruned (unless `verbose`, their count per reason).
fn write_physical(
    out: &mut String,
    physical: &PhysicalASAPMemoCandidates,
    plan: Option<&PhysicalASAPDAG>,
    verbose: bool,
) {
    let selected = selected_edges(plan);
    let _ = writeln!(
        out,
        "physical candidates ({}):",
        physical.physical_candidates.len()
    );
    if verbose {
        write_physical_edges(out, physical, &selected);
    } else {
        write_physical_by_logical_candidate(out, physical, plan, &selected);
    }
    let _ = writeln!(
        out,
        "pruned by Stage 2 ({}):",
        physical.pruned_candidates.len()
    );
    write_physical_pruned(out, &physical.pruned_candidates, verbose);
}

/// Each physical candidate with, per input, every physical edge whose
/// logical edge holds: `✓`, or `✗` and why, and `selected` for an edge of
/// the plan (`selected`).
fn write_physical_edges(
    out: &mut String,
    physical: &PhysicalASAPMemoCandidates,
    selected: &BTreeSet<(PhysicalCandidateId, PhysicalCandidateId, usize)>,
) {
    let memo = &physical.logical_candidates.memo;
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
}

/// Physical candidates Stage 2 or Stage 3 pruned: their count per reason,
/// or with `verbose` each with its logical candidate, properties and reason.
fn write_physical_pruned(
    out: &mut String,
    pruned: &[PrunedCandidate<PhysicalCandidate>],
    verbose: bool,
) {
    if !verbose {
        let reasons: Vec<String> = pruned
            .iter()
            .map(|pruned| {
                let reason = format!("{:?}", pruned.reason);
                reason
                    .split(['(', ' ', '{'])
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        if !reasons.is_empty() {
            let _ = writeln!(
                out,
                "    {}",
                counts_text(reasons.iter().map(String::as_str))
            );
        }
        return;
    }
    let rows = pruned
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

/// Stage 2's output without `verbose`: each logical candidate once, its
/// physical candidates under it (`★` when in `plan`), and per input the
/// producers it can read ($\alpha^{\log} = \alpha^{\mathrm{phys}} = 1$) and
/// the one the plan reads:
///
/// ```text
/// G1[0]  TimeRange 300s <- G0  exact
///     C4  (QueryTime, None)  ★
///         input 0 <- G0: reads C0..C1, C3 · uses C0
/// ```
fn write_physical_by_logical_candidate(
    out: &mut String,
    physical: &PhysicalASAPMemoCandidates,
    plan: Option<&PhysicalASAPDAG>,
    selected: &BTreeSet<(PhysicalCandidateId, PhysicalCandidateId, usize)>,
) {
    let memo = &physical.logical_candidates.memo;
    let in_plan: BTreeSet<PhysicalCandidateId> = plan
        .into_iter()
        .flat_map(|plan| &plan.nodes)
        .filter_map(|node| node.selected_physical_candidate)
        .collect();
    // Grouped explicitly: Stage 2 lists a logical candidate's physical
    // candidates together, but `PhysicalASAPMemoCandidates` does not require it.
    let mut by_logical: BTreeMap<CandidateForMemoGroupId, Vec<usize>> = BTreeMap::new();
    for (index, candidate) in physical.physical_candidates.iter().enumerate() {
        by_logical
            .entry(candidate.logical_candidate_id)
            .or_default()
            .push(index);
    }
    for (logical_id, indices) in by_logical {
        let logical = memo.candidate(logical_id);
        let _ = writeln!(
            out,
            "{}  {}",
            candidate_id_text(logical_id),
            candidate_text(logical, true)
        );
        for index in indices {
            let candidate = &physical.physical_candidates[index];
            let id = PhysicalCandidateId(index);
            let star = if in_plan.contains(&id) { "  ★" } else { "" };
            let _ = writeln!(
                out,
                "    C{index}  {}{star}",
                physical_properties_text(&candidate.physical_properties)
            );
            for (input, group) in logical
                .expression
                .operator
                .children()
                .into_iter()
                .enumerate()
            {
                let producers: Vec<usize> = physical
                    .admissible_edges_into(id, input)
                    .iter()
                    .map(|&edge| physical.edge(edge).producer_candidate_id.0)
                    .collect();
                let mut line = match producers.is_empty() {
                    true => format!("        input {input} <- G{}: reads nothing", group.0),
                    false => format!(
                        "        input {input} <- G{}: reads {}",
                        group.0,
                        physical_ranges(&producers)
                    ),
                };
                if let Some(&(producer, ..)) =
                    selected.iter().find(|&&(_, consumer, selected_input)| {
                        consumer == id && selected_input == input
                    })
                {
                    let _ = write!(line, " · uses C{}", producer.0);
                }
                let _ = writeln!(out, "{line}");
            }
        }
    }
}

/// `C0..C1, C3`: sorted physical candidate ids as ranges.
fn physical_ranges(ids: &[usize]) -> String {
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    index_ranges(&ids)
        .split(", ")
        .map(|range| match range.split_once("..") {
            Some((start, end)) => format!("C{start}..C{end}"),
            None => format!("C{range}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Stage 3's plan, each node with its physical candidate, operator and
/// input nodes; the plan's cost (`plan_cost`) with each node's charge;
/// each query's accepted accuracy; the selection MILP's size; and the
/// candidates Stage 3 pruned. Or why no plan was selected.
fn write_selection(
    out: &mut String,
    stages: &PlanningStagesRun,
    plan_cost: Option<&PlanCost>,
    verbose: bool,
) {
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
        "selection MILP: {} variables, {} constraints, {} ms building, {} ms solving, \
         objective {}/s",
        selection.milp_variables,
        selection.milp_constraints,
        selection.milp_construction_ms,
        selection.milp_solve_ms,
        selection.milp_objective
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
    write_physical_pruned(out, &selection.pruned_candidates, verbose);
}
