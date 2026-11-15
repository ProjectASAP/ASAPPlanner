//! §2.12 and §2.13: the `asap_run` binary end to end on a tiny workload,
//! the showcase suite, and determinism under a seed.

use std::process::Command;

use aqpbm_datagen::{DataDistribution, LabelSpec, TimeSeriesDescription, ZipfParameter};
use asap_devtools::measurement::benchmark::{bench_showcase, GateVerdict};
use asap_devtools::measurement::calibration_tables::{calibrate, CalibrateRequest, PlanningTables};
use asap_devtools::measurement::cost_calibration::CalibrationFamilies;
use asap_devtools::measurement::samples::{generate_samples, GeneratedDataSettings};
use asap_devtools::measurement::source_facts::{source_facts_of_samples, FactsOptions};

use super::common::{exact, four_constant_series, four_series_facts, workload, TempDir};

fn asap_run(args: &[&str], dir: &TempDir) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_asap_run"))
        .args(args)
        .env("ASAP_CALIBRATION_DIR", &dir.0)
        .current_dir(&dir.0)
        .output()
        .unwrap()
}

fn jsonl(samples: &[asap_devtools::measurement::samples::Sample]) -> String {
    samples
        .iter()
        .map(|sample| serde_json::to_string(sample).unwrap() + "\n")
        .collect()
}

/// `facts` on the hand-made JSONL, `calibrate --small --families exact`,
/// then `bench` on `sum by (job) (sum_over_time(m[1m]))`: the facts are the
/// data's, the costs validate, the bench answers 180 / 420, the gate
/// passes on answers (`--no-cost-gate`) and the run's files are written.
#[test]
fn e2e_tiny_hand_answers() {
    let dir = TempDir::new("e2e");
    let data = dir.write(
        "m.jsonl",
        &jsonl(&four_constant_series(600_000, |_, _| None)),
    );
    let plain = workload(
        &[("sum by (job) (sum_over_time(m[1m]))", exact())],
        10_000,
        &[],
    );
    let workload_path = dir.write("w.json", &serde_json::to_string(&plain).unwrap());
    let (data, workload_path) = (data.to_str().unwrap(), workload_path.to_str().unwrap());

    let facts = asap_run(
        &["facts", "--data", data, "--workload", workload_path],
        &dir,
    );
    assert!(
        facts.status.success(),
        "{}",
        String::from_utf8_lossy(&facts.stderr)
    );
    let with_facts: asap_planner::PlanningWorkload =
        serde_json::from_slice(&facts.stdout).expect("facts writes the workload");
    let m = &with_facts.data_workload.as_ref().unwrap().sources["m"];
    assert_eq!(m.series_count, Some(4));
    assert_eq!(m.sample_interval.map(|interval| interval.0), Some(1_000));

    let calibrated = asap_run(&["calibrate", "--small", "--families", "exact"], &dir);
    assert!(
        calibrated.status.success(),
        "{}",
        String::from_utf8_lossy(&calibrated.stderr)
    );
    let costs = std::fs::read_dir(&dir.0)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("costs.json"))
        .find(|path| path.is_file())
        .expect("calibrate wrote <machine-key>/costs.json");
    let table: asap_cost::MeasuredOperationCosts =
        serde_json::from_str(&std::fs::read_to_string(costs).unwrap()).unwrap();
    asap_cost::CostInputs {
        measured_operation_costs: table,
        ..asap_cost::CostInputs::default()
    }
    .validate()
    .unwrap();

    let facts_path = dir.write("w-facts.json", &serde_json::to_string(&with_facts).unwrap());
    let executed = asap_run(
        &[
            "execute",
            "--workload",
            facts_path.to_str().unwrap(),
            "--data",
            data,
            "--at",
            "600000",
        ],
        &dir,
    );
    assert!(
        executed.status.success(),
        "{}",
        String::from_utf8_lossy(&executed.stderr)
    );
    let answers = String::from_utf8_lossy(&executed.stdout);
    assert!(
        answers.contains("180") && answers.contains("420"),
        "{answers}"
    );

    let benched = asap_run(
        &[
            "bench",
            "--workload",
            facts_path.to_str().unwrap(),
            "--arms",
            "exact,planned,noraw",
            "--duration",
            "600000",
            "--no-cost-gate",
        ],
        &dir,
    );
    assert!(
        benched.status.success(),
        "{}",
        String::from_utf8_lossy(&benched.stderr)
    );
    let runs = dir.0.join("runs");
    assert!(runs.is_dir(), "bench wrote no runs/ directory");
}

/// `bench --suite showcase --small`: every row passes (from milestone 1).
#[test]
#[ignore = "timing: run on a dedicated host"]
fn e2e_small_showcase() {
    let report = bench_showcase(true, &calibrated(true), None).unwrap();
    assert_eq!(report.verdict, GateVerdict::Pass, "{:?}", report.rows);
}

/// The full showcase meets the milestone criteria.
#[test]
#[ignore = "milestone scale"]
fn e2e_full_showcase() {
    let report = bench_showcase(false, &calibrated(false), None).unwrap();
    assert_eq!(report.verdict, GateVerdict::Pass, "{:?}", report.rows);
}

fn calibrated(small: bool) -> PlanningTables {
    let calibration = calibrate(&CalibrateRequest {
        families: CalibrationFamilies::All,
        small,
        seed: 7,
    })
    .unwrap();
    PlanningTables {
        cost_inputs: asap_cost::CostInputs {
            measured_operation_costs: calibration.costs,
            ..asap_cost::CostInputs::default()
        },
        accuracy: calibration.accuracy,
        warnings: Vec::new(),
    }
}

/// A hash of every sample `description` generates.
fn stream_hash(description: &TimeSeriesDescription) -> u64 {
    use std::hash::{Hash, Hasher};
    let generated = description.generate().unwrap();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (labels, ts, value) in generated.samples() {
        labels.hash(&mut hasher);
        ts.hash(&mut hasher);
        value.to_bits().hash(&mut hasher);
    }
    hasher.finish()
}

/// `aqpbm-datagen`: one description and seed give one sample stream; a
/// different seed another.
#[test]
fn datagen_seeded_bit_identical() {
    let description = TimeSeriesDescription {
        labels: vec![
            LabelSpec {
                name: "job".into(),
                cardinality: 4,
                skewness: 0.0,
            },
            LabelSpec {
                name: "instance".into(),
                cardinality: 16,
                skewness: 1.1,
            },
        ],
        series_num: 8,
        start_ms: 0,
        sample_interval_ms: 1_000,
        duration_ms: 600_000,
        value: DataDistribution::Zipf(ZipfParameter {
            skewness: 1.1,
            population_size: 1_000,
            seed: 0,
        }),
        counter: false,
        seed: 42,
    };
    assert_eq!(stream_hash(&description), stream_hash(&description));
    let other = TimeSeriesDescription {
        seed: 43,
        ..description.clone()
    };
    assert_ne!(stream_hash(&description), stream_hash(&other));
}

/// Data generated from the facts is identical across runs under one seed.
#[test]
fn generated_samples_seeded_bit_identical() {
    let plain = workload(
        &[("sum by (job) (sum_over_time(m[1m]))", exact())],
        10_000,
        &[("m", four_series_facts())],
    );
    let data = plain.data_workload.clone().unwrap();
    let settings = GeneratedDataSettings {
        seed: 42,
        start_ms: 0,
        duration_ms: 600_000,
    };
    assert_eq!(
        generate_samples(&data, &settings).unwrap(),
        generate_samples(&data, &settings).unwrap()
    );
}

/// `facts` twice gives byte-identical JSON.
#[test]
fn facts_seeded_bit_identical() {
    let samples = four_constant_series(600_000, |series, ts| {
        (series == 1).then_some((ts % 13_000) as f64)
    });
    let plain = workload(
        &[("sum by (job, instance) (sum_over_time(m[1m]))", exact())],
        10_000,
        &[],
    );
    let once = || {
        let report = source_facts_of_samples(&samples, &plain, FactsOptions::default()).unwrap();
        serde_json::to_string(&report.data_workload).unwrap()
    };
    assert_eq!(once(), once());
}

/// Two small calibrations write byte-identical accuracy tables (trials
/// are seeded).
#[test]
fn calibrate_accuracy_seeded() {
    let once = || {
        let calibration = calibrate(&CalibrateRequest {
            families: CalibrationFamilies::All,
            small: true,
            seed: 7,
        })
        .unwrap();
        serde_json::to_string(&calibration.accuracy).unwrap()
    };
    assert_eq!(once(), once());
}

/// The same tables give the same `plan.txt`.
#[test]
fn plan_given_tables_identical() {
    let dir = TempDir::new("plan_txt");
    let plain = workload(
        &[("sum by (job) (sum_over_time(m[1m]))", exact())],
        10_000,
        &[("m", four_series_facts())],
    );
    let workload_path = dir.write("w.json", &serde_json::to_string(&plain).unwrap());
    for _ in 0..2 {
        let output = asap_run(
            &[
                "bench",
                "--workload",
                workload_path.to_str().unwrap(),
                "--arms",
                "planned",
                "--duration",
                "600000",
                "--no-cost-gate",
            ],
            &dir,
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let runs = dir.0.join("runs");
    let machine = std::fs::read_dir(&runs)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let plans: Vec<String> = std::fs::read_dir(machine)
        .unwrap()
        .map(|run| std::fs::read_to_string(run.unwrap().path().join("plan.txt")).unwrap())
        .collect();
    assert_eq!(plans.len(), 2, "two runs, two run directories");
    assert_eq!(plans[0], plans[1]);
}

/// Two full calibrations: the costs differ by at most the noise and the
/// showcase plan selection is identical, or each change still passes the
/// gate.
// provisional answer 23
#[test]
#[ignore = "milestone scale"]
fn plan_stable_under_recalibration() {
    let first = calibrated(false);
    let second = calibrated(false);
    for (a, b) in first
        .cost_inputs
        .measured_operation_costs
        .operators
        .iter()
        .zip(&second.cost_inputs.measured_operation_costs.operators)
    {
        assert_eq!(
            (a.operator, &a.phase, &a.sketch, &a.exact, &a.measure),
            (b.operator, &b.phase, &b.sketch, &b.exact, &b.measure)
        );
        let ratio = a.ns_per_unit.max(1e-9) / b.ns_per_unit.max(1e-9);
        assert!((1.0 / 1.05..=1.05).contains(&ratio), "{a:?} vs {b:?}");
    }
    let first_run = bench_showcase(false, &first, None).unwrap();
    let second_run = bench_showcase(false, &second, None).unwrap();
    for (a, b) in first_run.arms.iter().zip(&second_run.arms) {
        if a.plan != b.plan {
            assert_eq!(
                second_run.verdict,
                GateVerdict::Pass,
                "a changed plan fails the gate"
            );
        }
    }
}
