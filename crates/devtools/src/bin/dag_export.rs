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
// additionally runs `asap_aware_mapping::replacement::search_workload` (this
// binary took no strategies of its own — `default_strategies()` already
// includes `AvgToSumOverCountStrategy` as of #282) over every lowered query
// and ranks each discovered `MemoGroup` via `PlanSpace::cost_sorted`. The
// best-ranked
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
use std::time::Instant;

use asap_aware_mapping::cost_model::DefaultCostModel;
use asap_aware_mapping::replacement::{search_workload, Replacement, ReplacementSubDAG};
use asap_types::dag_export::{
    self, DagDecision, DagGraph, DagNote, NamedGraph, PostAsapSubstitution, TargetReplacement,
    TargetReplacementAfter, WorkloadGraph,
};
use asap_types::post_asap::SummaryExpr;
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
fn parse_args() -> (Vec<(String, Lang, String)>, AccuracyTarget, bool, bool) {
    let mut entries: Vec<(String, Lang, String)> = Vec::new();
    let mut pending: Option<(Lang, String)> = None;
    let mut accuracy = AccuracyTarget::Exact;
    let mut post_asap = false;
    let mut progress = false;
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
            "--progress" => {
                progress = true;
            }
            other => panic!("unrecognized argument: {other}"),
        }
    }
    flush(&mut entries, &mut pending);
    (entries, accuracy, post_asap, progress)
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

/// One `MemoGroup`'s best-ranked candidate, kept alongside its own `target`
/// and `cost` — the unit both [`PostAsapResults::replacements`] and
/// [`PostAsapResults::post_graphs`] are built from, so the two outputs can
/// never disagree about which candidate won for a given target.
struct Winner<'a> {
    target: &'a Rc<QueryExpr>,
    candidate: &'a ReplacementSubDAG,
    cost: f64,
}

/// Short explanation intended for a selected winner in node-level UI. The
/// candidate's full rationale remains available in pre-ASAP applicability
/// notes; repeating that exhaustive prose on every post-ASAP region node
/// obscures the actual decision.
fn decision_rationale(winner: &Winner<'_>) -> String {
    match winner.candidate.strategy {
        "AvgToSumOverCountStrategy" => {
            "Rewrites AVG into SUM and COUNT under the same grouping, then divides SUM by COUNT."
                .to_string()
        }
        "SharedSubtreeStrategy" => match winner.candidate.provenance {
            asap_aware_mapping::replacement::ReplacementProvenance::CseShare => {
                "Builds the repeated subtree once and shares it across consumers.".to_string()
            }
            asap_aware_mapping::replacement::ReplacementProvenance::CseRecompute => {
                "Recomputes the subtree per consumer because that has the lower estimated cost."
                    .to_string()
            }
            _ => "Chooses the lowest-cost handling of the repeated subtree.".to_string(),
        },
        "RollupStrategy" => {
            "Answers this aggregate from a compatible finer-grained aggregate.".to_string()
        }
        "TopKLimitReuseStrategy" => {
            "Derives this smaller top-k from a compatible larger top-k result shared by the workload."
                .to_string()
        }
        _ => {
            let summary = winner
                .candidate
                .rationale
                .split_once(" — ")
                .map_or(winner.candidate.rationale.as_str(), |(summary, _)| summary);
            summary
                .split_once(" (asap_")
                .map_or(summary, |(plain, _)| plain)
                .to_string()
        }
    }
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
fn target_replacement(
    decision_id: u32,
    target_pre_id: u32,
    winner: &Winner<'_>,
) -> TargetReplacement {
    let strategy = winner.candidate.strategy.to_string();
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
        decision_id,
        target_pre_id,
        strategy,
        rationale: decision_rationale(winner),
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

/// Assign collision-free, explicit identities to structurally equal nodes
/// across a set of exported query graphs. The full canonical subtree string
/// is the equality key; the compact integer is what JSON consumers receive.
/// Consequently the viewer never needs to guess identity from labels,
/// hashes, or a client-side node signature.
fn assign_workload_node_ids(graphs: &mut [&mut DagGraph]) {
    fn key_for(id: u32, graph: &DagGraph, memo: &mut HashMap<u32, String>) -> String {
        if let Some(key) = memo.get(&id) {
            return key.clone();
        }
        let node = &graph.nodes[id as usize];
        let child_keys: Vec<_> = node
            .children
            .iter()
            .map(|child| key_for(*child, graph, memo))
            .collect();
        let key = serde_json::to_string(&(node.kind, &node.detail, &node.schema, child_keys))
            .expect("exported DAG node content is serializable");
        memo.insert(id, key.clone());
        key
    }

    let mut ids = HashMap::<String, u32>::new();
    let mut next_id = 0_u32;
    for graph in graphs.iter_mut() {
        let mut memo = HashMap::new();
        let keys: Vec<_> = graph
            .nodes
            .iter()
            .map(|node| key_for(node.id, graph, &mut memo))
            .collect();
        for (node, key) in graph.nodes.iter_mut().zip(keys) {
            let id = *ids.entry(key).or_insert_with(|| {
                let id = next_id;
                next_id += 1;
                id
            });
            node.workload_node_id = Some(id);
        }
    }
}

/// Run `asap_aware_mapping::replacement::search_workload` (its own
/// `default_strategies()` — which includes `AvgToSumOverCountStrategy` as of
/// #282 — is exactly the strategy set this binary wants; no custom list
/// needed) over every lowered query, rank each discovered `MemoGroup` via
/// `PlanSpace::cost_sorted`, and build both `--post-asap` outputs from the
/// exact same set of winning candidates (see [`Winner`]), so the flat
/// `replacements` list and the merged `post_graph` can never disagree about
/// which candidate won for a given target.
fn run_post_asap_with_progress(
    lowered_queries: &[(String, String, QueryExpr)],
    progress: bool,
) -> PostAsapResults {
    let mapping_started = Instant::now();
    if progress {
        eprintln!("[3/4] ASAP-aware mapping is running…");
    }
    let roots: Vec<(String, Rc<QueryExpr>)> = lowered_queries
        .iter()
        .map(|(name, _, qe)| (name.clone(), Rc::new(qe.clone())))
        .collect();
    let space = search_workload(roots);
    let ranked_groups = space.cost_sorted(&DefaultCostModel);

    // A group's top candidate can be `keep_pre_asap`'s own conservative
    // fallback — `Replacement::Summary(SummaryNode { expr:
    // KeepPreAsap(Rc::new(target.clone())), .. })` — the *whole target*
    // wrapped as unbound, e.g. for a multi-measure/`HAVING`-bearing
    // aggregate, or (the case that actually surfaces this: `STDDEV_POP`/
    // `AVG`/`VARIANCE` dispatch to `Implementation::PassThrough` with no
    // alternative at all, per `implementations_for_with`'s own doc) an
    // intent with no summary realization whatsoever. This isn't a
    // replacement decision — it's `SketchAlgorithmStrategy` saying "nothing
    // to bind here" — the identical "no-op candidate" concept
    // `explanation.rs`'s own `sketch_finding_reason` already excludes from
    // being reported as a finding ("a candidate list containing only the
    // trivial no-op realization... isn't an opportunity, it's just the
    // target's existing shape reflected back"). Filtered out here for a
    // second, load-bearing reason beyond just matching that precedent:
    // `export_post_asap`'s `find_winner` re-checks every node reached
    // inside a spliced-in `KeepPreAsap` payload (by design, so a target
    // nested underneath one still gets found) — if that payload structurally
    // *is* the enclosing target, `find_winner` immediately matches the same
    // winner again, forever. Treating this candidate as "no winner" (same
    // as an empty candidate list) avoids ever handing `export_post_asap` a
    // winner that can't help but recurse into itself.
    let winners: Vec<Winner<'_>> = ranked_groups
        .iter()
        .filter_map(|group| {
            let candidate = group.candidates.first()?;
            if matches!(
                &candidate.replacement,
                Replacement::Summary(node) if matches!(node.expr, SummaryExpr::KeepPreAsap(_))
            ) {
                return None;
            }
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
    if progress {
        eprintln!(
            "[3/4] ASAP-aware mapping done in {:.2} ms",
            mapping_started.elapsed().as_secs_f64() * 1000.0
        );
    }

    // ---- Merged, whole-query `post_graph`, one per query ---------------
    //
    // Built *before* the flat `replacements` pass below, not after: a
    // winner whose target only exists inside another winner's own
    // `Replacement::Rewrite` output (e.g. the `sum`/`count` aggregates
    // `AvgToSumOverCountStrategy`'s rewrite exposes, which
    // `default_strategies()` — #282 — now discovers and independently
    // sketch-ranks in the same search pass) can never appear in any query's
    // *original*, pre-rewrite `graph` — there's nothing wrong with that
    // winner, it's just nested. `post_graph` is where it's expected to
    // surface instead (`export_post_asap`'s recursive `find_winner`
    // threading walks straight through a rewritten subtree and re-checks
    // every node inside it too), so the flat-`replacements` pass below
    // checks there before deciding a miss is a real anomaly worth a
    // warning.
    if progress {
        eprintln!("[4/4] Post-ASAP DAG generation is running…");
    }
    let post_started = Instant::now();
    let mut post_graph_cache = HashCache::new();
    let mut find_winner = |expr: &QueryExpr| -> Option<PostAsapSubstitution> {
        let i = lookup_winner(&by_hash, &winners, &mut post_graph_cache, expr)?;
        let winner = &winners[i];
        let decision = DagDecision {
            id: i as u32,
            strategy: winner.candidate.strategy.to_string(),
            rationale: decision_rationale(winner),
            rank: 0,
            cost: winner.cost,
            role: "replacement_region",
        };
        Some(match &winners[i].candidate.replacement {
            Replacement::Rewrite(rc) => PostAsapSubstitution::Rewrite {
                replacement: Rc::clone(rc),
                decision,
            },
            Replacement::Summary(rc) => PostAsapSubstitution::Summary {
                replacement: Rc::clone(rc),
                decision,
            },
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

    // ---- Flat per-target `replacements`, one list per query -----------
    //
    // Independently re-export every query's own graph for matching — a
    // fresh `export` per query, not reused from `main`'s own already-built
    // `NamedGraph`s, so this function stays self-contained and callable on
    // its own (see this file's `#[cfg(test)]` module). Deliberately anchored
    // to the *original* `graph` only (never `post_graph`) — `target_pre_id`
    // is documented as an id into `NamedGraph.graph.nodes`, so a nested
    // secondary target (see above) never gets a flat entry of its own here:
    // it's already visible, in place, inside its parent's own `after`
    // subtree and inside `post_graph` as a whole.
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
                replacements.push((
                    name.clone(),
                    target_replacement(i as u32, node.id, &winners[i]),
                ));
                matched[i] = true;
            }
        }
    }

    // A winner can legitimately stay unmatched here: `default_strategies()`
    // (#282) means `AvgToSumOverCountStrategy`'s rewrite output (and
    // similarly `RollupStrategy`'s) can expose a brand-new `sum`/`count`
    // descendant that the *same* search pass then independently discovers
    // and ranks — a real winner, but one with no node anywhere in any
    // query's original, pre-rewrite `graph` to attach a flat entry to
    // (`target_pre_id` is documented as an id into `graph.nodes`
    // specifically). This isn't a data loss: `export_post_asap` still
    // splices that winner in, in place, inside `post_graph` — see this
    // function's own construction of `post_graphs` above, which walks
    // straight through a rewritten subtree and resolves every nested
    // winner too, recursively. So an unmatched winner here is expected,
    // not necessarily a bug, whenever it's downstream of some other
    // winner's own `Replacement::Rewrite` — logged as an FYI rather than a
    // warning, since telling the two cases apart precisely would mean
    // reimplementing `search`'s own private descendant-discovery walk
    // (`discover_new_descendant_targets` in `asap_aware_mapping::replacement`,
    // not exposed) a second time here just to double-check something
    // `post_graph`'s own construction already handled correctly.
    for (winner, matched) in winners.iter().zip(&matched) {
        if !matched {
            let strategy = winner.candidate.strategy;
            eprintln!(
                "dag_export: post-asap replacement ({strategy}) has no node in any query's \
                 original graph — expected for a winner exposed only inside another winner's \
                 own rewrite output (e.g. a sum/count descendant of an avg rewrite); still \
                 present in that query's post_graph"
            );
        }
    }
    if progress {
        eprintln!(
            "[4/4] Post-ASAP DAG generation done in {:.2} ms",
            post_started.elapsed().as_secs_f64() * 1000.0
        );
    }

    PostAsapResults {
        replacements,
        post_graphs,
    }
}

#[cfg(test)]
fn run_post_asap(lowered_queries: &[(String, String, QueryExpr)]) -> PostAsapResults {
    run_post_asap_with_progress(lowered_queries, false)
}

#[tokio::main]
async fn main() {
    let (entries, accuracy, post_asap, progress) = parse_args();
    let planner_started = Instant::now();
    if entries.is_empty() {
        eprintln!(
            "usage: dag_export --sql \"<query>\" [--name <label>] [--epsilon <f64>] [--post-asap] ..."
        );
        std::process::exit(1);
    }

    let mut lowered_queries = Vec::new();
    let lowering_started = Instant::now();
    if progress {
        eprintln!("[1/4] Parsing and lowering SQL/PromQL queries…");
    }
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
    if progress {
        eprintln!(
            "[1/4] Parsing and lowering done in {:.2} ms",
            lowering_started.elapsed().as_secs_f64() * 1000.0
        );
    }

    let pre_started = Instant::now();
    if progress {
        eprintln!("[2/4] Pre-ASAP DAG generation is running…");
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
    if progress {
        eprintln!(
            "[2/4] Pre-ASAP DAG generation done in {:.2} ms",
            pre_started.elapsed().as_secs_f64() * 1000.0
        );
    }

    if post_asap {
        let results = run_post_asap_with_progress(&lowered_queries, progress);
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

    {
        let mut pre_graphs: Vec<_> = queries.iter_mut().map(|query| &mut query.graph).collect();
        assign_workload_node_ids(&mut pre_graphs);
    }
    {
        let mut post_graphs: Vec<_> = queries
            .iter_mut()
            .filter_map(|query| query.post_graph.as_mut())
            .collect();
        assign_workload_node_ids(&mut post_graphs);
    }

    let workload = WorkloadGraph { queries };
    if progress {
        eprintln!(
            "Total planner time: {:.2} ms",
            planner_started.elapsed().as_secs_f64() * 1000.0
        );
    }
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
            .any(|(_, r)| r.strategy == "AvgToSumOverCountStrategy"));

        assert_eq!(results.post_graphs.len(), 2, "one post_graph per query");
        for (name, graph) in &results.post_graphs {
            assert!(
                !graph.nodes.is_empty(),
                "post_graph for {name:?} must not be empty"
            );
        }
        let avg_post = &results
            .post_graphs
            .iter()
            .find(|(name, _)| name == "avg")
            .expect("avg post graph")
            .1;
        assert_eq!(
            avg_post
                .nodes
                .iter()
                .filter(|node| node.kind == "Scan")
                .count(),
            1,
            "AVG's SUM and COUNT branches must retain their shared input as one DAG node"
        );
        let decisions: Vec<_> = results
            .post_graphs
            .iter()
            .flat_map(|(_, graph)| graph.nodes.iter().filter_map(|node| node.decision.as_ref()))
            .collect();
        assert!(
            !decisions.is_empty(),
            "post_graph nodes must carry explicit strategy metadata"
        );
        for decision in decisions {
            assert!(!decision.strategy.is_empty());
            assert!(!decision.rationale.is_empty());
        }
    }

    #[test]
    fn workload_node_ids_make_smaller_topk_reuse_explicit() {
        let small = lower_promql(
            "topk(5, rate(http_requests_total[5m]))",
            AccuracyTarget::Epsilon(0.01),
        )
        .unwrap();
        let large = lower_promql(
            "topk(10, rate(http_requests_total[5m]))",
            AccuracyTarget::Epsilon(0.01),
        )
        .unwrap();
        let lowered = vec![
            (
                "q3".into(),
                "topk(5, rate(http_requests_total[5m]))".into(),
                small,
            ),
            (
                "q4".into(),
                "topk(10, rate(http_requests_total[5m]))".into(),
                large,
            ),
        ];
        let mut results = run_post_asap(&lowered);
        let mut graph_refs: Vec<_> = results
            .post_graphs
            .iter_mut()
            .map(|(_, graph)| graph)
            .collect();
        assign_workload_node_ids(&mut graph_refs);

        let q3 = &results
            .post_graphs
            .iter()
            .find(|(name, _)| name == "q3")
            .unwrap()
            .1;
        let q4 = &results
            .post_graphs
            .iter()
            .find(|(name, _)| name == "q4")
            .unwrap()
            .1;
        let q3_root = &q3.nodes[q3.root as usize];
        assert_eq!(q3_root.label, "Limit(5)");
        assert_eq!(
            q3_root
                .decision
                .as_ref()
                .map(|decision| decision.strategy.as_str()),
            Some("TopKLimitReuseStrategy")
        );
        let q3_large = &q3.nodes[q3_root.children[0] as usize];
        let q4_large = &q4.nodes[q4.root as usize];
        assert_eq!(q3_large.label, "Limit(10)");
        assert_eq!(q3_large.workload_node_id, q4_large.workload_node_id);
    }

    /// Regression test for a real stack overflow found via manual testing
    /// against real corpus queries (a `STDDEV_POP` aggregate, which — like
    /// `AVG` — dispatches to `Implementation::PassThrough` with no
    /// alternative strategy of its own, so its *only* candidate is
    /// `keep_pre_asap`'s conservative fallback: `Replacement::Summary`
    /// wrapping the *entire target* as `SummaryExpr::KeepPreAsap`).
    /// `run_post_asap` must not treat that as a real winner: splicing it
    /// into `export_post_asap` would recurse forever, since `find_winner`
    /// re-checks every node inside a spliced `KeepPreAsap` payload by
    /// design, and this payload structurally *is* the enclosing target — a
    /// fresh `find_winner` call finds the identical winner again,
    /// unconditionally, every time. Filtering this shape out of `winners`
    /// (same "no-op candidate" concept `explanation.rs`'s own
    /// `sketch_finding_reason` already excludes from being a finding) is
    /// what keeps this terminating: this test's only assertion that matters
    /// is that `run_post_asap` returns at all instead of overflowing the
    /// stack.
    #[tokio::test]
    async fn post_asap_does_not_recurse_forever_on_a_trivial_keep_pre_asap_winner() {
        let cat = catalog();
        let stddev_query = lower_sql(
            "SELECT STDDEV_POP(latency) FROM metrics",
            &cat,
            AccuracyTarget::Epsilon(0.01),
        )
        .await
        .unwrap();
        let lowered_queries = vec![(
            "stddev".to_string(),
            "SELECT STDDEV_POP(latency) FROM metrics".to_string(),
            stddev_query,
        )];

        let results = run_post_asap(&lowered_queries);

        // A trivial keep_pre_asap winner must be filtered before it ever
        // becomes a flat `TargetReplacement` — there's no real replacement
        // to report for a target with no alternative at all.
        assert!(
            results.replacements.is_empty(),
            "a target whose only candidate is the trivial keep_pre_asap fallback \
             shouldn't produce a flat replacement entry: {:?}",
            results
                .replacements
                .iter()
                .map(|(name, r)| (name.clone(), r.strategy.clone()))
                .collect::<Vec<_>>()
        );
        assert_eq!(results.post_graphs.len(), 1);
        assert!(!results.post_graphs[0].1.nodes.is_empty());
    }
}
