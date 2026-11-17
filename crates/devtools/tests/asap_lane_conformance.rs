//! The harness's conformance kit on the ASAP lane. It checks that the
//! executor's work runs on the calling thread by comparing thread and process
//! CPU, so it runs in its own test binary: other tests running in the same
//! process would add process CPU the lane did not spend.
use std::collections::BTreeMap;
use std::path::PathBuf;

use aqpbm_core::harness::conformance::{self, ConformanceCheck};
use aqpbm_core::harness::engine::{
    ArmSpec, CpuAccounting, EngineDescription, EngineError, EngineUnderTest, InstalledArm,
    QueryLanguage, WorkloadSpec,
};
use aqpbm_core::harness::protocol::{MeasurementClocks, OsCpuClock};
use asap_devtools::asap_lane::AsapEngine;
use asap_planner::{plan_installable, FrontendInput, PlanningModels, PlanningWorkload, UserInput};

#[path = "asap_lane_support.rs"]
mod support;
use support::{planning_workload, TempDir};

/// The conformance kit's probe arm has no engine options, so this engine
/// hands `AsapEngine` the plan file of each probe workload, by name.
struct WithPlanFiles {
    plan_files: BTreeMap<String, PathBuf>,
}

impl EngineUnderTest for WithPlanFiles {
    fn description(&self) -> EngineDescription {
        AsapEngine.description()
    }
    fn cpu_accounting(&self) -> CpuAccounting {
        AsapEngine.cpu_accounting()
    }
    fn install(
        &self,
        workload: &WorkloadSpec,
        arm: &ArmSpec,
    ) -> Result<Box<dyn InstalledArm>, EngineError> {
        let arm = ArmSpec {
            engine_options: serde_json::json!({"plan_file": self.plan_files[&workload.name]}),
            ..arm.clone()
        };
        AsapEngine.install(workload, &arm)
    }
}

/// The harness's conformance kit passes on the ASAP lane: deterministic
/// answers, left-open windows (`count_over_time(probe[10s])` at 20 s counts
/// only the sample at 20 s), and the executor's work on the calling thread.
#[test]
fn conformance_kit_passes_on_the_asap_engine() {
    let temp = TempDir::new("conformance");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let mut plan_files = BTreeMap::new();
    for workload in conformance::probe_workloads() {
        let query = &workload.queries[0].texts[0];
        assert_eq!(query.language, QueryLanguage::PromQl);
        let planning: PlanningWorkload = serde_json::from_str(&planning_workload(
            &query.text,
            "probe",
            1,
            10_000,
            r#"{"ingesting": {"rows_per_second": 0.1}}"#,
        ))
        .unwrap();
        let (plan, _) = runtime
            .block_on(plan_installable(UserInput::new(
                &planning,
                FrontendInput::Promql,
                PlanningModels::builtin(),
            )))
            .unwrap();
        let path = temp.0.join(format!("{}.plan.json", workload.name));
        std::fs::write(&path, serde_json::to_string(&plan).unwrap()).unwrap();
        plan_files.insert(workload.name.clone(), path);
    }
    let mut engine = WithPlanFiles { plan_files };
    let report = conformance::check(
        &mut engine,
        &MeasurementClocks::with_calling_thread(&OsCpuClock),
    )
    .unwrap_or_else(|failures| panic!("{failures:#?}"));
    for check in [
        ConformanceCheck::DeterministicAnswers,
        ConformanceCheck::LeftOpenWindow,
        ConformanceCheck::WorkOnCallingThread,
    ] {
        assert!(report.passed.contains(&check), "{report:?}");
    }
}
