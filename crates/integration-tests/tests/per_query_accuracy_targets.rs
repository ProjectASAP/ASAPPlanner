//! Each query meets its own accuracy target when several queries read one
//! sketch readout group at different targets (`accuracy-models.md`,
//! "Checking each query's target in selection: constraint (K)").
//!
//! Pass 2 gives a readout group read by several queries one readout
//! candidate per requirement level (`readout_requirements.rs`): for
//! `quantile_over_time(0.99, latency[1h])` asked at rank 0.05 and at rank
//! 0.001, one readout requires 0.05 and its copy 0.001, each reading only
//! the KLL states that meet its level. Both queries root at the same group,
//! so only Stage 3's per-query root check of (K) ties the 0.001 query to
//! the 0.001 copy: without it, the plan reads one KLL with k = 52 for both.
//!
//! The check here is independent of the planner's: from each query's root
//! in the selected plan, every sketch readout on its path, with the θ of
//! the state it reads, must have a theoretical guarantee within that
//! query's target.

use asap_accuracy::{theoretical_guarantee, tolerated_limit};
use asap_devtools::planning_stages::{lower_promql_roots, run_planning_stages_over};
use asap_planner::{DeploymentCapabilities, PlanningModels};
use asap_types::accuracy::AccuracyTarget;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator};
use asap_types::plan::physical_asap_dag::{PhysicalASAPDAG, PhysicalASAPNodeId};
use asap_types::stages::ValueSign;
use asap_types::summary_type::SketchParams;
use asap_types::workload::PlanningWorkload;

/// PromQL `queries`, each `(query, accuracy JSON)` every 60 s, over one
/// series of `latency`, a gauge sampled at 10 Hz whose values are declared
/// non-negative (so DDSketch's quantile guarantee holds over it).
fn workload(queries: &[(&str, &str)]) -> PlanningWorkload {
    let entries: Vec<String> = queries
        .iter()
        .map(|(query, accuracy)| {
            format!(
                r#"{{"query": {query:?}, "demand": {{"fixed_interval_at": {{"interval": 60000, "evaluation_phase": 0}}}},
                  "requirements": {{"accuracy": {accuracy}}}, "predictability": {{"predictable": {{"known_at": null}}}}}}"#
            )
        })
        .collect();
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 10.0}}}},
              "sources": {{"latency": {{"series_count": 1, "metric_type": "gauge", "sample_interval": 100,
                "column_signs": {{"value": "non_negative"}}}}}}}}}}"#,
        entries.join(",")
    ))
    .unwrap()
}

/// The plan the production stages select for `queries`, no raw data kept
/// (every query reads summaries built at ingestion or exact states).
fn plan(queries: &[(&str, &str)]) -> PhysicalASAPDAG {
    let workload = workload(queries);
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap();
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..Default::default()
    };
    let models = PlanningModels::builtin().with_deployment_capabilities(&capabilities);
    let roots = lower_promql_roots(&workload).unwrap();
    let run = run_planning_stages_over(roots, &entries, &data, models).unwrap();
    let (plan, _) = run.run.selection.as_ref().expect("a plan is selected");
    plan.clone()
}

/// Whether the readouts query `query`'s output reads in `plan` meet
/// `target`, each alone: every `SummaryEstimate` on the path below its
/// root reads a state whose θ has a theoretical guarantee, at the target's
/// failure probability, in a metric the target allows and within its
/// error. One series and one readout per path, so the target's δ is the
/// readout value's. `Err` says which readout misses and why.
fn query_meets_its_target(
    plan: &PhysicalASAPDAG,
    query: usize,
    target: &AccuracyTarget,
) -> Result<(), String> {
    let node = |id: PhysicalASAPNodeId| plan.nodes.iter().find(|n| n.id == id).unwrap();
    let mut pending = vec![plan.roots[query]];
    let mut readouts = 0;
    while let Some(id) = pending.pop() {
        pending.extend(
            plan.edges
                .iter()
                .filter(|edge| edge.consumer == id)
                .map(|edge| edge.producer),
        );
        let Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input,
            query: statistic,
        }) = &node(id).payload
        else {
            continue;
        };
        readouts += 1;
        let state = node(*summary_input);
        let Some(parameters) = &state.parameters else {
            continue;
        };
        let summary_type = state
            .output_schema
            .fields
            .iter()
            .map(|field| field.dtype.clone())
            .find(|dtype| matches!(dtype, FieldDataType::Sketch(_)))
            .expect("a sketch state has a sketch column");
        let failure_probability = target.failure_probability.unwrap_or(1.0);
        let guarantee = theoretical_guarantee(
            &summary_type,
            parameters,
            statistic,
            failure_probability,
            Some(ValueSign::NonNegative),
            None,
        )
        .ok_or(format!(
            "{parameters:?} has no guarantee at δ = {failure_probability}"
        ))?;
        let error = guarantee.spec.error.evaluate().unwrap();
        if !target.metrics().contains(&guarantee.spec.accuracy_metric)
            || error > tolerated_limit(target.max_error)
        {
            return Err(format!(
                "{parameters:?} guarantees {:?} {error}, the target is {target:?}",
                guarantee.spec.accuracy_metric
            ));
        }
    }
    assert!(readouts <= 1, "one readout per query path here");
    Ok(())
}

/// Plans `queries` and checks each query's own target on the plan.
fn assert_each_query_meets_its_target(queries: &[(&str, &str)]) -> PhysicalASAPDAG {
    let plan = plan(queries);
    for (index, (query, accuracy)) in queries.iter().enumerate() {
        let target: AccuracyTarget = serde_json::from_str(accuracy).unwrap();
        if let Err(reason) = query_meets_its_target(&plan, index, &target) {
            panic!("query {index} ({query} at {accuracy}) misses its target: {reason}\nqueries: {queries:?}");
        }
    }
    plan
}

const P99: &str = "quantile_over_time(0.99, latency[1h])";
const RANK_0_05: &str =
    r#"{"accuracy_metric": "rank", "max_error": 0.05, "failure_probability": 0.01}"#;
const RANK_0_001: &str =
    r#"{"accuracy_metric": "rank", "max_error": 0.001, "failure_probability": 0.01}"#;

/// The p99 at rank 0.05 and at rank 0.001, in both orders: the plan reads
/// a KLL meeting 0.001 for the strict query (k = 2,863 on the built-in
/// grid; k = 52 meets only 0.05).
#[test]
fn one_query_text_at_two_rank_targets_reads_a_kll_meeting_the_stricter() {
    for queries in [
        [(P99, RANK_0_05), (P99, RANK_0_001)],
        [(P99, RANK_0_001), (P99, RANK_0_05)],
    ] {
        let plan = assert_each_query_meets_its_target(&queries);
        assert!(
            plan.nodes
                .iter()
                .any(|n| matches!(n.parameters, Some(SketchParams::Kll { k }) if k >= 2_863)),
            "{:?}",
            plan.nodes.iter().map(|n| &n.parameters).collect::<Vec<_>>()
        );
    }
}

/// The L2 norm of one series over 24 h at relative 0.1 and at relative
/// 0.05: the plan reads a UnivMon meeting 0.05 for the strict query.
#[test]
fn one_query_text_at_two_relative_targets_reads_a_univmon_meeting_the_stricter() {
    let l2 = "l2_over_time(latency[24h])";
    let loose = r#"{"accuracy_metric": "relative", "max_error": 0.1, "failure_probability": 0.1}"#;
    let strict =
        r#"{"accuracy_metric": "relative", "max_error": 0.05, "failure_probability": 0.1}"#;
    for queries in [[(l2, loose), (l2, strict)], [(l2, strict), (l2, loose)]] {
        assert_each_query_meets_its_target(&queries);
    }
}

/// Two query texts sharing the p99's readout group: the bare p99 at rank
/// 0.05 and `sort` of it at rank 0.001. Their roots differ, but both
/// read the one readout group, whose copies carry both levels.
#[test]
fn two_query_texts_sharing_a_readout_group_each_meet_their_target() {
    let sorted_p99 = "sort(quantile_over_time(0.99, latency[1h]))";
    assert_each_query_meets_its_target(&[(P99, RANK_0_05), (sorted_p99, RANK_0_001)]);
    assert_each_query_meets_its_target(&[(sorted_p99, RANK_0_05), (P99, RANK_0_001)]);
}

/// Property: for any set of queries over shared readout groups, each with
/// its own target, every query's own check holds on the selected plan.
/// The sets are drawn with a fixed-seed generator from queries reading the
/// p99 (KLL for rank, DDSketch for relative), the p50 and the L2 norm, at
/// targets from loose to strict.
#[test]
fn every_query_meets_its_own_target_on_the_selected_plan() {
    const POOL: [(&str, &str); 9] = [
        (P99, RANK_0_05),
        (P99, RANK_0_001),
        (
            P99,
            r#"{"accuracy_metric": "rank", "max_error": 0.01, "failure_probability": 0.01}"#,
        ),
        ("sort(quantile_over_time(0.99, latency[1h]))", RANK_0_001),
        (
            P99,
            r#"{"accuracy_metric": "relative", "max_error": 0.05, "failure_probability": 0.01}"#,
        ),
        (
            P99,
            r#"{"accuracy_metric": "relative", "max_error": 0.01, "failure_probability": 0.01}"#,
        ),
        ("quantile_over_time(0.5, latency[1h])", RANK_0_05),
        (
            "l2_over_time(latency[1h])",
            r#"{"accuracy_metric": "relative", "max_error": 0.1, "failure_probability": 0.1}"#,
        ),
        (
            "l2_over_time(latency[1h])",
            r#"{"accuracy_metric": "relative", "max_error": 0.05, "failure_probability": 0.1}"#,
        ),
    ];
    // A 64-bit linear congruential generator (Knuth's MMIX constants).
    let mut state: u64 = 0x5eed;
    let mut next = |bound: usize| {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((state >> 33) % bound as u64) as usize
    };
    for _ in 0..16 {
        let size = 2 + next(2);
        let queries: Vec<(&str, &str)> = (0..size).map(|_| POOL[next(POOL.len())]).collect();
        assert_each_query_meets_its_target(&queries);
    }
}
