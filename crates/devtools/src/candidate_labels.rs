//! Display labels for the planning stages' candidates, for the
//! `stage_pipeline` viewer document only. Planning never reads them: they
//! are pure functions of the candidates' planning data (the Pass 1 choice,
//! the Pass 2 rules applied and the materialization assignment).

use std::collections::BTreeMap;
use std::rc::Rc;

use asap_aware_mapping::asap_aware_cse::PaneCandidate;
use asap_aware_mapping::pass::Pass2Candidate;
use asap_aware_mapping::per_computation_candidates::{
    ComputationCandidate, PerComputationCandidates,
};
use asap_aware_mapping::stage2_physical_asap_optimization::PhysicalCandidate;
use asap_types::ir::NonASAPOp;
use asap_types::ir::{ASAPOp, Operator, OperatorNode, QueryRoot};
use asap_types::post_asap::{FieldDataType, Materialization, SketchAlgorithm};

use crate::planning_stages::PlanningStagesRun;

/// The label of every logical (`L<n>`) and physical (`P<n>`, `P<n>-m<k>`)
/// candidate of `run`, by id.
pub fn candidate_labels(run: &PlanningStagesRun) -> BTreeMap<String, String> {
    let logical = &run.stage1_logical_asap_optimization;
    let mut labels = BTreeMap::new();
    for candidate in logical {
        let mut label = choice_label(&run.stage1_pass1_per_computation, &candidate.choice);
        if let Some(pass2) = &candidate.pass2 {
            let pass1_roots = logical[pass2.from_pass1_position]
                .roots
                .as_deref()
                .unwrap_or_default();
            let candidate_roots = candidate.roots.as_deref().unwrap_or_default();
            label += &pass2_label(pass2, pass1_roots, candidate_roots);
        }
        labels.insert(candidate.id.clone(), label);
    }
    for candidate in &run.stage2_physical_asap_optimization {
        let logical = labels
            .get(&candidate.from_logical)
            .cloned()
            .unwrap_or_default();
        labels.insert(candidate.id.clone(), physical_label(&logical, candidate));
    }
    labels
}

/// A Pass 1 choice, e.g. "Q1 exact · Q2 CMS+heap" (exact accumulators in
/// parentheses), each target listed under the first query that reads it.
pub fn choice_label<Id>(inventory: &PerComputationCandidates<Id>, choice: &[usize]) -> String {
    let query_of_target: Vec<usize> = inventory
        .targets
        .iter()
        .map(|target| {
            inventory
                .roots
                .iter()
                .position(|(_, root)| {
                    root_operators(root).into_iter().any(|node| {
                        OperatorNode::reachable(node)
                            .iter()
                            .any(|n| Rc::ptr_eq(n, &target.aggregate))
                    })
                })
                .expect("every target is reached from a root")
        })
        .collect();
    // Each target's chosen candidate, with the target that computes it: the
    // target itself or its fused target.
    let chosen: Vec<_> = inventory
        .targets
        .iter()
        .zip(choice)
        .map(|(target, &index)| target.chosen(index).expect("a listed candidate"))
        .collect();
    // Targets a chosen fused target absorbs are not computed.
    let absorbed: Vec<usize> = chosen
        .iter()
        .filter_map(|(computing, _)| computing.absorbs)
        .collect();
    (0..inventory.roots.len())
        .map(|query| {
            let mut sketches = Vec::new();
            let mut accumulators = Vec::new();
            for (t, ((computing, candidate), &target_query)) in
                chosen.iter().zip(&query_of_target).enumerate()
            {
                if target_query != query || absorbed.contains(&t) {
                    continue;
                }
                match candidate {
                    ComputationCandidate::Exact => {}
                    ComputationCandidate::ExactAccumulator(states) => {
                        // e.g. "Sum acc", "Sum/Count acc" for an avg, or
                        // "nested Sum acc" for a fused nested sum.
                        let kinds: Vec<String> = states
                            .iter()
                            .map(|state| format!("{:?}", state.kind))
                            .collect();
                        let nested = if computing.absorbs.is_some() {
                            "nested "
                        } else {
                            ""
                        };
                        accumulators.push(format!("{nested}{} acc", kinds.join("/")))
                    }
                    ComputationCandidate::Summary(algorithm) => {
                        sketches.push(sketch_name(algorithm))
                    }
                }
            }
            let mut text = format!("Q{} ", query + 1);
            text += &if sketches.is_empty() {
                "exact".to_string()
            } else {
                sketches.join(" + ")
            };
            if !accumulators.is_empty() {
                text += &format!(" ({})", accumulators.join(", "));
            }
            text
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// What a Pass 2 candidate adds to its Pass 1 candidate's label, e.g.
/// " · one UnivMon for Q1, Q3 · Kll for Q1 in 5 × 1m panes · Sum acc for Q2
/// rolled up from Q1 · shared Scan (Q1, Q2)". `pass1_roots` are the roots
/// of the Pass 1 candidate it derives from, which hold the shared summary
/// builds and the rolled-up builds; `candidate_roots` are its own, which hold its
/// panes, rankings and shared nodes. Readers are found by structure, since
/// later rules rebuild the nodes a rule saw.
pub fn pass2_label(
    pass2: &Pass2Candidate,
    pass1_roots: &[QueryRoot],
    candidate_roots: &[QueryRoot],
) -> String {
    let mut label = String::new();
    // Summary-capability rule, e.g. "UnivMon for Q1, Q3".
    let summaries: Vec<String> = pass2
        .shared_builds
        .iter()
        .map(|builds| {
            let queries = readers(pass1_roots, |node| builds.iter().any(|b| same(node, b)));
            let family = match builds[0].asap().and_then(ASAPOp::produced_state) {
                Some(FieldDataType::Sketch(kind, _)) => format!("{:?}", kind.algorithm()),
                _ => builds[0].operator.kind_name().to_string(),
            };
            format!("{family} for {}", query_names(&queries))
        })
        .collect();
    if !summaries.is_empty() {
        label += &format!(" · one {}", summaries.join(", one "));
    }
    // Window-composition rule, e.g. "Kll for Q1 in 5 × 1m panes": the
    // queries that read the pane chain the build became. Identical builds
    // in different queries become one structurally identical chain, listed
    // once with all its readers.
    let panes: Vec<String> = pass2
        .pane_candidates
        .iter()
        .enumerate()
        .filter(|(i, candidate)| {
            !pass2.pane_candidates[..*i]
                .iter()
                .any(|earlier| same_pane_chain(earlier, candidate))
        })
        .map(|(_, candidate)| {
            format!(
                "{} for {} in {} × {} panes",
                state_name(&candidate.build),
                query_names(&readers(candidate_roots, |node| is_pane_chain_of(
                    node, candidate
                ))),
                candidate.pane_count,
                duration_name(candidate.pane_width_ms)
            )
        })
        .collect();
    if !panes.is_empty() {
        label += &format!(" · {}", panes.join(", "));
    }
    // Rollup rule, e.g. "Sum acc for Q1 rolled up from Q2", then exact
    // top-k reuse, e.g. "top-k for Q1, Q2 from one top-10": the queries
    // that read the ranking.
    let reused: Vec<String> = pass2
        .rollup_candidates
        .iter()
        .map(|candidate| {
            format!(
                "{} for {} rolled up from {}",
                state_name(&candidate.coarse),
                query_names(&readers(pass1_roots, |n| same(n, &candidate.coarse))),
                query_names(&readers(pass1_roots, |n| same(n, &candidate.finer)))
            )
        })
        .chain(pass2.topk_reuse_candidates.iter().map(|candidate| {
            let k = match candidate.ranking.non_asap() {
                Some(NonASAPOp::Limit { n: Some(k), .. }) => *k,
                _ => 0,
            };
            format!(
                "top-k for {} from one top-{k}",
                query_names(&readers(candidate_roots, |n| same(n, &candidate.ranking)))
            )
        }))
        .collect();
    if !reused.is_empty() {
        label += &format!(" · {}", reused.join(", "));
    }
    // Identical-expression rule, e.g. "Scan (Q1, Q2)": each shared node's
    // operator kind and the queries that read it.
    let shared: Vec<String> = pass2
        .sharing_candidates
        .iter()
        .map(|candidate| {
            let node = &candidate.nodes[0];
            format!(
                "{} ({})",
                node.operator.kind_name(),
                query_names(&readers(candidate_roots, |n| same(n, node)))
            )
        })
        .collect();
    if !shared.is_empty() {
        label += &format!(" · shared {}", shared.join(", "));
    }
    label
}

/// Query names for query indices, e.g. "Q1, Q3".
fn query_names(queries: &[usize]) -> String {
    queries
        .iter()
        .map(|q| format!("Q{}", q + 1))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The queries (indices into `roots`) from which a node `matches` is
/// reachable.
fn readers(roots: &[QueryRoot], matches: impl Fn(&OperatorNode) -> bool) -> Vec<usize> {
    roots
        .iter()
        .enumerate()
        .filter(|(_, root)| {
            root_operators(root)
                .into_iter()
                .flat_map(OperatorNode::reachable)
                .any(|n| matches(&n))
        })
        .map(|(q, _)| q)
        .collect()
}

/// Whether `node` is `other` or computes the same thing: the same
/// operators, schemas and children, whatever guarantees, timings and
/// materializations later steps attached.
fn same(node: &OperatorNode, other: &Rc<OperatorNode>) -> bool {
    same_computation(node, other)
}

fn same_computation(a: &OperatorNode, b: &OperatorNode) -> bool {
    std::ptr::eq(a, b)
        || (a.operator.map_children(|_| ()) == b.operator.map_children(|_| ())
            && a.result_kind == b.result_kind
            && a.schema == b.schema
            && a.children().len() == b.children().len()
            && a.children()
                .iter()
                .zip(b.children())
                .all(|(x, y)| same_computation(x, y)))
}

/// Whether `node` is the pane chain `candidate`'s build became: the same
/// family, update, grouping and filter, over the build's window input, in
/// `candidate`'s panes.
fn is_pane_chain_of(node: &OperatorNode, candidate: &PaneCandidate) -> bool {
    let (
        Some(ASAPOp::PaneChain {
            child,
            family,
            input,
            reduction,
            grouping,
            filter,
            pane_width_ms,
            pane_count,
        }),
        Some(ASAPOp::SummaryAgg {
            child: range,
            family: build_family,
            input: build_input,
            reduction: build_reduction,
            grouping: build_grouping,
            filter: build_filter,
        }),
    ) = (node.asap(), candidate.build.asap())
    else {
        return false;
    };
    (*pane_width_ms, *pane_count) == (candidate.pane_width_ms, candidate.pane_count)
        && family == build_family
        && input == build_input
        && reduction == build_reduction
        && grouping == build_grouping
        && filter == build_filter
        && range
            .children()
            .first()
            .is_some_and(|window_input| same(child, window_input))
}

/// Whether two pane candidates become the same pane chain: the same
/// computation in the same panes.
fn same_pane_chain(a: &PaneCandidate, b: &PaneCandidate) -> bool {
    (a.pane_width_ms, a.pane_count) == (b.pane_width_ms, b.pane_count)
        && same_computation(&a.build, &b.build)
}

/// The state a summary build keeps, e.g. "Kll" or "Rate acc".
fn state_name(build: &OperatorNode) -> String {
    match build.asap().and_then(ASAPOp::produced_state) {
        Some(FieldDataType::Sketch(kind, _)) => sketch_name(kind.algorithm()),
        Some(FieldDataType::ExactAggregate(kind, _)) => format!("{kind:?} acc"),
        _ => build.operator.kind_name().to_string(),
    }
}

/// `ms` in the largest whole unit: "1m", "10s", "250ms".
fn duration_name(ms: u64) -> String {
    match ms {
        ms if ms % 3_600_000 == 0 => format!("{}h", ms / 3_600_000),
        ms if ms % 60_000 == 0 => format!("{}m", ms / 60_000),
        ms if ms % 1_000 == 0 => format!("{}s", ms / 1_000),
        ms => format!("{ms}ms"),
    }
}

/// `logical` followed by what the candidate materializes, e.g.
/// "… · ingestion time: Kll ×5 panes · query time, shared: DDSketch";
/// just `logical` when nothing is.
pub fn physical_label(logical: &str, candidate: &PhysicalCandidate) -> String {
    let mut parts: Vec<(&str, Vec<String>)> = Vec::new();
    for (id, materialization) in &candidate.materialization {
        let phase = match materialization {
            Materialization::NotMaterialized => continue,
            Materialization::MatAtIngestion(_) => "ingestion time",
            Materialization::MatAtQuery(_) => "query time, shared",
        };
        let node = candidate
            .dag
            .nodes
            .iter()
            .find(|n| n.id == *id)
            .expect("an assigned node is in the DAG");
        let label = build_label(&node.payload);
        match parts.iter_mut().find(|(p, _)| *p == phase) {
            Some((_, labels)) => labels.push(label),
            None => parts.push((phase, vec![label])),
        }
    }
    std::iter::once(logical.to_string())
        .chain(
            parts
                .into_iter()
                .map(|(phase, labels)| format!("{phase}: {}", labels.join(", "))),
        )
        .collect::<Vec<_>>()
        .join(" · ")
}

/// A summary build, e.g. "Kll ×5 panes" or "Sum acc".
fn build_label<C>(payload: &Operator<C>) -> String {
    let Operator::ASAP(op) = payload else {
        return payload.kind_name().to_string();
    };
    let state = match op {
        ASAPOp::SummaryAgg { family, .. } | ASAPOp::PaneChain { family, .. } => match family {
            FieldDataType::Sketch(kind, _) => sketch_name(kind.algorithm()),
            FieldDataType::ExactAggregate(kind, _) => format!("{kind:?} acc"),
            _ => payload.kind_name().to_string(),
        },
        _ => payload.kind_name().to_string(),
    };
    match op {
        ASAPOp::PaneChain { pane_count, .. } => format!("{state} ×{pane_count} panes"),
        _ => state,
    }
}

/// A sketch algorithm's display name, e.g. "CMS+heap" or "Kll".
fn sketch_name(algorithm: &SketchAlgorithm) -> String {
    match algorithm {
        SketchAlgorithm::CmsWithHeap => "CMS+heap".to_string(),
        SketchAlgorithm::CountSketchWithHeap => "CountSketch+heap".to_string(),
        other => format!("{other:?}"),
    }
}

fn root_operators(root: &QueryRoot) -> Vec<&Rc<OperatorNode>> {
    match root {
        QueryRoot::Operator(node) => vec![node],
        QueryRoot::Scalar(expr) => expr.operator_refs(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner_layering_examples::{example1, example2};
    use crate::planning_stages::{lower_promql_roots, lower_sql_roots, run_planning_stages};

    /// Logical labels name each query's choice and the Pass 2 rules applied;
    /// a physical label adds what it materializes, and with nothing
    /// materialized it is its logical candidate's label.
    #[test]
    fn example1_labels_name_choices_rules_and_materialization() {
        let workload = example1();
        let run =
            run_planning_stages(&workload, lower_promql_roots(&workload).unwrap(), None).unwrap();
        let labels = candidate_labels(&run);
        assert_eq!(
            labels["L1"],
            "Q1 exact (Sum acc, Rate acc) · Q2 exact (Sum acc)"
        );
        assert!(labels
            .values()
            .any(|l| l.ends_with(" · shared Scan (Q1, Q2)")));
        assert!(labels
            .values()
            .any(|l| l.contains("Rate acc for Q1 in 6 × 10s panes")));
        for p in &run.stage2_physical_asap_optimization {
            let label = &labels[&p.id];
            assert!(label.starts_with(&labels[&p.from_logical]), "{label}");
            assert_eq!(
                p.materializes_any(),
                label != &labels[&p.from_logical],
                "{label}"
            );
        }
        assert!(labels
            .values()
            .any(|l| l.ends_with(" · ingestion time: Rate acc ×6 panes")));
        assert!(labels
            .values()
            .any(|l| l.ends_with(" · ingestion time: Sum acc ×6 panes")));
    }

    /// Two identical Sum builds in different queries, both paned, are one
    /// pane chain: the label lists it once, with both readers, instead of
    /// once per build with both readers each time.
    #[test]
    fn identical_pane_chains_in_two_queries_are_listed_once() {
        use crate::planner_layering_examples::shared_data_workload;
        use asap_types::types::AccuracyTarget;
        use asap_types::workload::{
            DataArrival, PlanningWorkload, Predictability, Query, QueryLanguage, QueryRequirements,
            QueryWorkload, RepeatedDemand, RepeatingEntry, RepetitionInterval,
        };
        let panel = |query: &str| RepeatingEntry {
            query: Query(query.into()),
            demand: RepeatedDemand::FixedInterval(RepetitionInterval(10_000)),
            requirements: QueryRequirements {
                accuracy: AccuracyTarget::exact(),
                max_latency_ms: None,
            },
            predictability: Predictability::Predictable { known_at: None },
        };
        let workload = PlanningWorkload {
            query_workload: QueryWorkload {
                language: QueryLanguage::PromQL,
                query_batch: None,
                repeating_queries: Some(vec![
                    panel("sum_over_time(a[1m])"),
                    panel("sum_over_time(a[1m]) * 2"),
                    panel("max_over_time(a[1m])"),
                ]),
            },
            data_workload: Some(shared_data_workload(DataArrival::ContinuouslyIngesting)),
        };
        let run =
            run_planning_stages(&workload, lower_promql_roots(&workload).unwrap(), None).unwrap();
        let labels = candidate_labels(&run);
        let logical: Vec<&String> = run
            .stage1_logical_asap_optimization
            .iter()
            .map(|c| &labels[&c.id])
            .collect();
        for label in &logical {
            assert!(
                !label.contains("Sum acc for Q1, Q2 in 6 × 10s panes, Sum acc for Q1, Q2"),
                "{label}"
            );
        }
        assert!(
            logical
                .iter()
                .any(|l| l.contains("Sum acc for Q1, Q2 in 6 × 10s panes")),
            "{logical:#?}"
        );
        let distinct: std::collections::BTreeSet<&String> = logical.iter().copied().collect();
        assert_eq!(distinct.len(), logical.len(), "{logical:#?}");
    }

    /// A fused target's candidate is labeled apart from the unfused pair's:
    /// the nested sum as "nested Sum acc", beside the two separate sums; an
    /// avg's exact states as "Sum/Count acc".
    #[test]
    fn fused_targets_and_avg_states_have_their_own_labels() {
        use crate::planner_layering_examples::shared_data_workload;
        use asap_types::types::AccuracyTarget;
        use asap_types::workload::{
            DataArrival, PlanningWorkload, Predictability, Query, QueryLanguage, QueryRequirements,
            QueryWorkload, RepeatedDemand, RepeatingEntry, RepetitionInterval,
        };
        let labels_of = |query: &str| {
            let workload = PlanningWorkload {
                query_workload: QueryWorkload {
                    language: QueryLanguage::PromQL,
                    query_batch: None,
                    repeating_queries: Some(vec![RepeatingEntry {
                        query: Query(query.into()),
                        demand: RepeatedDemand::FixedInterval(RepetitionInterval(10_000)),
                        requirements: QueryRequirements {
                            accuracy: AccuracyTarget::exact(),
                            max_latency_ms: None,
                        },
                        predictability: Predictability::Predictable { known_at: None },
                    }]),
                },
                data_workload: Some(shared_data_workload(DataArrival::ContinuouslyIngesting)),
            };
            let run = run_planning_stages(&workload, lower_promql_roots(&workload).unwrap(), None)
                .unwrap();
            let labels = candidate_labels(&run);
            run.stage1_logical_asap_optimization
                .iter()
                .filter(|c| c.pass2.is_none())
                .map(|c| labels[&c.id].clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            labels_of("sum by (job) (sum_over_time(m[5m]))"),
            ["Q1 exact (Sum acc, Sum acc)", "Q1 exact (nested Sum acc)"]
        );
        assert_eq!(labels_of("avg by (job) (m)"), ["Q1 exact (Sum/Count acc)"]);
    }

    /// A summary build read by two queries can be built once per evaluation
    /// for both: its label says "query time, shared".
    #[test]
    fn example2_labels_name_builds_shared_at_query_time() {
        let (workload, catalog) = example2();
        let roots = lower_sql_roots(&workload, &catalog).unwrap();
        let run = run_planning_stages(&workload, roots, None).unwrap();
        let labels = candidate_labels(&run);
        assert!(
            labels
                .values()
                .any(|l| l.contains(" · query time, shared: UnivMon")),
            "{:?}",
            labels
                .values()
                .filter(|l| l.contains("query time"))
                .take(3)
                .collect::<Vec<_>>()
        );
    }
}
