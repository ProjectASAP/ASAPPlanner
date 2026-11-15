//! §2.4: the accuracy calibration's aggregation, and the measured table
//! read by Pass 2 through the planner.

use std::collections::BTreeSet;

use asap_accuracy::{
    ErrorAtFailureProbability, MeasuredAccuracyEntry, MeasuredSketchAccuracy, NoMeasuredGuarantees,
};
use asap_cost::CostInputs;
use asap_devtools::measurement::accuracy_calibration::{
    error_quantile, worst_over_merge_counts, MergeCountErrors,
};
use asap_devtools::measurement::calibration_tables::{calibrate, CalibrateRequest};
use asap_devtools::measurement::cost_calibration::CalibrationFamilies;
use asap_devtools::planning_stages::{lower_promql_roots, run_planning_stages_over};
use asap_planner::{DeploymentCapabilities, PlanningModels, PlanningWorkload};
use asap_types::accuracy::AccuracyMetric;
use asap_types::ir::{ASAPOp, Operator};
use asap_types::summary_type::{SketchParams, SketchStatistic};

use super::common::{facts, per_value, plan, workload};

fn point(failure_probability: f64, error: f64) -> ErrorAtFailureProbability {
    ErrorAtFailureProbability {
        failure_probability,
        error,
    }
}

/// Rank errors of KLL k = 547 at δ = 0.01 after 1, 6 and 360 merges:
/// 0.0015, 0.0018, 0.0017. The table records the worst, 0.0018, not the
/// mean (0.001667) or the last (0.0017).
#[test]
fn worst_over_merge_counts_is_recorded() {
    let after = |merge_count, error| MergeCountErrors {
        sketch: SketchParams::Kll { k: 547 },
        statistic: SketchStatistic::Quantile { q: 0.99 },
        accuracy_metric: AccuracyMetric::Rank,
        merge_count,
        trials: 1_000,
        error_at: vec![point(0.01, error)],
    };
    let entries =
        worst_over_merge_counts(&[after(1, 0.0015), after(6, 0.0018), after(360, 0.0017)]);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].error_at, vec![point(0.01, 0.0018)]);
    assert_eq!(entries[0].trials, 1_000);
}

/// Errors i / 1e5 for i = 1..1000, given in descending order: the
/// nearest-rank 0.99 quantile is the ⌈0.99 · 1000⌉ = 990th smallest,
/// 0.0099.
// provisional answer 14
#[test]
fn quantile_of_errors_nearest_rank() {
    let errors: Vec<f64> = (1..=1000).rev().map(|i| i as f64 / 1e5).collect();
    assert_eq!(error_quantile(&errors, 0.01), 990.0 / 1e5);
}

/// `quantile_over_time(0.99, m[5m])` every 10 s over 8 series sampled each
/// second, within rank error 0.0025 per value at δ = 0.01.
fn kll_workload() -> PlanningWorkload {
    workload(
        &[(
            "quantile_over_time(0.99, m[5m])",
            per_value("rank", 0.0025, 0.01),
        )],
        10_000,
        &[("m", facts(8, 1_000))],
    )
}

/// Every KLL `k` Pass 2 connects to a readout through an edge that meets
/// its requirement.
fn admissible_kll_sizes(workload: &PlanningWorkload, models: PlanningModels<'_>) -> BTreeSet<u32> {
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap_or_default();
    let run = run_planning_stages_over(
        lower_promql_roots(workload).unwrap(),
        &entries,
        &data,
        models,
    )
    .unwrap();
    let logical = run.run.logical_asap_candidates.expect("Pass 2 ran");
    logical
        .edges
        .iter()
        .filter(|edge| edge.satisfies_logical_requirement)
        .filter(|edge| {
            matches!(
                logical
                    .memo
                    .candidate(edge.consumer_candidate_id)
                    .expression
                    .operator,
                Operator::ASAP(ASAPOp::SummaryEstimate { .. })
            )
        })
        .filter_map(|edge| {
            match logical
                .memo
                .candidate(edge.producer_candidate_id)
                .logical_properties
                .parameters()
            {
                Some(SketchParams::Kll { k }) => Some(*k),
                _ => None,
            }
        })
        .collect()
}

/// The KLL `k` of the selected plan's sketch, if it has one.
fn selected_kll_size(
    workload: &PlanningWorkload,
    evidence: &dyn asap_accuracy::AccuracyEvidence,
) -> Option<u32> {
    let (installable, _) = plan(
        workload,
        &CostInputs::default(),
        evidence,
        &DeploymentCapabilities::default(),
    );
    installable
        .dag
        .nodes
        .iter()
        .find_map(|node| match node.parameters.as_ref() {
            Some(SketchParams::Kll { k }) => Some(*k),
            _ => None,
        })
}

/// Without a table, the closed form sizes KLL: of the predefined sizes,
/// k = 547 (rank error 0.004998 at δ = 0.01) misses 0.0025 and the smallest
/// admissible is k = 1404 (0.00200).
#[test]
fn kll_sized_by_closed_form_without_measured_table() {
    let sizes = admissible_kll_sizes(&kll_workload(), PlanningModels::builtin());
    assert!(!sizes.contains(&547), "{sizes:?}");
    assert_eq!(sizes.first(), Some(&1_404), "{sizes:?}");
}

/// The fixture table: KLL k = 547 for the 0.99 quantile measured at rank
/// error 0.0024 at δ = 0.01 (within 0.0025).
fn kll_547_measured(metric: AccuracyMetric, k: u32) -> MeasuredAccuracyEntry {
    MeasuredAccuracyEntry {
        sketch: SketchParams::Kll { k },
        statistic: SketchStatistic::Quantile { q: 0.99 },
        accuracy_metric: metric,
        trials: 1_000,
        error_at: vec![
            point(0.003, 0.0031),
            point(0.01, 0.0024),
            point(0.05, 0.0016),
        ],
    }
}

/// With the table, k = 547 is admissible and, its state being smaller than
/// k = 1404's, selected.
#[test]
fn measured_accuracy_shrinks_sketch() {
    let table = MeasuredSketchAccuracy {
        entries: vec![kll_547_measured(AccuracyMetric::Rank, 547)],
    };
    let sizes = admissible_kll_sizes(&kll_workload(), PlanningModels::new(&table));
    assert!(sizes.contains(&547), "{sizes:?}");
    assert_eq!(selected_kll_size(&kll_workload(), &table), Some(547));
}

/// Entries for k = 547 and k = 1404 in the relative metric (not KLL's
/// rank) neither admit k = 547 nor prune k = 1404: the plan is the plan
/// without a table.
// provisional answer 7
#[test]
fn wrong_metric_entry_does_not_prune() {
    let table = MeasuredSketchAccuracy {
        entries: vec![
            kll_547_measured(AccuracyMetric::Relative, 547),
            kll_547_measured(AccuracyMetric::Relative, 1_404),
        ],
    };
    let with = admissible_kll_sizes(&kll_workload(), PlanningModels::new(&table));
    let without = admissible_kll_sizes(&kll_workload(), PlanningModels::builtin());
    assert_eq!(with, without);
    assert!(with.contains(&1_404), "{with:?}");
    let caps = DeploymentCapabilities::default();
    assert_eq!(
        plan(&kll_workload(), &CostInputs::default(), &table, &caps),
        plan(
            &kll_workload(),
            &CostInputs::default(),
            &NoMeasuredGuarantees,
            &caps
        ),
    );
}

/// An empty table plans exactly as no measurement: the same plan, cost
/// report and pruned candidates (I7).
#[test]
fn empty_table_equals_no_measured() {
    let workload = kll_workload();
    let empty = MeasuredSketchAccuracy::default();
    let caps = DeploymentCapabilities::default();
    assert_eq!(
        plan(&workload, &CostInputs::default(), &empty, &caps),
        plan(
            &workload,
            &CostInputs::default(),
            &NoMeasuredGuarantees,
            &caps
        ),
    );
    let pruned = |models: PlanningModels<'_>| {
        let entries: Vec<_> = workload.query_workload.entries().collect();
        let data = workload.data_workload.clone().unwrap_or_default();
        let run = run_planning_stages_over(
            lower_promql_roots(&workload).unwrap(),
            &entries,
            &data,
            models,
        )
        .unwrap();
        format!(
            "{:?} {:?}",
            run.run
                .logical_asap_candidates
                .map(|logical| logical.pruned),
            run.run
                .physical_asap_candidates
                .map(|physical| physical.pruned_candidates)
        )
    };
    assert_eq!(
        pruned(PlanningModels::new(&empty)),
        pruned(PlanningModels::builtin())
    );
}

/// A small calibration's accuracy table holds I4: per entry, δ ascending
/// in (0, 1] and ≥ 3 / trials, errors finite, non-negative and
/// non-increasing in δ, and each the worst of the per-merge-count results
/// at that δ.
// provisional answer 13
#[test]
fn accuracy_table_is_monotone() {
    let calibration = calibrate(&CalibrateRequest {
        families: CalibrationFamilies::All,
        small: true,
        seed: 7,
    })
    .unwrap();
    calibration.accuracy.validate().unwrap();
    for entry in &calibration.accuracy.entries {
        let floor = 3.0 / entry.trials as f64;
        for pair in entry.error_at.windows(2) {
            assert!(
                pair[0].failure_probability < pair[1].failure_probability,
                "{entry:?}"
            );
            assert!(pair[1].error <= pair[0].error, "{entry:?}");
        }
        for at in &entry.error_at {
            assert!(
                at.failure_probability >= floor && at.failure_probability <= 1.0,
                "{entry:?}"
            );
            assert!(at.error.is_finite() && at.error >= 0.0, "{entry:?}");
            let worst = calibration
                .accuracy_by_merge_count
                .iter()
                .filter(|raw| {
                    raw.sketch == entry.sketch
                        && raw.statistic == entry.statistic
                        && raw.accuracy_metric == entry.accuracy_metric
                })
                .flat_map(|raw| raw.error_at.iter())
                .filter(|raw| raw.failure_probability == at.failure_probability)
                .map(|raw| raw.error)
                .fold(f64::NEG_INFINITY, f64::max);
            assert_eq!(at.error, worst, "{entry:?}");
        }
    }
}

/// A full calibration's measurements lie within the sketches' known
/// bounds: DDSketch α = 0.01 never exceeds relative error 0.01 (a
/// deterministic bound); CMS (w, d) at δ ≥ e^−d plus a 99% binomial slack
/// has absolute error at most e / w of ‖f‖₁. KLL k = 547 at δ = 0.01
/// within 1.2 × 0.004998 is a sanity check that only warns.
#[test]
#[ignore = "timing: run on a dedicated host"]
fn measured_within_known_bounds() {
    let calibration = calibrate(&CalibrateRequest {
        families: CalibrationFamilies::All,
        small: false,
        seed: 7,
    })
    .unwrap();
    for raw in &calibration.accuracy_by_merge_count {
        match (&raw.sketch, raw.accuracy_metric) {
            (SketchParams::DDSketch { alpha }, AccuracyMetric::Relative) if *alpha == 0.01 => {
                for at in &raw.error_at {
                    assert!(at.error <= 0.01 * (1.0 + 1e-9), "{raw:?}");
                }
            }
            (SketchParams::Cms { width, depth }, metric)
                if metric == AccuracyMetric::ABSOLUTE_STREAM_L1 =>
            {
                let trials = raw.trials as f64;
                let failure = (-(*depth as f64)).exp();
                // One-sided 99% slack on a binomial fraction of `trials`.
                let slack = 2.326 * (failure * (1.0 - failure) / trials).sqrt();
                for at in raw
                    .error_at
                    .iter()
                    .filter(|at| at.failure_probability >= failure + slack)
                {
                    assert!(at.error <= std::f64::consts::E / *width as f64, "{raw:?}");
                }
            }
            (SketchParams::Kll { k: 547 }, AccuracyMetric::Rank) => {
                for at in raw
                    .error_at
                    .iter()
                    .filter(|at| at.failure_probability == 0.01)
                {
                    if at.error > 1.2 * 0.004998 {
                        eprintln!(
                            "warning: KLL k = 547 measured {} > 1.2 x 0.004998",
                            at.error
                        );
                    }
                }
            }
            _ => {}
        }
    }
}
