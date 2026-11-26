//! Workload-driven search-space pruning: each rule leaves out candidates
//! no optimal plan needs, so the selected plan costs what it costs with
//! the rule switched off.
//!
//! Every test plans small workloads through `asap_planner --format json`
//! (the production stage pipeline) and reads its counts from that
//! document. A rule is switched off with
//! `--search-space-pruning <rule>=off`; the plan with every rule on must
//! cost what the plan with the rule off costs (to 1e-9, relative). The
//! expected candidate sets are computed here from the document's own
//! logical edges and physical candidates, not by the planner's pruning
//! code.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

use serde_json::{json, Value};

use super::asap_planner::TempFile;

/// Raw data retained, query-time results not retained.
fn raw_retained() -> Value {
    json!({"raw_data_retained": true, "query_time_results_retained": false})
}

/// One repeating PromQL query: `query` every `interval_ms` at phase
/// `phase_ms`, at relative error `max_error` (failure probability 0.01
/// over the whole result).
fn repeating(query: &str, interval_ms: u64, phase_ms: u64, max_error: f64) -> Value {
    json!({
        "query": query,
        "demand": {"fixed_interval_at": {"interval": interval_ms, "evaluation_phase": phase_ms}},
        "requirements": {"accuracy": {
            "max_error": max_error, "failure_probability": 0.01, "accuracy_scope": "whole_result"
        }},
        "predictability": {"predictable": {"known_at": null}}
    })
}

/// A PromQL workload over gauge `m` with `series` series sampled every
/// `sample_interval_ms`, values non-negative.
fn promql(queries: Vec<Value>, series: u64, sample_interval_ms: u64) -> Value {
    json!({
        "query_workload": {"language": "promql", "repeating_queries": queries},
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": series as f64 * 1000.0 / sample_interval_ms as f64}},
            "sources": {"m": {
                "series_count": series, "metric_type": "gauge", "sample_interval": sample_interval_ms,
                "column_signs": {"value": "non_negative"}
            }}
        }
    })
}

/// `asap_planner --run-until stage3 --format json` over `workload` under
/// `capabilities`, with `args` and the environment `env`.
fn run(
    workload: &Value,
    capabilities: &Value,
    args: &[&str],
    env: &[(&str, &str)],
) -> std::process::Output {
    let workload_file = TempFile::json(workload);
    let capabilities_file = TempFile::json(capabilities);
    let mut command = Command::new(env!("CARGO_BIN_EXE_asap_planner"));
    command
        .args([
            "--workload",
            workload_file.path(),
            "--deployment-capabilities",
            capabilities_file.path(),
            "--run-until",
            "stage3",
            "--format",
            "json",
        ])
        .args(args)
        .envs(env.iter().copied());
    command.output().unwrap()
}

/// The stage document of `workload` planned to Stage 3 under
/// `capabilities`, with each rule of `off` switched off and the
/// environment `env`. Panics when the run fails or Stage 3 selects no
/// plan.
fn plan_with(workload: &Value, capabilities: &Value, off: &[&str], env: &[(&str, &str)]) -> Value {
    let settings: Vec<String> = off.iter().map(|rule| format!("{rule}=off")).collect();
    let args: Vec<&str> = settings
        .iter()
        .flat_map(|setting| ["--search-space-pruning", setting.as_str()])
        .collect();
    let output = run(workload, capabilities, &args, env);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "asap_planner with {off:?} off failed: {stderr}"
    );
    let document: Value =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| panic!("{error}: {stderr}"));
    assert!(
        document["stage3_plan_selection"]["error"].is_null(),
        "no plan: {}",
        document["stage3_plan_selection"]["error"]
    );
    document
}

/// [`plan_with`] in the default environment.
fn plan(workload: &Value, capabilities: &Value, off: &[&str]) -> Value {
    plan_with(workload, capabilities, off, &[])
}

/// The selected plan's cost per second.
fn cost(document: &Value) -> f64 {
    document["stage3_plan_selection"]["cost"]["cost_per_second"]
        .as_f64()
        .expect("a costed plan")
}

/// The plans cost the same, to 1e-9 relative.
fn assert_same_cost(pruned: &Value, unpruned: &Value, rule: &str) {
    let (a, b) = (cost(pruned), cost(unpruned));
    assert!(
        (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1e-300),
        "{rule}: the plan with the rule costs {a:e}/s, without it {b:e}/s"
    );
}

/// Pass 2's memo groups.
fn pass2_groups(document: &Value) -> &Vec<Value> {
    document["memo"]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["step"] == "pass2")
        .expect("a pass2 step")["groups"]
        .as_array()
        .unwrap()
}

/// A candidate by (group, index in group).
type CandidateKey = (u64, u64);

/// Pass 2's candidates that Pass 2 did not eliminate, by key.
fn live_candidates(document: &Value) -> BTreeMap<CandidateKey, &Value> {
    pass2_groups(document)
        .iter()
        .flat_map(|group| {
            let id = group["id"].as_u64().unwrap();
            group["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|candidate| candidate["pruned"] != json!(true))
                .map(move |candidate| ((id, candidate["index"].as_u64().unwrap()), candidate))
        })
        .collect()
}

/// The number of live θ candidates.
fn live_theta_count(document: &Value) -> usize {
    live_candidates(document)
        .values()
        .filter(|candidate| !candidate["parameters"].is_null())
        .count()
}

/// Stage 2's physical candidates.
fn physical_candidates(document: &Value) -> &Vec<Value> {
    document["stage2_physical_asap_optimization"]["physical_candidates"]
        .as_array()
        .unwrap()
}

/// A physical candidate's lifetime name (`None`, `OneEvaluation`,
/// `AcrossEvaluations`).
fn lifetime(physical: &Value) -> &str {
    let lifetime = &physical["properties"]["lifetime"];
    lifetime
        .as_str()
        .unwrap_or_else(|| lifetime.as_object().unwrap().keys().next().unwrap())
}

// The switches.

/// `all=off` is accepted; a default run's document has no
/// `search_space_pruning_off`; the plan costs what it costs with every rule
/// on.
#[test]
fn every_rule_off_is_recorded_and_costs_the_same() {
    let workload = promql(
        vec![repeating(
            "quantile_over_time(0.99, m[5m])",
            60_000,
            0,
            0.05,
        )],
        100,
        1_000,
    );
    let pruned = plan(&workload, &raw_retained(), &[]);
    let unpruned = plan(&workload, &raw_retained(), &["all"]);
    assert!(pruned["search_space_pruning_off"].is_null());
    assert_same_cost(&pruned, &unpruned, "all");
}

/// An unknown rule, or a setting that is neither `on` nor `off`, is a
/// usage error (status 2) naming the flag.
#[test]
fn an_unknown_rule_or_setting_is_a_usage_error() {
    let workload = promql(
        vec![repeating(
            "quantile_over_time(0.99, m[5m])",
            60_000,
            0,
            0.05,
        )],
        100,
        1_000,
    );
    for setting in ["no_such_rule=off", "all=maybe", "all"] {
        let output = run(
            &workload,
            &raw_retained(),
            &["--search-space-pruning", setting],
            &[],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{setting}: {stderr}");
        assert!(
            stderr.contains("--search-space-pruning"),
            "{setting}: {stderr}"
        );
    }
}

/// Stage 3 reports the greedy feasible plan's cost, which bounds the
/// optimum from above, and with `ASAP_STAGE3_ROOT_LP=1` the LP
/// relaxation's, which bounds it from below; without it the LP bound is
/// null.
#[test]
fn stage3_reports_the_greedy_and_lp_bounds() {
    let workload = promql(
        vec![repeating(
            "quantile_over_time(0.99, m[5m])",
            60_000,
            0,
            0.05,
        )],
        100,
        1_000,
    );
    let field = |document: &Value, name: &str| document["stage3_plan_selection"][name].clone();
    let plain = plan(&workload, &raw_retained(), &[]);
    assert!(field(&plain, "milp_lp_bound_cost_per_second").is_null());
    let reported = plan_with(
        &workload,
        &raw_retained(),
        &[],
        &[("ASAP_STAGE3_ROOT_LP", "1")],
    );
    let optimum = cost(&reported);
    let upper = field(&reported, "milp_upper_bound_cost_per_second")
        .as_f64()
        .expect("a greedy bound");
    let lower = field(&reported, "milp_lp_bound_cost_per_second")
        .as_f64()
        .expect("an LP bound");
    assert!(
        upper >= optimum * (1.0 - 1e-9),
        "upper {upper:e} < optimum {optimum:e}"
    );
    assert!(
        lower <= optimum * (1.0 + 1e-9),
        "lower {lower:e} > optimum {optimum:e}"
    );
    assert_same_cost(&plain, &reported, "bounds");
}

// Rule 1: threshold θ only (Pass 2).

/// Plans `workload` with rule 1 on and off: the rule leaves out θ
/// candidates, and the plans cost the same.
fn assert_rule1_keeps_the_optimum(workload: &Value) {
    let pruned = plan(workload, &raw_retained(), &[]);
    let unpruned = plan(workload, &raw_retained(), &["threshold_theta"]);
    assert!(
        live_theta_count(&pruned) < live_theta_count(&unpruned),
        "the rule leaves θ candidates out: {} with it, {} without",
        live_theta_count(&pruned),
        live_theta_count(&unpruned)
    );
    assert_same_cost(&pruned, &unpruned, "threshold_theta");
}

/// `quantile_over_time(0.99, m[5m])` every 60 s at relative 0.05 over 100
/// series: the DDSketch θs a larger α meets as well are left out (KLL keeps
/// every k: a larger k can cost less), at the same cost.
#[test]
fn rule1_one_quantile_keeps_the_optimum() {
    assert_rule1_keeps_the_optimum(&promql(
        vec![repeating(
            "quantile_over_time(0.99, m[5m])",
            60_000,
            0,
            0.05,
        )],
        100,
        1_000,
    ));
}

/// A strict/loose pair: `quantile_over_time(0.99, m[1h])` at
/// relative 0.05 every 15 s and at 0.005 every 60 s, 10,000 series sampled
/// every 100 ms: the two readers share chains whose θ must meet both.
#[test]
fn rule1_two_readers_of_one_chain_keep_the_optimum() {
    assert_rule1_keeps_the_optimum(&promql(
        vec![
            repeating("quantile_over_time(0.99, m[1h])", 15_000, 0, 0.05),
            repeating("quantile_over_time(0.99, m[1h])", 60_000, 0, 0.005),
        ],
        10_000,
        100,
    ));
}

/// Three top-k readers of one `sum by (host)` with different k and errors
/// (k = 5 at 0.05, k = 10 at 0.01, k = 20 at 0.02), 1,000 series: a
/// `CountMinSketchWithHeap` state all three share needs a θ at least each one's in
/// every field (width, depth, heap), which may be no reader's threshold.
#[test]
fn rule1_three_readers_of_one_multi_parameter_state_keep_the_optimum() {
    assert_rule1_keeps_the_optimum(&promql(
        vec![
            repeating("topk(5, sum by (host) (m))", 60_000, 0, 0.05),
            repeating("topk(10, sum by (host) (m))", 60_000, 0, 0.01),
            repeating("topk(20, sum by (host) (m))", 60_000, 0, 0.02),
        ],
        1_000,
        1_000,
    ));
}

/// Two quantiles over different ranges and errors of one metric
/// (`[5m]` at 0.05, `[1h]` at 0.01, every 60 s): their states are built
/// from shared chain windows, so a θ set spans several groups.
#[test]
fn rule1_two_ranges_sharing_windows_keep_the_optimum() {
    assert_rule1_keeps_the_optimum(&promql(
        vec![
            repeating("quantile_over_time(0.99, m[5m])", 60_000, 0, 0.05),
            repeating("quantile_over_time(0.9, m[1h])", 60_000, 0, 0.01),
        ],
        1_000,
        1_000,
    ));
}

// Rule 4: candidates no context can read (Stage 2).

/// `quantile_over_time(0.9, m[5m])` every 60 s and every 30 s at phase
/// 15 s: Pass 2 adds a 60 s KLL chain level that only the 5 min cover over
/// the 15 s chain reads, and a cover reads its wider levels only from a
/// chain kept at ingestion time. Its query-time copies have no execution
/// in any context, so Stage 2 does not keep them.
#[test]
fn rule4_a_wider_level_only_a_cover_reads_is_kept_at_ingestion_time_only() {
    let workload = json!({
        "query_workload": {"language": "promql", "repeating_queries": [
            repeating("quantile_over_time(0.9, m[5m])", 60_000, 0, 0.05),
            repeating("quantile_over_time(0.9, m[5m])", 30_000, 15_000, 0.05),
        ]},
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 1000.0}},
            "sources": {"m": {"series_count": 1000, "metric_type": "gauge", "sample_interval": 1000}}
        }
    });
    let pruned = plan(&workload, &raw_retained(), &[]);
    let groups: BTreeMap<u64, &str> = pass2_groups(&pruned)
        .iter()
        .map(|group| {
            (
                group["id"].as_u64().unwrap(),
                group["label"].as_str().unwrap(),
            )
        })
        .collect();
    let wide_query_time: Vec<&Value> = physical_candidates(&pruned)
        .iter()
        .filter(|physical| {
            groups[&physical["group"].as_u64().unwrap()].contains("tumbling_window_width_ms=60000")
                && physical["properties"]["timing"] == "query_time"
        })
        .collect();
    assert_eq!(wide_query_time, Vec::<&Value>::new());
    let unpruned = plan(&workload, &raw_retained(), &["context_reachability"]);
    assert!(physical_candidates(&pruned).len() < physical_candidates(&unpruned).len());
    assert_same_cost(&pruned, &unpruned, "context_reachability");
}

// Rule 5: `OneEvaluation` only for a result two consumers may read in one
// evaluation (Stage 2).

/// The live candidates at most one reader can read: one live consumer
/// candidate's input over a satisfied edge, or one query output. Two
/// candidates of one consumer group count twice (readout copies of one
/// group may both be selected in one evaluation).
fn candidates_read_at_most_once(document: &Value) -> BTreeSet<CandidateKey> {
    let live = live_candidates(document);
    let mut reads: BTreeMap<CandidateKey, usize> = live.keys().map(|&key| (key, 0)).collect();
    for candidate in live.values() {
        for input in candidate["inputs"].as_array().into_iter().flatten() {
            let group = input["group"].as_u64().unwrap();
            for edge in input["edges"].as_array().unwrap() {
                let producer = (group, edge["producer"].as_u64().unwrap());
                if edge["satisfies_logical_requirement"] == json!(true) {
                    if let Some(count) = reads.get_mut(&producer) {
                        *count += 1;
                    }
                }
            }
        }
    }
    for group in pass2_groups(document) {
        if group["roots_of"]
            .as_array()
            .is_some_and(|roots| !roots.is_empty())
        {
            let id = group["id"].as_u64().unwrap();
            for (key, count) in reads.iter_mut() {
                if key.0 == id {
                    *count += 1;
                }
            }
        }
    }
    reads
        .into_iter()
        .filter(|&(_, count)| count <= 1)
        .map(|(key, _)| key)
        .collect()
}

/// The physical candidate's logical candidate.
fn logical_key(physical: &Value) -> CandidateKey {
    (
        physical["group"].as_u64().unwrap(),
        physical["index_in_group"].as_u64().unwrap(),
    )
}

/// A single quantile: every candidate has one reader, so no physical
/// candidate is kept for one evaluation; the plan costs the same as with
/// the twins.
#[test]
fn rule5_a_result_read_once_per_evaluation_gets_no_one_evaluation_twin() {
    let workload = promql(
        vec![repeating(
            "quantile_over_time(0.99, m[5m])",
            60_000,
            0,
            0.05,
        )],
        100,
        1_000,
    );
    let pruned = plan(&workload, &raw_retained(), &[]);
    let once = candidates_read_at_most_once(&pruned);
    let twins: Vec<&Value> = physical_candidates(&pruned)
        .iter()
        .filter(|physical| {
            lifetime(physical) == "OneEvaluation" && once.contains(&logical_key(physical))
        })
        .collect();
    assert_eq!(twins, Vec::<&Value>::new());
    let unpruned = plan(&workload, &raw_retained(), &["one_evaluation_by_readers"]);
    assert_same_cost(&pruned, &unpruned, "one_evaluation_by_readers");
}

/// `sum(rate(m[5m])) / count(rate(m[5m]))`: one `rate` group is read by
/// both aggregates in each evaluation, so its query-time candidates keep
/// their `OneEvaluation` twins (computed once, read twice).
#[test]
fn rule5_a_result_two_consumers_read_keeps_its_one_evaluation_twin() {
    let workload = promql(
        vec![repeating(
            "sum(rate(m[5m])) / count(rate(m[5m]))",
            60_000,
            0,
            0.05,
        )],
        100,
        1_000,
    );
    let pruned = plan(&workload, &raw_retained(), &[]);
    let once = candidates_read_at_most_once(&pruned);
    let shared_twins = physical_candidates(&pruned)
        .iter()
        .filter(|physical| {
            lifetime(physical) == "OneEvaluation" && !once.contains(&logical_key(physical))
        })
        .count();
    assert!(
        shared_twins > 0,
        "a group read twice keeps its OneEvaluation twins"
    );
    let unpruned = plan(&workload, &raw_retained(), &["one_evaluation_by_readers"]);
    assert_same_cost(&pruned, &unpruned, "one_evaluation_by_readers");
}

// Rule 8a: identical candidates (fixed on main; a regression guard).

/// No group holds two live candidates with the same operator, inputs, θ
/// and input requirements (8a; on main since the 11-14 count of 174).
#[test]
fn rule8_no_group_holds_two_identical_candidates() {
    let workload = promql(
        vec![
            repeating(
                "avg by (job) (quantile_over_time(0.95, m[100s]))",
                60_000,
                0,
                0.05,
            ),
            repeating(
                "avg by (job) (quantile_over_time(0.90, m[100s]))",
                60_000,
                0,
                0.05,
            ),
            repeating(
                "avg by (job, host) (quantile_over_time(0.90, m[100s]))",
                60_000,
                0,
                0.05,
            ),
        ],
        100,
        1_000,
    );
    let document = plan(&workload, &raw_retained(), &[]);
    let mut seen: BTreeMap<(u64, String), u64> = BTreeMap::new();
    let mut duplicates = Vec::new();
    for ((group, index), candidate) in live_candidates(&document) {
        let key = (
            group,
            json!([
                candidate["operator"],
                candidate["children"],
                candidate["parameters"],
                candidate["required_accuracy"]
            ])
            .to_string(),
        );
        if let Some(first) = seen.insert(key, index) {
            duplicates.push(format!("G{group}[{first}] = G{group}[{index}]"));
        }
    }
    assert_eq!(duplicates, Vec::<String>::new());
}

// Rule 8b: logical candidates no physical form can build.

/// `topk(5, sum by (host) (m))` over a gauge whose sign is not declared:
/// a Count-Min build over weights not proven non-negative is unusable in
/// every physical form, and whether the weights are proven is a logical
/// fact, so Pass 1 lists no such θ (8b; Stage 2 pruned 280 `CountMinSketchWithHeap`
/// θs and the readout above them before). Every live logical candidate
/// then has a physical candidate.
#[test]
fn rule8b_a_count_min_build_over_unproven_weights_is_not_a_logical_candidate() {
    let workload = json!({
        "query_workload": {"language": "promql", "repeating_queries": [
            repeating("topk(5, sum by (host) (m))", 60_000, 0, 0.05),
        ]},
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 1000.0}},
            "sources": {"m": {"series_count": 1000, "metric_type": "gauge", "sample_interval": 1000}}
        }
    });
    let document = plan(&workload, &raw_retained(), &[]);
    let physical: BTreeSet<CandidateKey> = physical_candidates(&document)
        .iter()
        .map(|p| {
            (
                p["group"].as_u64().unwrap(),
                p["index_in_group"].as_u64().unwrap(),
            )
        })
        .collect();
    let without_physical: Vec<String> = live_candidates(&document)
        .iter()
        .filter(|(key, _)| !physical.contains(key))
        .map(|((group, index), candidate)| {
            format!(
                "G{group}[{index}] {} θ={}",
                candidate["label"], candidate["parameters"]
            )
        })
        .collect();
    assert_eq!(without_physical, Vec::<String>::new());
    // The selected plan is the one Stage 2 left when it pruned those θs.
    let expected = 0.0464580586986;
    assert!(
        (cost(&document) - expected).abs() <= 1e-9 * expected,
        "plan cost {} (expected {expected})",
        cost(&document)
    );
}
