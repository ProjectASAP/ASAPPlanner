// cargo run -p asap-lower --bin dag_export -- \
//     --sql "SELECT service, COUNT(*) FROM metrics GROUP BY service" --name q1 \
//     --promql "topk(5, rate(http_requests_total[5m]))" --name q2
//
// Lowers each given SQL/PromQL query to pre-ASAP IR and prints a single
// `asap_types::dag_export::WorkloadGraph` as JSON on stdout — the input format
// for `tools/dag-viewer` (issue #133). Redirect to a file and load it there:
//   cargo run -p asap-lower --bin dag_export -- --sql "..." --name q1 > /tmp/dag.json
//
// `--name` is optional; an unnamed query defaults to `q<n>` (1-indexed).
//
// `--epsilon <f64>` is optional and applies to every query in the run: it
// lowers with `AccuracyTarget::Epsilon(<f64>)` instead of the default
// `AccuracyTarget::Exact`. Without it, every `AggIntent` lowers exact and
// `asap_aware_mapping::SketchAlgorithmStrategy` never has a genuine sketch
// alternative to report — so no node ever picks up a `SketchApproximation`
// note. Pass it to actually exercise that path, e.g.:
//   cargo run -p asap-lower --bin dag_export -- \
//       --epsilon 0.01 --sql "SELECT quantile(0.99, latency) FROM metrics" --name p99
//
// `--post-asap` is optional and off by default. When passed, this binary
// additionally runs `asap_aware_mapping::replacement::search_workload_with`
// (with `default_strategies()` plus `AvgToSumOverCountStrategy`, which isn't
// one of that crate's own defaults) over every lowered query and ranks each
// discovered `MemoGroup` via `PlanSpace::cost_sorted`. The best-ranked
// candidate per group feeds two additive outputs:
//
//   - one `asap_types::dag_export::TargetReplacement` per group on whichever
//     query's `NamedGraph.replacements` contains that target node (matched
//     by `DagNode::hash` + structural equality, the same collision-safe
//     pattern `annotate_with_explanations` below already uses for notes) —
//     a small, self-contained "before -> after" pair per replacement site;
//   - one merged `NamedGraph.post_graph`: a single flattened graph per
//     query with every winning candidate spliced directly into the query's
//     own pre-ASAP shape in place, built via
//     `asap_types::dag_export::export_post_asap`.
//
// Together these surface every one of the four concrete replacement kinds:
// the sketch family `SketchAlgorithmStrategy`/`HydraGroupingStrategy` bound,
// the CSE share/recompute choice `SharedSubtreeStrategy` found, the
// workload-aware roll-up `RollupStrategy` derived, and the `avg ->
// sum/count` rewrite `AvgToSumOverCountStrategy` proposes. Without
// `--post-asap`, every existing invocation of this binary produces
// byte-identical output to before (`NamedGraph.replacements` is empty and
// `post_graph` is `None`, both skipped from the JSON entirely in that
// case). E.g.:
//   cargo run -p asap-lower --bin dag_export -- \
//       --post-asap --epsilon 0.01 \
//       --sql "SELECT quantile(0.95, latency) FROM metrics" --name q1

use std::collections::HashMap;
use std::rc::Rc;

use asap_aware_mapping::cost_model::DefaultCostModel;
use asap_aware_mapping::replacement::{
    default_strategies, search_workload_with, Replacement, ReplacementProvenance,
    ReplacementStrategy, ReplacementSubDAG,
};
use asap_aware_mapping::rewrite::AvgToSumOverCountStrategy;
use asap_types::dag_export::{
    self, DagGraph, DagNote, NamedGraph, PostAsapSubstitution, TargetReplacement,
    TargetReplacementAfter, WorkloadGraph,
};
use asap_types::post_asap::{GroupingStrategy, SummaryExpr};
use asap_types::pre_asap::agg_intent::AggIntent;
use asap_types::pre_asap::cse::{structural_hash, HashCache};
use asap_types::pre_asap::query_expr::QueryExpr;
use asap_types::pre_asap::schema::{Column, DataType, Schema};
use asap_types::types::AccuracyTarget;

use asap_devtools::{lower_promql, lower_sql, SqlCatalog};

enum Lang {
    Sql,
    PromQl,
}

fn catalog() -> SqlCatalog {
    SqlCatalog::new()
        .with_table(
            "metrics",
            Schema::with_time_index(
                vec![
                    Column::new("ts", DataType::Timestamp, false),
                    Column::new("service", DataType::Utf8, false),
                    Column::new("region", DataType::Utf8, false),
                    Column::new("latency", DataType::Float64, false),
                    Column::new("bytes", DataType::Int64, false),
                ],
                0,
                vec![],
            ),
        )
        .with_table(
            "hosts",
            Schema::new(vec![
                Column::new("service", DataType::Utf8, false),
                Column::new("region", DataType::Utf8, false),
            ]),
        )
}

/// Parses `--sql "<query>" --name "<label>"` / `--promql "<query>" --name
/// "<label>"` pairs off argv, in the order given, plus one optional global
/// `--epsilon <f64>` and one optional global `--post-asap` flag. `--name` is
/// optional and applies to the immediately preceding `--sql`/`--promql`.
fn parse_args() -> (Vec<(String, Lang, String)>, AccuracyTarget, bool) {
    let mut entries: Vec<(String, Lang, String)> = Vec::new();
    let mut pending: Option<(Lang, String)> = None;
    let mut accuracy = AccuracyTarget::Exact;
    let mut post_asap = false;
    let mut args = std::env::args().skip(1);

    fn flush(entries: &mut Vec<(String, Lang, String)>, pending: &mut Option<(Lang, String)>) {
        if let Some((lang, query)) = pending.take() {
            entries.push((format!("q{}", entries.len() + 1), lang, query));
        }
    }

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--sql" | "--promql" => {
                flush(&mut entries, &mut pending);
                let query = args
                    .next()
                    .unwrap_or_else(|| panic!("{arg} requires a query string"));
                let lang = if arg == "--sql" {
                    Lang::Sql
                } else {
                    Lang::PromQl
                };
                pending = Some((lang, query));
            }
            "--name" => {
                let name = args.next().expect("--name requires a value");
                let (lang, query) = pending.take().expect("--name must follow --sql/--promql");
                entries.push((name, lang, query));
            }
            "--epsilon" => {
                let raw = args.next().expect("--epsilon requires a value");
                let epsilon: f64 = raw
                    .parse()
                    .unwrap_or_else(|_| panic!("--epsilon must be a float, got {raw:?}"));
                accuracy = AccuracyTarget::Epsilon(epsilon);
            }
            "--post-asap" => {
                post_asap = true;
            }
            other => panic!("unrecognized argument: {other}"),
        }
    }
    flush(&mut entries, &mut pending);
    (entries, accuracy, post_asap)
}

/// Attach workload-wide replacement explanations to their exact graph nodes.
/// `node_hash` is only a narrowing filter; `source_expr == Some(target)` is
/// the collision-safe identity check (`source_expr` is `None` only for a
/// post-ASAP-originated node inside a `--post-asap` `post_graph`, which this
/// function is never called on — every node it sees, from an ordinary
/// [`dag_export::export`], carries `Some`).
fn annotate_with_explanations(
    graph: &mut DagGraph,
    explanations: &[asap_aware_mapping::ReplacementExplanation],
    matched: &mut [bool],
) {
    for (i, explanation) in explanations.iter().enumerate() {
        for node in graph.nodes.iter_mut() {
            if node.hash == Some(explanation.node_hash)
                && node.source_expr.as_ref() == Some(explanation.target.as_ref())
            {
                node.notes.push(DagNote {
                    kind: format!("{:?}", explanation.kind),
                    reason: explanation.reason.clone(),
                });
                matched[i] = true;
            }
        }
    }
}

/// Which display `strategy` label a winning candidate earns, derived from
/// its `ReplacementProvenance` plus (for `SummaryImplementation`/
/// `LogicalRewrite`, which each cover more than one concrete strategy) a
/// look at the winning candidate's own shape:
///
/// - `SummaryImplementation` covers both `SketchAlgorithmStrategy` and
///   `HydraGroupingStrategy` — both produce `Replacement::Summary`
///   candidates with this same provenance, distinguished only by whether
///   the bound `SummaryAgg`'s own `grouping` chose
///   `GroupingStrategy::SharedMultiSubpopulation` (Hydra) or not (an
///   ordinary per-subpopulation sketch instance).
/// - `CseShare`/`CseRecompute` both come from `SharedSubtreeStrategy`'s own
///   two-way share-vs-recompute pair — one label either way, since the
///   *choice* of which of the two won is exactly what `rank`/`cost`/`after`
///   already show, not something the strategy label itself needs to encode.
/// - `LogicalRewrite` covers both `AvgToSumOverCountStrategy` and
///   `RollupStrategy` — both emit a `Replacement::Rewrite` with this same
///   provenance, and (per `rollup.rs`) `RollupStrategy` never sets
///   `provenance` to anything else either, so provenance alone can't tell
///   them apart. Disambiguated instead by `target`'s own shape:
///   `AvgToSumOverCountStrategy::matches` requires exactly one
///   `AggIntent::Avg` measure (`rewrite.rs`'s own `avg_rewrite_target`
///   precondition) — and `Avg` has no summary/accumulator realization at
///   all (`implementations_for_with` dispatches it straight to
///   `PassThrough`), so it can never be a `RollupStrategy` source either
///   (that strategy only rolls up ordinary mergeable accumulators). A
///   target whose sole measure is `Avg` is therefore unambiguously an
///   `AvgToSumOverCountStrategy` candidate, and everything else this
///   provenance produces is `RollupStrategy`'s.
fn classify_strategy(target: &QueryExpr, winner: &ReplacementSubDAG) -> String {
    match winner.provenance {
        ReplacementProvenance::SummaryImplementation => match &winner.replacement {
            Replacement::Summary(node) => match &node.expr {
                SummaryExpr::SummaryAgg {
                    grouping: GroupingStrategy::SharedMultiSubpopulation { .. },
                    ..
                } => "HydraGrouping".to_string(),
                _ => "Sketch".to_string(),
            },
            // Never actually produced by SketchAlgorithmStrategy/
            // HydraGroupingStrategy (both only ever emit
            // Replacement::Summary) — a defensive fallback, not a case this
            // binary expects to hit.
            Replacement::Rewrite(_) => "Sketch".to_string(),
        },
        ReplacementProvenance::CseShare | ReplacementProvenance::CseRecompute => {
            "SharedSubtree".to_string()
        }
        ReplacementProvenance::LogicalRewrite => {
            if let QueryExpr::Aggregate { measures, .. } = target {
                if let [AggIntent::Avg { .. }] = measures.as_slice() {
                    return "AvgToSumRewrite".to_string();
                }
            }
            "Rollup".to_string()
        }
    }
}

/// One `MemoGroup`'s best-ranked candidate, kept alongside its own `target`
/// and `cost` — the unit both [`PostAsapResults::replacements`] and
/// [`PostAsapResults::post_graphs`] are built from, so the two outputs can
/// never disagree about which candidate won for a given target.
struct Winner<'a> {
    target: &'a Rc<QueryExpr>,
    candidate: &'a ReplacementSubDAG,
    cost: f64,
}

/// Find the index into `winners` (if any) whose `target` is structurally
/// identical to `expr` — `by_hash` is only a narrowing filter (keyed by
/// [`structural_hash`]); `expr == target` is the collision-safe identity
/// check, the same two-step discipline [`annotate_with_explanations`] and
/// this file's original single-target matching already use. Returns an
/// index rather than a `&Winner` directly so a caller can both use the
/// match and record it (e.g. `matched[i] = true`) without juggling a second
/// way to name the same winner.
fn lookup_winner(
    by_hash: &HashMap<u64, Vec<usize>>,
    winners: &[Winner<'_>],
    cache: &mut HashCache,
    expr: &QueryExpr,
) -> Option<usize> {
    let hash = structural_hash(expr, cache);
    by_hash
        .get(&hash)?
        .iter()
        .copied()
        .find(|&i| expr == winners[i].target.as_ref())
}

/// Build a [`TargetReplacement`] for `winner`, matching this file's own
/// per-target `before`/`after` construction.
fn target_replacement(target_pre_id: u32, winner: &Winner<'_>) -> TargetReplacement {
    let strategy = classify_strategy(winner.target, winner.candidate);
    let before = dag_export::export(winner.target);
    let after = match &winner.candidate.replacement {
        Replacement::Summary(node) => {
            TargetReplacementAfter::Summary(dag_export::export_summary(node))
        }
        Replacement::Rewrite(rewritten) => {
            TargetReplacementAfter::Rewrite(dag_export::export(rewritten))
        }
    };
    TargetReplacement {
        target_pre_id,
        strategy,
        rationale: winner.candidate.rationale.clone(),
        rank: 0,
        cost: winner.cost,
        before,
        after,
    }
}

/// The two additive `--post-asap` outputs — see this file's top-of-file
/// usage doc for what each is for.
struct PostAsapResults {
    /// One `(query_name, TargetReplacement)` pair per discovered replacement
    /// site whose target node is found in that query's own exported graph. A
    /// target can in principle be reachable from more than one query's root
    /// after CSE (a shared subtree), in which case it yields one pair per
    /// matching query, each with that query's own `target_pre_id`.
    replacements: Vec<(String, TargetReplacement)>,
    /// One merged, whole-query [`DagGraph`] per query, built via
    /// [`dag_export::export_post_asap`] — every winning candidate spliced
    /// directly into that query's own pre-ASAP shape in place.
    post_graphs: Vec<(String, DagGraph)>,
}

/// Run `asap_aware_mapping::replacement::search_workload_with` (this
/// binary's own strategy set — `default_strategies()` plus
/// `AvgToSumOverCountStrategy`, since that strategy isn't one of that
/// crate's own defaults) over every lowered query, rank each discovered
/// `MemoGroup` via `PlanSpace::cost_sorted`, and build both `--post-asap`
/// outputs from the exact same set of winning candidates (see [`Winner`]),
/// so the flat `replacements` list and the merged `post_graph` can never
/// disagree about which candidate won for a given target.
fn run_post_asap(lowered_queries: &[(String, String, QueryExpr)]) -> PostAsapResults {
    let mut strategies: Vec<Box<dyn ReplacementStrategy>> = default_strategies();
    strategies.push(Box::new(AvgToSumOverCountStrategy));

    let roots: Vec<(String, Rc<QueryExpr>)> = lowered_queries
        .iter()
        .map(|(name, _, qe)| (name.clone(), Rc::new(qe.clone())))
        .collect();
    let space = search_workload_with(roots, &strategies);
    let ranked_groups = space.cost_sorted(&DefaultCostModel);

    let winners: Vec<Winner<'_>> = ranked_groups
        .iter()
        .filter_map(|group| {
            let candidate = group.candidates.first()?;
            Some(Winner {
                target: group.target,
                candidate,
                cost: group.costs[0],
            })
        })
        .collect();

    // `by_hash` only narrows the search; `lookup_winner`'s own structural
    // equality check is the real decision — see that function's doc.
    let mut by_hash_cache = HashCache::new();
    let mut by_hash: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, winner) in winners.iter().enumerate() {
        let hash = structural_hash(winner.target, &mut by_hash_cache);
        by_hash.entry(hash).or_default().push(i);
    }

    // ---- Flat per-target `replacements`, one list per query -----------
    //
    // Independently re-export every query's own graph for matching — a
    // fresh `export` per query, not reused from `main`'s own already-built
    // `NamedGraph`s, so this function stays self-contained and callable on
    // its own (see this file's `#[cfg(test)]` module).
    let mut lookup_cache = HashCache::new();
    let mut replacements = Vec::new();
    let mut matched = vec![false; winners.len()];
    for (name, _, qe) in lowered_queries {
        let graph = dag_export::export(qe);
        for node in &graph.nodes {
            let Some(source_expr) = node.source_expr.as_ref() else {
                continue; // never true for a plain `export` — defensive only.
            };
            if let Some(i) = lookup_winner(&by_hash, &winners, &mut lookup_cache, source_expr) {
                replacements.push((name.clone(), target_replacement(node.id, &winners[i])));
                matched[i] = true;
            }
        }
    }
    for (winner, matched) in winners.iter().zip(&matched) {
        if !matched {
            let strategy = classify_strategy(winner.target, winner.candidate);
            eprintln!(
                "dag_export: post-asap replacement ({strategy}) matched no DagNode in any query graph"
            );
        }
    }

    // ---- Merged, whole-query `post_graph`, one per query ---------------
    //
    // A fresh `HashCache` here (rather than reusing `lookup_cache` above):
    // `export_post_asap` calls `find_winner` at every node of every query's
    // own tree, not just at nodes a plain `export` already flattened, so
    // this is a genuinely separate hashing pass, not a re-walk of the same
    // nodes the loop above already visited.
    let mut post_graph_cache = HashCache::new();
    let mut find_winner = |expr: &QueryExpr| -> Option<PostAsapSubstitution> {
        let i = lookup_winner(&by_hash, &winners, &mut post_graph_cache, expr)?;
        Some(match &winners[i].candidate.replacement {
            Replacement::Rewrite(rc) => PostAsapSubstitution::Rewrite(Rc::clone(rc)),
            Replacement::Summary(rc) => PostAsapSubstitution::Summary(Rc::clone(rc)),
        })
    };
    let post_graphs: Vec<(String, DagGraph)> = lowered_queries
        .iter()
        .map(|(name, _, qe)| {
            (
                name.clone(),
                dag_export::export_post_asap(qe, &mut find_winner),
            )
        })
        .collect();

    PostAsapResults {
        replacements,
        post_graphs,
    }
}

#[tokio::main]
async fn main() {
    let (entries, accuracy, post_asap) = parse_args();
    if entries.is_empty() {
        eprintln!(
            "usage: dag_export --sql \"<query>\" [--name <label>] [--epsilon <f64>] [--post-asap] ..."
        );
        std::process::exit(1);
    }

    let mut lowered_queries = Vec::new();
    for (name, lang, query) in entries {
        let lowered = match lang {
            Lang::Sql => lower_sql(&query, &catalog(), accuracy.clone())
                .await
                .map_err(|e| e.to_string()),
            Lang::PromQl => lower_promql(&query, accuracy.clone()).map_err(|e| e.to_string()),
        };
        match lowered {
            Ok(qe) => {
                lowered_queries.push((name, query, qe));
            }
            Err(e) => eprintln!("skipping {name:?} — lowering failed: {e}"),
        }
    }

    let explanations = asap_aware_mapping::explain_replacements(
        lowered_queries
            .iter()
            .map(|(name, _, qe)| (name.clone(), qe.clone()))
            .collect(),
    );
    let mut matched = vec![false; explanations.len()];
    let mut queries = Vec::new();
    for (name, source, qe) in &lowered_queries {
        let mut graph = dag_export::export(qe);
        annotate_with_explanations(&mut graph, &explanations, &mut matched);
        queries.push(NamedGraph {
            name: name.clone(),
            source: Some(source.clone()),
            graph,
            replacements: Vec::new(),
            post_graph: None,
        });
    }
    for (explanation, matched) in explanations.iter().zip(matched) {
        if !matched {
            eprintln!(
                "dag_export: explanation at {} ({:?}, node_hash={}) matched no DagNode",
                explanation.location, explanation.kind, explanation.node_hash
            );
        }
    }

    if post_asap {
        let results = run_post_asap(&lowered_queries);
        for (query_name, replacement) in results.replacements {
            if let Some(named) = queries.iter_mut().find(|q| q.name == query_name) {
                named.replacements.push(replacement);
            }
        }
        for (query_name, post_graph) in results.post_graphs {
            if let Some(named) = queries.iter_mut().find(|q| q.name == query_name) {
                named.post_graph = Some(post_graph);
            }
        }
    }

    let workload = WorkloadGraph { queries };
    println!("{}", serde_json::to_string_pretty(&workload).unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workload_wide_explanations_annotate_cross_query_reuse() {
        let query = "sum by (job) (rate(http_requests_total[5m]))";
        let a = lower_promql(query, AccuracyTarget::Exact).unwrap();
        let b = lower_promql(query, AccuracyTarget::Exact).unwrap();
        let explanations =
            asap_aware_mapping::explain_replacements(vec![("a", a.clone()), ("b", b.clone())]);
        assert!(explanations
            .iter()
            .any(|e| { e.kind == asap_aware_mapping::ExplanationKind::CommonSubexpressionReuse }));

        let mut matched = vec![false; explanations.len()];
        let mut graph_a = dag_export::export(&a);
        let mut graph_b = dag_export::export(&b);
        annotate_with_explanations(&mut graph_a, &explanations, &mut matched);
        annotate_with_explanations(&mut graph_b, &explanations, &mut matched);
        assert!(graph_a.nodes.iter().any(|n| !n.notes.is_empty()));
        assert!(graph_b.nodes.iter().any(|n| !n.notes.is_empty()));
    }

    #[test]
    fn hash_collision_without_structural_equality_does_not_attach_a_note() {
        let target = lower_promql(
            "quantile(0.99, rate(http_requests_total[5m]))",
            AccuracyTarget::Epsilon(0.01),
        )
        .unwrap();
        let explanations = asap_aware_mapping::explain_replacements(vec![("target", target)]);
        let explanation = explanations
            .iter()
            .find(|e| e.kind == asap_aware_mapping::ExplanationKind::SketchApproximation)
            .unwrap();

        let unrelated =
            lower_promql("sum(rate(other_metric[5m]))", AccuracyTarget::Epsilon(0.01)).unwrap();
        let mut graph = dag_export::export(&unrelated);
        for node in &mut graph.nodes {
            node.hash = Some(explanation.node_hash);
        }
        let mut matched = vec![false; explanations.len()];
        annotate_with_explanations(&mut graph, &explanations, &mut matched);
        assert!(graph.nodes.iter().all(|n| n.notes.is_empty()));
    }

    /// The `--post-asap` code path, exercised directly (not through the CLI):
    /// a workload with one sketch-approximable quantile aggregate and one
    /// `avg` aggregate must produce at least one `TargetReplacement` with
    /// `after: TargetReplacementAfter::Summary(..)` (the bound sketch) and
    /// at least one with `after: TargetReplacementAfter::Rewrite(..)` (the
    /// `avg -> sum/count` rewrite) — see [`run_post_asap`]. Also checks that
    /// a non-empty `post_graph` comes back for every query, since that's the
    /// other `--post-asap` output `main` wires up.
    #[tokio::test]
    async fn post_asap_run_produces_both_summary_and_rewrite_replacements() {
        let cat = catalog();
        let sketch_query = lower_sql(
            "SELECT approx_percentile_cont(latency, 0.95) FROM metrics",
            &cat,
            AccuracyTarget::Epsilon(0.01),
        )
        .await
        .unwrap();
        let avg_query = lower_sql(
            "SELECT service, AVG(latency) FROM metrics GROUP BY service",
            &cat,
            AccuracyTarget::Epsilon(0.01),
        )
        .await
        .unwrap();

        let lowered_queries = vec![
            (
                "sketch".to_string(),
                "SELECT approx_percentile_cont(latency, 0.95) FROM metrics".to_string(),
                sketch_query,
            ),
            (
                "avg".to_string(),
                "SELECT service, AVG(latency) FROM metrics GROUP BY service".to_string(),
                avg_query,
            ),
        ];

        let results = run_post_asap(&lowered_queries);

        assert!(
            results
                .replacements
                .iter()
                .any(|(_, r)| matches!(r.after, TargetReplacementAfter::Summary(_))),
            "expected at least one Summary replacement (the sketch-bound quantile aggregate): {:?}",
            results
                .replacements
                .iter()
                .map(|(name, r)| (name.clone(), r.strategy.clone()))
                .collect::<Vec<_>>()
        );
        assert!(
            results
                .replacements
                .iter()
                .any(|(_, r)| matches!(r.after, TargetReplacementAfter::Rewrite(_))),
            "expected at least one Rewrite replacement (the avg -> sum/count rewrite): {:?}",
            results
                .replacements
                .iter()
                .map(|(name, r)| (name.clone(), r.strategy.clone()))
                .collect::<Vec<_>>()
        );
        // The avg rewrite's own strategy label must disambiguate correctly
        // (not fall through to "Rollup", the other LogicalRewrite shape).
        assert!(results
            .replacements
            .iter()
            .any(|(_, r)| r.strategy == "AvgToSumRewrite"));

        assert_eq!(results.post_graphs.len(), 2, "one post_graph per query");
        for (name, graph) in &results.post_graphs {
            assert!(
                !graph.nodes.is_empty(),
                "post_graph for {name:?} must not be empty"
            );
        }
    }
}
