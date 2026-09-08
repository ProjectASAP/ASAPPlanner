//! Fixed-snapshot exact summary profiling for a deliberately bounded o11y subset.
use std::{hint::black_box, rc::Rc};

use asap_aware_mapping::cost_model::Cost;
use asap_aware_mapping::{
    default_strategies_with, search_workload_with_targets, CostModel, DefaultAccuracyModel,
    DefaultCostModel, Replacement, ReplacementStrategy, ReplacementSubDAG, SketchAlgorithmStrategy,
    TargetSubDAG,
};
use asap_devtools::lower_promql;
use asap_types::{
    post_asap::SketchAlgorithm,
    pre_asap::{AggIntent, QueryExpr},
    types::AccuracyTarget,
};
use serde_json::{json, Value};

const CORPUS: &str =
    include_str!("../../../frontend-promql/tests/observability/data/o11y_bench_promql.txt");

#[derive(Clone, Copy, Debug)]
enum Kernel {
    SumInstant,
    MaxWindow,
    SumAverageWindow,
}

struct Fixture {
    query: &'static str,
    kernel: Kernel,
    range_seconds: u64,
    job_matcher: Option<regex::Regex>,
}

// Admit pinned fixture queries whose full value computation is implemented.
// Regex construction belongs to planning, not execution, on both paths.
fn fixture(query: &'static str) -> Option<Fixture> {
    let (kernel, range_seconds, matcher) = match query {
        "sum(process_resident_memory_bytes)" | "sum(up)" => (Kernel::SumInstant, 0, None),
        "max_over_time(service_cache_refresh_lag_seconds{job=\"user-service\"}[12h])" => {
            (Kernel::MaxWindow, 43200, Some("^user-service$"))
        }
        "max_over_time(service_retry_queue_depth{job=\"order-service\"}[6h])" => {
            (Kernel::MaxWindow, 21600, Some("^order-service$"))
        }
        "max_over_time(service_retry_queue_depth{job=\"payment-service\"}[6h])" => {
            (Kernel::MaxWindow, 21600, Some("^payment-service$"))
        }
        "max_over_time(service_retry_queue_depth{job=~\".+\"}[6h])" => {
            (Kernel::MaxWindow, 21600, Some("^.+$"))
        }
        "sum(avg_over_time(process_resident_memory_bytes{job=~\".+\"}[6h]))" => {
            (Kernel::SumAverageWindow, 21600, Some("^.+$"))
        }
        _ => return None,
    };
    Some(Fixture {
        query,
        kernel,
        range_seconds,
        job_matcher: matcher.map(|m| regex::Regex::new(m).unwrap()),
    })
}

struct Series {
    job: &'static str,
    samples: Vec<f64>,
}

fn data(fixture: &Fixture, series: usize) -> Vec<Series> {
    let samples = if fixture.range_seconds == 0 {
        1
    } else {
        fixture.range_seconds as usize / 60
    };
    (0..series)
        .map(|s| Series {
            job: ["user-service", "order-service", "payment-service"][s % 3],
            samples: (0..samples)
                .map(|t| {
                    if fixture.query == "sum(up)" {
                        if s % 11 == 0 {
                            0.0
                        } else {
                            1.0
                        }
                    } else {
                        ((s * 73 + t * 31 + (s * t) % 17) % 10000) as f64 / 100.0
                    }
                })
                .collect(),
        })
        .collect()
}

fn raw(fixture: &Fixture, data: &[Series]) -> Vec<f64> {
    let selected = data.iter().filter(|s| {
        fixture
            .job_matcher
            .as_ref()
            .is_none_or(|r| r.is_match(s.job))
    });
    match fixture.kernel {
        Kernel::SumInstant => vec![selected.map(|s| s.samples[0]).sum()],
        Kernel::MaxWindow => selected
            .map(|s| s.samples.iter().copied().fold(f64::NEG_INFINITY, f64::max))
            .collect(),
        Kernel::SumAverageWindow => vec![selected
            .map(|s| s.samples.iter().sum::<f64>() / s.samples.len() as f64)
            .sum()],
    }
}

#[cfg(unix)]
fn cpu_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // A process CPU clock excludes scheduling waits; no wall-time conversion.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) },
        0
    );
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn measure(mut op: impl FnMut(), iterations: usize) -> Value {
    for _ in 0..2 {
        op();
    }
    let samples: Vec<_> = (0..5)
        .map(|_| {
            let start = cpu_ns();
            for _ in 0..iterations {
                op();
            }
            (cpu_ns() - start) as f64 / iterations as f64
        })
        .collect();
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let stddev = (samples.iter().map(|x| (x - mean).powi(2)).sum::<f64>()
        / (samples.len() - 1) as f64)
        .sqrt();
    json!({"mean_cpu_ns":mean, "stddev_cpu_ns":stddev, "samples_cpu_ns":samples, "iterations_per_sample":iterations})
}

// This reference deployment admits exactly the root it profiled. CPU values
// apply to the complete bounded in-memory kernel, never an interior group.
struct ProfiledModel<'a> {
    root: &'a QueryExpr,
    approved: Vec<Rc<asap_types::post_asap::SummaryNode>>,
    raw_cpu: f64,
    candidate_cpu: f64,
}

fn approved_profiles(root: &Rc<QueryExpr>) -> Vec<Rc<asap_types::post_asap::SummaryNode>> {
    SketchAlgorithmStrategy::new(&DefaultCostModel)
        .replacements(&TargetSubDAG::new(root))
        .into_iter()
        .filter_map(|candidate| match candidate.replacement {
            Replacement::Summary(node)
                if matches!(
                    node.expr,
                    asap_types::post_asap::SummaryExpr::SummaryAgg { .. }
                ) && node.guarantee.as_ref().is_some_and(|g| g.is_exact()) =>
            {
                Some(node)
            }
            _ => None,
        })
        .collect()
}

impl CostModel for ProfiledModel<'_> {
    fn rank_candidates(
        &self,
        intent: &AggIntent,
        candidates: &[SketchAlgorithm],
    ) -> Vec<SketchAlgorithm> {
        DefaultCostModel.rank_candidates(intent, candidates)
    }
    fn candidate_cost_covers_complete_plan(&self) -> bool {
        true
    }
    fn candidate_cost(
        &self,
        candidate: &ReplacementSubDAG,
        target: &TargetSubDAG<'_>,
    ) -> Option<Cost> {
        let Replacement::Summary(node) = &candidate.replacement else {
            return None;
        };
        if target.root.as_ref() != self.root || !self.approved.contains(node) {
            return None;
        }
        (self.candidate_cpu < self.raw_cpu).then_some(Cost(self.candidate_cpu))
    }
    fn estimate_cost(&self, candidate: &ReplacementSubDAG, target: &TargetSubDAG<'_>) -> f64 {
        self.candidate_cost(candidate, target)
            .map_or(f64::INFINITY, |c| c.0)
    }
}

fn run(fixture: &Fixture, series: usize, evaluations: usize) -> Value {
    let root = Rc::new(lower_promql(fixture.query, AccuracyTarget::Exact).unwrap());
    let data = data(fixture, series);
    let cached = raw(fixture, &data);
    // Same ordered per-series output, or the same scalar; immutable fixed
    // windows, no staleness, NaNs, counter resets, or advancing evaluation time.
    assert_eq!(raw(fixture, &data), cached.clone());
    let raw_measurement = measure(
        || {
            black_box(raw(black_box(fixture), black_box(&data)));
        },
        25,
    );
    let read_measurement = measure(
        || {
            black_box(black_box(&cached).clone());
        },
        10000,
    );
    let raw_per_eval = raw_measurement["mean_cpu_ns"].as_f64().unwrap();
    let read_per_eval = read_measurement["mean_cpu_ns"].as_f64().unwrap();
    // Building a fixed-window summary executes the identical raw kernel once.
    // Both timers include output allocation and destruction; one-time build
    // therefore conservatively includes a destruction absent during retention.
    let raw_total = raw_per_eval * evaluations as f64;
    let summary_total = raw_per_eval + read_per_eval * evaluations as f64;
    let model = ProfiledModel {
        root: &root,
        approved: approved_profiles(&root),
        raw_cpu: raw_total,
        candidate_cpu: summary_total,
    };
    let candidates = SketchAlgorithmStrategy::new(&model).replacements(&TargetSubDAG::new(&root));
    let accepted = candidates
        .iter()
        .filter(|c| model.candidate_cost(c, &TargetSubDAG::new(&root)).is_some())
        .count();
    let space = search_workload_with_targets(
        vec![(0, root.clone(), Some(AccuracyTarget::Exact))],
        &default_strategies_with(&model),
        &DefaultAccuracyModel,
    );
    let selection = space.global_selection(&model);
    let selected_root = &space.roots[0].1;
    let selected_plan = selection.materialize(selected_root).unwrap();
    let selected_summary = selected_plan
        .as_ref()
        .is_some_and(|node| model.approved.contains(node));
    let retained_value_bytes = cached.len() * std::mem::size_of::<f64>();
    json!({"query":fixture.query,"status":"profiled_fixed_snapshot", "kernel":format!("{:?}",fixture.kernel),
        "profile_model_version":"fixed-snapshot-exact-values-v1", "planner_candidate_count":candidates.len(),
        "accepted_measured_candidates":accepted,
        "selected":if selected_summary {"retained_exact_summary"} else {"raw_recompute"},
        "selected_planner_graph":selected_plan.as_ref().map(|node|asap_types::dag_export::export_summary(node)),
        "input_series":series,"selected_series":data.iter().filter(|s|fixture.job_matcher.as_ref().is_none_or(|r|r.is_match(s.job))).count(),
        "samples_per_series":data[0].samples.len(),"sample_interval_seconds":60,"range_seconds":fixture.range_seconds,
        "input_value_bytes":data.iter().map(|s|s.samples.len()*8).sum::<usize>(),
        "retained_value_bytes":retained_value_bytes,"retained_value_bytes_method":"exact Vec length times sizeof(f64), logical values only; label/input storage remains shared",
        "raw_per_evaluation":raw_measurement,"summary_read_per_evaluation":read_measurement,
        "evaluations":evaluations,"raw_cpu_ns":raw_total,"summary_cpu_ns":summary_total,
        "estimated_cpu_reduction_fraction":if selected_summary {Some(1.0-summary_total/raw_total)} else {None},
        "output_equality_verified":true,
        "comparison_scope":"same immutable metric snapshot and fixed evaluation time; retained whole-query exact result reference implementation including filtering, reduction, allocation and readout",
        "exclusions":["disk/network storage and protocol serialization", "label output materialization shared by both paths", "live updates, eviction, sliding windows, staleness and exceptional samples"]})
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let series: usize = args.first().map_or(Ok(300), |x| x.parse())?;
    let evaluations: usize = args.get(1).map_or(Ok(60), |x| x.parse())?;
    if args.len() > 2 || series == 0 || series > 10000 || evaluations == 0 {
        return Err("usage: o11y_exact_bench [SERIES(1..10000)] [EVALUATIONS>0]".into());
    }
    let rows: Vec<_> = CORPUS.lines().map(str::trim).filter(|q|!q.is_empty()&&!q.starts_with('#')).map(|q|
        fixture(q).map_or_else(||json!({"query":q,"status":"unavailable","reason":"no complete reference kernel for this query; no borrowed costs"}), |f|run(&f,series,evaluations))).collect();
    let cpu = std::fs::read_to_string("/proc/cpuinfo").ok().and_then(|text|
        text.lines().find(|line|line.starts_with("model name")).map(str::to_owned));
    let revision = std::process::Command::new("git").args(["rev-parse","HEAD"]).output().ok()
        .filter(|out|out.status.success()).map(|out|String::from_utf8_lossy(&out.stdout).trim().to_owned());
    serde_json::to_writer_pretty(
        std::io::stdout(),
        &json!({"schema_version":1,
        "data":"synthetic deterministic finite gauges; three jobs; non-stale finite float samples on (T-window,T] every60s",
        "implementation":"o11y_exact_bench Rust reference kernels; not production data-plane runtime",
        "build_profile":if cfg!(debug_assertions){"debug"}else{"release"},
        "source_revision":revision,"source_file":"crates/devtools/src/bin/o11y_exact_bench.rs",
        "command_arguments":args,"cpu":cpu,"os":std::env::consts::OS,"arch":std::env::consts::ARCH,
        "timing_environment":"shared development host; no exclusive core reservation or CPU affinity",
        "clock":"CLOCK_PROCESS_CPUTIME_ID","rows":rows}),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn admission_is_explicit_and_unknown_semantics_stay_unavailable() {
        assert_eq!(CORPUS.lines().filter(|q| fixture(q).is_some()).count(), 7);
        assert!(fixture("sum(rate(http_requests_total[5m]))").is_none());
    }
    #[test]
    fn kernels_match_independent_small_reference_results() {
        let input = vec![
            Series {
                job: "order-service",
                samples: vec![2.0, 4.0],
            },
            Series {
                job: "payment-service",
                samples: vec![8.0, 10.0],
            },
        ];
        let max =
            fixture("max_over_time(service_retry_queue_depth{job=\"order-service\"}[6h])").unwrap();
        assert_eq!(raw(&max, &input), vec![4.0]);
        let avg =
            fixture("sum(avg_over_time(process_resident_memory_bytes{job=~\".+\"}[6h]))").unwrap();
        assert_eq!(raw(&avg, &input), vec![12.0]);
    }
    #[test]
    fn measured_scope_and_break_even_control_candidate_acceptance() {
        let root = Rc::new(lower_promql("sum(up)", AccuracyTarget::Exact).unwrap());
        let target = TargetSubDAG::new(&root);
        let candidates = SketchAlgorithmStrategy::new(&DefaultCostModel).replacements(&target);
        assert!(!candidates.is_empty());
        let slow = ProfiledModel {
            root: &root,
            approved: approved_profiles(&root),
            raw_cpu: 100.0,
            candidate_cpu: 101.0,
        };
        let fast = ProfiledModel {
            root: &root,
            approved: approved_profiles(&root),
            raw_cpu: 100.0,
            candidate_cpu: 90.0,
        };
        assert!(slow.candidate_cost(&candidates[0], &target).is_none());
        assert_eq!(
            fast.candidate_cost(&candidates[0], &target),
            Some(Cost(90.0))
        );
        let other = Rc::new(lower_promql("sum(other)", AccuracyTarget::Exact).unwrap());
        assert!(fast
            .candidate_cost(&candidates[0], &TargetSubDAG::new(&other))
            .is_none());
        let mut unprofiled = candidates[0].clone();
        let mut node = fast.approved[0].as_ref().clone();
        node.expr = asap_types::post_asap::SummaryExpr::KeepPreAsap(root.clone());
        unprofiled.replacement = Replacement::Summary(Rc::new(node));
        assert!(fast.candidate_cost(&unprofiled, &target).is_none());
    }
}
