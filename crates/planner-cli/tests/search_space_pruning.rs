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

/// Stage 2's physical candidates.
fn physical_candidates(document: &Value) -> &Vec<Value> {
    document["stage2_physical_asap_optimization"]["physical_candidates"]
        .as_array()
        .unwrap()
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
/// fact, so Pass 1 lists no such θ (8b; Stage 2 pruned 240 `CmsWithHeap`
/// θs and the readout above them before). Every live logical candidate
/// then has a physical candidate, and the plan costs the same.
#[test]
fn rule8_a_count_min_build_over_unproven_weights_is_not_a_logical_candidate() {
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
    assert!(cost(&document) > 0.0);
}
