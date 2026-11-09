//! ASAPPlanner's library facade: one call from a prepared workload to the
//! selected plan (issue #429).
//!
//! ```text
//! PlanningWorkload ──frontends──▶ ParsedWorkload ──optimization pass──▶ PlanOutput
//! ```
//!
//! The default pass, [`Stage1ToStage3Pass`], orchestrates the stages
//! ([`run_planning`]): Stage 0, Pass 1, Pass 2, Stage 2
//! (`generate_physical_asap_memo_candidates`) and Stage 3
//! (`asap_stage3_plan_selection::select_plan`), which builds the selected plan.
//! The stages never call each other.
//!
//! [`e2e_plan`] runs both steps. [`plan_installable`] runs them for a
//! deployment runtime and returns the single selected DAG ready to install
//! ([`InstallablePlan`]). A caller who already holds pre-ASAP IR — a
//! new frontend, a deserialized plan, a test that does not want to build SQL
//! and a catalog — calls [`optimize`] with its own [`ParsedWorkload`].

use asap_types::workload::parsed_workload::{ParsedWorkload, ParsedWorkloadError};
use asap_types::workload::SqlDialect;
use asap_types::workload::{QueryLanguage, WorkloadError};

use asap_frontend_metricsql::{lower_metricsql_query_workload, MetricsqlError};
use asap_frontend_promql::{lower_promql_query_workload, PromqlError};
use asap_frontend_sql::{lower_sql_dialect, SqlError};

mod pass;
mod run;
pub use pass::{
    optimize, OptimizationInput, OptimizationInputError, OptimizationPass, OptimizeError,
    PassNameConflict, PassRegistry, PlanOutput, PlanningModels,
};
pub use run::{
    run_planning, PlanSearchSettings, PlanningReport, PlanningRun, SelectionError,
    Stage1ToStage3Pass,
};

mod installable;
pub use installable::{
    installable_plan_of, plan_installable, InstallablePlan, InstallablePlanError,
    PredictedNodeUsage, PredictedQueryUsage, PredictedUsage, SelectedPlanReport,
    INSTALLABLE_PLAN_WIRE_VERSION,
};

// What a deployment runtime needs to build a `UserInput` and read an
// `InstallablePlan`, so it depends on this crate and `asap-executor` alone.
pub use asap_cost::{
    ClusterLimits, ClusterObjective, CostInputs, CostModel, CostTableProvenance,
    MeasuredOperationCost, MeasuredOperationCosts, NodeCostSource, OperatorCost, OperatorKind,
    ResourceSpeed, ResourceUnitCost, SummaryOperation,
};
pub use asap_frontend_sql::SqlCatalog;
pub use asap_stage3_plan_selection::{
    AcceptedAccuracy, ErrorSelectingPlan, RejectionCause, Selection,
};
pub use asap_types::deployment_capabilities::{DeploymentCapabilities, SupportedSummary};
pub use asap_types::ir::materialized_view::{
    IngestionRefreshGroup, InstallableDagError, MaterializedView, MaterializedViewInputs,
    MaterializedViewRefresh, RawDataRetention, RawDataWindow, RefreshWindow, ViewCoverage,
};
pub use asap_types::ir::operator::SourceColumn;
pub use asap_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPNodeId};
pub use asap_types::ir::properties::summary_coverage::{
    Constraint, IntervalConstraintLimit, SelectionBox,
};
pub use asap_types::ir::properties::ExecutionTiming;
pub use asap_types::ir::schema::{SketchAlgorithm, SketchParams, StatisticKind};
pub use asap_types::workload::PlanningWorkload;

// ── Input ────────────────────────────────────────────────────────────────

/// The lowering dependencies of one frontend. Which variant applies is fixed by
/// `PlanningWorkload::query_workload.language`; [`UserInput::validate`] checks
/// that they agree.
#[non_exhaustive]
pub enum FrontendInput<'a> {
    Sql { catalog: &'a SqlCatalog },
    Promql,
    Metricsql,
}

#[non_exhaustive]
pub struct UserInput<'a> {
    pub workload: &'a PlanningWorkload,
    pub frontend_specific: FrontendInput<'a>,
    pub models: PlanningModels<'a>,
    /// `None` uses [`Stage1ToStage3Pass`]. A black-box caller never sets this.
    pub pass: Option<&'a dyn OptimizationPass>,
}

impl<'a> UserInput<'a> {
    pub fn new(
        workload: &'a PlanningWorkload,
        frontend_specific: FrontendInput<'a>,
        models: PlanningModels<'a>,
    ) -> Self {
        Self {
            workload,
            frontend_specific,
            models,
            pass: None,
        }
    }

    pub fn with_pass(mut self, pass: &'a dyn OptimizationPass) -> Self {
        self.pass = Some(pass);
        self
    }

    /// Fail before lowering on anything the two stages would otherwise only
    /// discover separately, or not at all.
    pub fn validate(&self) -> Result<(), UserInputError> {
        self.workload.validate().map_err(UserInputError::Workload)?;

        let language = &self.workload.query_workload.language;
        // The `FrontendInput` variant that lowers `language`.
        let frontend = match language {
            QueryLanguage::SQL(_) | QueryLanguage::DataFusion => "Sql",
            QueryLanguage::PromQL => "Promql",
            QueryLanguage::MetricsQL => "Metricsql",
            // No frontend lowers it.
            QueryLanguage::ElasticDSL => "none",
        };
        if self.frontend_specific.name() != frontend {
            return Err(UserInputError::FrontendMismatch {
                language: format!("{language:?}"),
                frontend: self.frontend_specific.name(),
            });
        }

        Ok(())
    }
}

impl FrontendInput<'_> {
    fn name(&self) -> &'static str {
        match self {
            Self::Sql { .. } => "Sql",
            Self::Promql => "Promql",
            Self::Metricsql => "Metricsql",
        }
    }
}

// ── Errors ───────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum UserInputError {
    #[error("workload: {0}")]
    Workload(WorkloadError),
    #[error("workload language {language} cannot be lowered by the {frontend} frontend input")]
    FrontendMismatch {
        language: String,
        frontend: &'static str,
    },
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LoweringError {
    #[error(transparent)]
    Sql(SqlError),
    #[error(transparent)]
    Promql(PromqlError),
    #[error(transparent)]
    Metricsql(MetricsqlError),
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PlanError {
    #[error("input: {0}")]
    Input(#[from] UserInputError),
    /// `entry_index` is `None` when the frontend fails the whole batch rather
    /// than one query — the PromQL and MetricsQL frontends report it for
    /// the workload as a whole.
    #[error("lowering{}: {source}", .entry_index.map(|i| format!(" entry {i}")).unwrap_or_default())]
    Lowering {
        entry_index: Option<usize>,
        source: LoweringError,
    },
    #[error("parsed workload: {0}")]
    Parsed(#[from] ParsedWorkloadError),
    #[error(transparent)]
    Optimize(OptimizeError),
    /// The selected plan cannot be installed ([`plan_installable`]): an
    /// unsupported raw-data window or materialized view (unsatisfiable
    /// request), or an internal bug; see [`InstallablePlanError`].
    #[error("installable plan: {0}")]
    Installable(InstallablePlanError),
}

// ── Entry point ──────────────────────────────────────────────────────────

/// Lower every query (the query-language frontends), then run the
/// optimization pass over the result: by default [`Stage1ToStage3Pass`], the
/// planning stages in order.
///
/// Async because the SQL frontend plans through DataFusion. One failed query
/// fails the whole call: the output is positionally aligned with
/// `QueryWorkload::entries()`, and a partial result would silently break that.
pub async fn e2e_plan(input: UserInput<'_>) -> Result<PlanOutput, PlanError> {
    input.validate()?;

    let exprs = lower(&input).await?;
    let parsed = ParsedWorkload::from_roots(input.workload.clone(), exprs)?;

    let default_pass = Stage1ToStage3Pass;
    let pass: &dyn OptimizationPass = input.pass.unwrap_or(&default_pass);

    let optimization = OptimizationInput::new(&parsed, input.models);
    optimize(pass, optimization).map_err(PlanError::Optimize)
}

/// One expression per normalized entry, in `QueryWorkload::entries()` order.
///
/// The SQL frontend lowers one entry at a time.
async fn lower(input: &UserInput<'_>) -> Result<Vec<asap_types::ir::QueryRoot>, PlanError> {
    match &input.frontend_specific {
        FrontendInput::Sql { catalog } => {
            let dialect = match &input.workload.query_workload.language {
                QueryLanguage::SQL(dialect) => dialect.clone(),
                // `DataFusion` is the legacy alias for `SQL(DataFusionSQL)`.
                _ => SqlDialect::DataFusionSQL,
            };
            let mut lowered = Vec::new();
            for (index, entry) in input.workload.query_workload.entries().enumerate() {
                let expr = lower_sql_dialect(&entry.query.0, catalog, dialect.clone())
                    .await
                    .map_err(|source| PlanError::Lowering {
                        entry_index: Some(index),
                        source: LoweringError::Sql(source),
                    })?;
                lowered.push(expr.into());
            }
            Ok(lowered)
        }
        FrontendInput::Promql => {
            lower_promql_query_workload(input.workload).map_err(|source| PlanError::Lowering {
                entry_index: None,
                source: LoweringError::Promql(source),
            })
        }
        FrontendInput::Metricsql => {
            lower_metricsql_query_workload(input.workload).map_err(|source| PlanError::Lowering {
                entry_index: None,
                source: LoweringError::Metricsql(source),
            })
        }
    }
}
