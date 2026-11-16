//! Cost calibration (`asap_run calibrate`): microbenchmark every operator
//! kind × phase × state family on the executor, at several sizes, and fit
//! the planner's per-node model `cpu_ns = units · ns_per_unit +
//! ns_per_invocation` to the measurements.
//!
//! Example: an operator measured at 1,000, 10,000 and 100,000 units taking
//! 60,000, 510,000 and 5,010,000 ns fits `ns_per_unit = 50` and
//! `ns_per_invocation = 10,000`.

use asap_cost::{MeasuredOperationCosts, OperatorCost, OperatorKind};
use asap_planner::{PhysicalASAPDAG, PhysicalASAPNodeId};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::summary_type::{ExactKind, SketchParams};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

/// One measurement: the units one invocation processed and the thread CPU
/// time it took.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostSample {
    pub units: f64,
    pub cpu_ns: f64,
}

/// The fitted per-node model and what the fit noticed.
#[derive(Debug, Clone, PartialEq)]
pub struct FittedOperatorCost {
    pub ns_per_unit: f64,
    pub ns_per_invocation: f64,
    /// Empty for a clean fit.
    pub flags: Vec<FitFlag>,
}

/// Something about a fit `calibrate-run.json` records.
#[derive(Debug, Clone, PartialEq)]
pub enum FitFlag {
    /// The unconstrained intercept was negative (`raw_intercept_ns`); it
    /// was set to 0 and the slope refitted through the origin.
    NegativeInterceptClamped { raw_intercept_ns: f64 },
    /// The fit misses the measurement at `units` by `relative_residual`,
    /// more than the tolerance.
    ResidualAboveTolerance { units: f64, relative_residual: f64 },
    /// Fewer distinct sizes, or a narrower span of sizes, than a fit needs.
    TooFewSizes { sizes: usize },
}

/// Fit `cpu_ns = units · ns_per_unit + ns_per_invocation` to `samples`.
pub fn fit_operator_cost(samples: &[CostSample]) -> FittedOperatorCost {
    let _ = samples;
    todo!("asap_run: implement after test approval")
}

/// A per-thread CPU clock.
pub trait CpuClock {
    /// CPU nanoseconds this thread has used.
    fn thread_cpu_ns(&self) -> u64;
}

/// The operating system's per-thread CPU clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct ThreadCpuClock;

impl CpuClock for ThreadCpuClock {
    fn thread_cpu_ns(&self) -> u64 {
        todo!("asap_run: implement after test approval")
    }
}

/// One operator invocation a calibration times, at a given size.
pub trait CalibrationKernel {
    /// Run one invocation processing `units` units, counted as the cost
    /// model counts them (`OperatorKind`).
    fn run(&mut self, units: u64);
}

/// Time `kernel` on `clock` at each of `sizes`, `repetitions` times each,
/// one [`CostSample`] per size (the median of its repetitions).
pub fn measure_kernel(
    kernel: &mut dyn CalibrationKernel,
    clock: &dyn CpuClock,
    sizes: &[u64],
    repetitions: u32,
) -> Vec<CostSample> {
    let _ = (kernel, clock, sizes, repetitions);
    todo!("asap_run: implement after test approval")
}

/// The state family an entry is for.
#[derive(Debug, Clone, PartialEq)]
pub enum CalibratedFamily {
    None,
    Sketch(SketchParams),
    Exact(ExactKind),
    /// An `Aggregate` measure, by its JSON `kind`.
    Measure(String),
}

/// One (operator, phase, family) a calibration measures.
#[derive(Debug, Clone, PartialEq)]
pub struct CalibrationCase {
    pub operator: OperatorKind,
    pub phase: ExecutionTiming,
    pub family: CalibratedFamily,
}

/// Which families a calibration measures.
#[derive(Debug, Clone, PartialEq)]
pub enum CalibrationFamilies {
    All,
    /// The exact accumulators and aggregate measures only.
    Exact,
    Only(Vec<String>),
}

/// The physical DAG a case's microbenchmark runs at one size.
#[derive(Debug, Clone)]
pub struct CalibrationDag {
    pub dag: PhysicalASAPDAG,
    pub entries: Vec<QueryWorkloadEntry>,
    pub data: DataWorkload,
    /// The node the case times.
    pub measured_node: PhysicalASAPNodeId,
    /// The units that node processes per invocation, as the
    /// microbenchmark counts them.
    pub units: u64,
}

/// Every case of `families`: each `OperatorKind` × phase × family.
pub fn calibration_cases(families: &CalibrationFamilies) -> Vec<CalibrationCase> {
    let _ = families;
    todo!("asap_run: implement after test approval")
}

impl CalibrationCase {
    /// The DAG this case's microbenchmark runs at about `units` units.
    pub fn dag_at(&self, units: u64) -> CalibrationDag {
        let _ = units;
        todo!("asap_run: implement after test approval")
    }

    /// The table entry of this case with `fit`.
    pub fn operator_cost(
        &self,
        fit: &FittedOperatorCost,
        memory_bytes_per_state: Option<f64>,
    ) -> OperatorCost {
        let _ = (fit, memory_bytes_per_state);
        todo!("asap_run: implement after test approval")
    }
}

/// One case's measurements and fit, as `calibrate-run.json` records them.
#[derive(Debug, Clone, PartialEq)]
pub struct CostFitRecord {
    pub case: CalibrationCase,
    pub samples: Vec<CostSample>,
    pub fit: FittedOperatorCost,
    pub memory_bytes_per_state: Option<f64>,
}

/// The cost table of `records`, with the per-request overhead
/// `ns_per_evaluation`.
pub fn cost_table_of(records: &[CostFitRecord], ns_per_evaluation: f64) -> MeasuredOperationCosts {
    let _ = (records, ns_per_evaluation);
    todo!("asap_run: implement after test approval")
}
