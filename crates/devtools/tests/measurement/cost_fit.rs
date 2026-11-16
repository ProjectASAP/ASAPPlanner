//! §2.1: the cost fit `cpu_ns = units · ns_per_unit + ns_per_invocation`
//! over synthetic measurements of known coefficients, and the measurement
//! loop over a fake clock.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use asap_cost::{CostInputs, MeasuredOperationCosts, OperatorCost, OperatorKind};
use asap_devtools::measurement::cost_calibration::{
    calibration_cases, fit_operator_cost, measure_kernel, CalibrationFamilies, CalibrationKernel,
    CostSample, CpuClock, FitFlag, ThreadCpuClock,
};

fn samples(points: &[(f64, f64)]) -> Vec<CostSample> {
    points
        .iter()
        .map(|&(units, cpu_ns)| CostSample { units, cpu_ns })
        .collect()
}

/// The line through (1e3, 60,000), (1e4, 510,000), (1e5, 5,010,000) is
/// 50 ns per unit + 10,000 ns per invocation.
#[test]
fn fit_recovers_exact_line() {
    let fit = fit_operator_cost(&samples(&[
        (1e3, 60_000.0),
        (1e4, 510_000.0),
        (1e5, 5_010_000.0),
    ]));
    assert!((fit.ns_per_unit - 50.0).abs() < 1e-9, "{fit:?}");
    assert!(
        (fit.ns_per_invocation - 10_000.0).abs() < 1e-9 * 10_000.0,
        "{fit:?}"
    );
    assert!(fit.flags.is_empty(), "{fit:?}");
}

/// The same points × (1.02, 0.98, 1.01). Relative-weighted least squares
/// gives 49.69 / 11,391 (computed by hand from the normal equations with
/// weights 1/y²); plain least squares gives an intercept of 2,783, which
/// fails.
// provisional answer 11
#[test]
fn fit_is_relative_weighted() {
    let fit = fit_operator_cost(&samples(&[
        (1e3, 60_000.0 * 1.02),
        (1e4, 510_000.0 * 0.98),
        (1e5, 5_010_000.0 * 1.01),
    ]));
    assert!((48.5..=51.5).contains(&fit.ns_per_unit), "{fit:?}");
    assert!(
        (8_000.0..=12_500.0).contains(&fit.ns_per_invocation),
        "{fit:?}"
    );
}

/// Five sizes from 10 to 1e5 units, noise +2%, −2%, +2%, −2%, +2%: the
/// weighted fit is 50.12 / 10,045 by hand, within 1% of 50 / 10,000.
// provisional answer 11
#[test]
fn fit_five_sizes_tightens() {
    let noise = [1.02, 0.98, 1.02, 0.98, 1.02];
    let points: Vec<(f64, f64)> = [10.0, 100.0, 1e3, 1e4, 1e5]
        .iter()
        .zip(noise)
        .map(|(&units, noise)| (units, (50.0 * units + 10_000.0) * noise))
        .collect();
    let fit = fit_operator_cost(&samples(&points));
    assert!((fit.ns_per_unit - 50.0).abs() <= 0.5, "{fit:?}");
    assert!((fit.ns_per_invocation - 10_000.0).abs() <= 100.0, "{fit:?}");
}

/// (100, 900), (1000, 10,900) have intercept −211. The fit clamps it to
/// 0, refits through the origin (weighted: Σ(x/y) / Σ(x²/y²) = 9.7702),
/// flags the clamp, and the entry validates.
// provisional answer 11
#[test]
fn fit_negative_intercept_is_clamped() {
    let fit = fit_operator_cost(&samples(&[(100.0, 900.0), (1_000.0, 10_900.0)]));
    assert_eq!(fit.ns_per_invocation, 0.0);
    assert!(
        (fit.ns_per_unit - 9.770_231_720_134_127).abs() < 1e-9,
        "{fit:?}"
    );
    assert!(
        fit.flags.iter().any(
            |flag| matches!(flag, FitFlag::NegativeInterceptClamped { raw_intercept_ns }
                if (raw_intercept_ns + 211.111).abs() < 0.01)
        ),
        "{fit:?}"
    );
    let inputs = CostInputs {
        measured_operation_costs: MeasuredOperationCosts {
            operators: vec![OperatorCost {
                operator: OperatorKind::Scan,
                phase: None,
                sketch: None,
                exact: None,
                measure: None,
                ns_per_unit: fit.ns_per_unit,
                ns_per_invocation: fit.ns_per_invocation,
                memory_bytes_per_state: None,
            }],
            ..MeasuredOperationCosts::default()
        },
        ..CostInputs::default()
    };
    inputs.validate().unwrap();
}

/// A quadratic kernel (1e3, 1e4), (1e4, 1e5), (1e5, 1e7) misses a line by
/// far more than 10% at some point (−89% at 1e5 for the weighted line):
/// flagged, not silently accepted.
// provisional answer 11
#[test]
fn fit_flags_nonlinear() {
    let fit = fit_operator_cost(&samples(&[(1e3, 1e4), (1e4, 1e5), (1e5, 1e7)]));
    assert!(
        fit.flags
            .iter()
            .any(|flag| matches!(flag, FitFlag::ResidualAboveTolerance { .. })),
        "{fit:?}"
    );
}

/// A clock only the fake kernel advances.
struct FakeClock(Rc<Cell<u64>>);

impl CpuClock for FakeClock {
    fn thread_cpu_ns(&self) -> u64 {
        self.0.get()
    }
}

/// Charges 50 ns per unit + 10 µs per invocation to the fake clock.
struct FakeKernel(Rc<Cell<u64>>);

impl CalibrationKernel for FakeKernel {
    fn run(&mut self, units: u64) {
        self.0.set(self.0.get() + 50 * units + 10_000);
    }
}

/// Driven by the measurement loop at three sizes, a kernel of known cost
/// is recovered exactly: the loop times one invocation per sample, not a
/// batch, and reports the units the kernel processed.
#[test]
fn fit_recovers_fake_clock_kernel() {
    let ticks = Rc::new(Cell::new(0));
    let clock = FakeClock(Rc::clone(&ticks));
    let mut kernel = FakeKernel(ticks);
    let measured = measure_kernel(&mut kernel, &clock, &[1_000, 10_000, 100_000], 5);
    assert_eq!(
        measured,
        samples(&[(1e3, 60_000.0), (1e4, 510_000.0), (1e5, 5_010_000.0)])
    );
    let fit = fit_operator_cost(&measured);
    assert!((fit.ns_per_unit - 50.0).abs() < 1e-9, "{fit:?}");
    assert!((fit.ns_per_invocation - 10_000.0).abs() < 1e-6, "{fit:?}");
}

/// Busy-spins 50 ns per unit + 20 µs per invocation of wall time on this
/// thread, which is that much thread CPU time on an idle core.
struct SpinKernel;

impl CalibrationKernel for SpinKernel {
    fn run(&mut self, units: u64) {
        let until = Instant::now() + Duration::from_nanos(50 * units + 20_000);
        while Instant::now() < until {
            std::hint::spin_loop();
        }
    }
}

/// On the thread CPU clock, a spinning kernel's coefficients come back
/// within 10% (per unit) and 25% (per invocation).
#[test]
#[ignore = "timing: run on a dedicated host"]
fn fit_recovers_spin_kernel() {
    let measured = measure_kernel(
        &mut SpinKernel,
        &ThreadCpuClock,
        &[1_000, 10_000, 100_000],
        9,
    );
    let fit = fit_operator_cost(&measured);
    assert!((fit.ns_per_unit - 50.0).abs() <= 5.0, "{fit:?}");
    assert!(
        (fit.ns_per_invocation - 20_000.0).abs() <= 5_000.0,
        "{fit:?}"
    );
}

/// Each case's microbenchmark counts the units the planner prices it by:
/// pricing the case's DAG with only the case's entry at 1 ns per unit and
/// none per invocation, the planner's measured node reports exactly the
/// microbenchmark's unit count (a scan's output rows, a merge's merged
/// states, any other node's first input rows).
#[test]
fn units_match_cost_model() {
    for case in calibration_cases(&CalibrationFamilies::All) {
        let calibration_dag = case.dag_at(1_000);
        let fit = asap_devtools::measurement::cost_calibration::FittedOperatorCost {
            ns_per_unit: 1.0,
            ns_per_invocation: 0.0,
            flags: Vec::new(),
        };
        let inputs = CostInputs {
            measured_operation_costs: MeasuredOperationCosts {
                operators: vec![case.operator_cost(&fit, None)],
                ..MeasuredOperationCosts::default()
            },
            ..CostInputs::default()
        };
        let (cost, _) = asap_cost::plan_cost(
            &calibration_dag.dag,
            &calibration_dag.entries,
            &calibration_dag.data,
            &inputs,
        )
        .unwrap();
        let node = &cost.per_node[&calibration_dag.measured_node];
        assert_eq!(
            node.cost_source,
            asap_cost::NodeCostSource::Measured,
            "{case:?}"
        );
        // The planner writes "measured <units> x <ns> ns + <ns> ns".
        let units: u64 = node
            .detail
            .split("measured ")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|units| units.parse().ok())
            .unwrap_or_else(|| panic!("{case:?}: {}", node.detail));
        assert_eq!(units, calibration_dag.units, "{case:?}");
    }
}
