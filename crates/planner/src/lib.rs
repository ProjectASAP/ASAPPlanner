//! ASAPPlanner's library facade: one call from a prepared workload to the
//! selected plan (issue #429).
//!
//! ```text
//! PlanningWorkload ──frontends──▶ ParsedWorkload ──optimization pass──▶ PlanOutput
//!                   (this crate)                  (asap-plan-selection)
//! ```
//!
//! The default pass, [`Stage1ToStage3Pass`], runs Stage 1 (logical ASAP-aware
//! optimization), Stage 2 (physical ASAP-aware optimization) and Stage 3 (plan
//! selection) in order ([`run_stage1_to_stage3`]).
//!
//! [`e2e_plan`] runs both steps. [`plan_installable`] runs them for a
//! deployment runtime and returns the single selected DAG ready to install
//! ([`InstallablePlan`]). A caller who already holds pre-ASAP IR — a
//! new frontend, a deserialized plan, a test that does not want to build SQL
//! and a catalog — skips this crate and calls
//! [`asap_plan_selection::optimize`] directly.
//!
//! Each frontend is a cargo feature — `promql`, `sql` and `metricsql`, all on
//! by default. A workload whose language's feature is off fails validation
//! with [`UserInputError::FrontendDisabled`].

use asap_types::workload::parsed_workload::{ParsedWorkload, ParsedWorkloadError};
#[cfg(feature = "sql")]
use asap_types::workload::SqlDialect;
use asap_types::workload::{QueryLanguage, WorkloadError};

#[cfg(feature = "metricsql")]
use asap_frontend_metricsql::{lower_metricsql_query_workload, MetricsqlError};
#[cfg(feature = "promql")]
use asap_frontend_promql::{
    lower_promql_query_workload, lower_promql_query_workload_with_histograms, PromqlError,
};
#[cfg(feature = "sql")]
use asap_frontend_sql::{lower_sql_dialect, SqlError};

// The optimization stage's vocabulary is this facade's vocabulary too: a caller
// configures the same models and reads the same output whether it goes through
// `e2e_plan` or straight to `optimize`.
pub use asap_plan_selection::pass::{
    optimize, run_stage1_to_stage3, OptimizationInput, OptimizationPass, OptimizeError,
    PassRegistry, PlanOutput, PlanningModels, QueryPlan, Stage1ToStage3Pass, Stage1ToStage3Run,
};

mod installable;
pub use installable::{
    installable_plan_of, plan_installable, InstallablePlan, InstallablePlanError,
    PredictedNodeUsage, PredictedQueryUsage, PredictedUsage, SelectedPlanReport,
    INSTALLABLE_PLAN_WIRE_VERSION,
};

/// Helpers for a runtime's correctness tests (feature `test-support`).
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::collections::HashMap;

    use crate::{InstallablePlan, InstallablePlanError};

    /// For correctness tests only, never for installing: `plan` with
    /// nothing materialized, the same operators, summaries and parameters
    /// with every node at query time, and the same query texts. A runtime's
    /// test compares its installed plan's answers with the executor running
    /// this one over raw data samples. Validates `plan` first.
    pub fn query_time_reference(
        plan: &InstallablePlan,
    ) -> Result<InstallablePlan, InstallablePlanError> {
        plan.validate()?;
        let dag = plan.dag.with_every_node_at_query_time()?;
        InstallablePlan::of_dag(
            dag,
            plan.query_texts.clone(),
            &HashMap::new(),
            &HashMap::new(),
        )
    }
}

// What a deployment runtime needs to build a `UserInput` and read an
// `InstallablePlan`, so it depends on this crate and `asap-executor` alone.
#[cfg(feature = "promql")]
pub use asap_frontend_promql::{HistogramCatalog, HistogramKind};
#[cfg(feature = "sql")]
pub use asap_frontend_sql::SqlCatalog;
pub use asap_logical_asap_optimization::{DeploymentCapabilities, SupportedSummary};
pub use asap_plan_selection::{
    ClusterLimits, ClusterObjective, CostModel, CostTableProvenance, MeasuredOperationCost,
    MeasuredOperationCosts, NodePriceSource, OperatorCost, OperatorKind, PlanSelectionSettings,
    RejectionCause, ResourceSpeed, ResourceUnitCost, SearchReport, SummaryOperation,
};
pub use asap_types::ir::materialized_view::{
    InstallableDagError, MaterializedView, MaterializedViewInputs, MaterializedViewRefresh,
    RawDataRetention, RawDataWindow, ViewCoverage,
};
pub use asap_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPNodeId};
pub use asap_types::ir::properties::summary_coverage::{ColumnIdentity, Constraint, SelectionBox};
pub use asap_types::ir::properties::ExecutionTiming;
pub use asap_types::ir::schema::{SketchAlgorithm, SketchParams, StatisticKind};
pub use asap_types::workload::PlanningWorkload;

// ── Input ────────────────────────────────────────────────────────────────

/// The lowering dependencies of one frontend. Which variant applies is fixed by
/// `PlanningWorkload::query_workload.language`; [`UserInput::validate`] checks
/// that they agree. Each variant exists only when its frontend's feature is on.
#[non_exhaustive]
pub enum FrontendInput<'a> {
    #[cfg(feature = "sql")]
    Sql { catalog: &'a SqlCatalog },
    #[cfg(feature = "promql")]
    Promql {
        histograms: Option<HistogramCatalog>,
    },
    #[cfg(feature = "metricsql")]
    Metricsql,
    /// Keeps `'a` in use when the `sql` feature is off; it cannot be
    /// constructed.
    #[cfg(not(feature = "sql"))]
    #[doc(hidden)]
    NoSqlFeature(std::convert::Infallible, std::marker::PhantomData<&'a ()>),
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
        // The `FrontendInput` variant that lowers `language`, and its cargo
        // feature with whether this build enables it.
        let (frontend, feature) = match language {
            QueryLanguage::SQL(_) | QueryLanguage::DataFusion => {
                ("Sql", Some(("sql", cfg!(feature = "sql"))))
            }
            QueryLanguage::PromQL => ("Promql", Some(("promql", cfg!(feature = "promql")))),
            QueryLanguage::MetricsQL => (
                "Metricsql",
                Some(("metricsql", cfg!(feature = "metricsql"))),
            ),
            // No frontend lowers it.
            QueryLanguage::ElasticDSL => ("none", None),
        };
        if let Some((feature, false)) = feature {
            return Err(UserInputError::FrontendDisabled {
                language: format!("{language:?}"),
                feature,
            });
        }
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
            #[cfg(feature = "sql")]
            Self::Sql { .. } => "Sql",
            #[cfg(feature = "promql")]
            Self::Promql { .. } => "Promql",
            #[cfg(feature = "metricsql")]
            Self::Metricsql => "Metricsql",
            #[cfg(not(feature = "sql"))]
            Self::NoSqlFeature(never, _) => match *never {},
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
    /// The workload's language needs a frontend this build leaves out: the
    /// caller must enable `asap-planner`'s cargo feature `feature`. Bad
    /// input for this build, not an internal bug.
    #[error(
        "workload language {language} needs asap-planner's `{feature}` cargo feature, \
         which this build does not enable"
    )]
    FrontendDisabled {
        language: String,
        feature: &'static str,
    },
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LoweringError {
    #[cfg(feature = "sql")]
    #[error(transparent)]
    Sql(SqlError),
    #[cfg(feature = "promql")]
    #[error(transparent)]
    Promql(PromqlError),
    #[cfg(feature = "metricsql")]
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
/// Async because the SQL frontend plans through DataFusion; it stays async
/// when the `sql` feature is off, so callers see one signature. One failed query
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
/// The SQL frontend is driven one entry at a time rather than
/// through `lower_sql_batch`, which walks `query_batch` alone and would drop
/// every repeating query, and the output must cover every entry.
async fn lower(input: &UserInput<'_>) -> Result<Vec<asap_types::ir::QueryRoot>, PlanError> {
    match &input.frontend_specific {
        #[cfg(feature = "sql")]
        FrontendInput::Sql { catalog } => {
            let dialect = match &input.workload.query_workload.language {
                QueryLanguage::SQL(dialect) => dialect.clone(),
                // `DataFusion` is the legacy alias for `SQL(DataFusionSQL)`.
                _ => SqlDialect::DataFusionSQL,
            };
            let mut lowered = Vec::new();
            for (index, entry) in input.workload.query_workload.entries().enumerate() {
                let expr = lower_sql_dialect(
                    &entry.query.0,
                    catalog,
                    dialect.clone(),
                    entry.requirements.accuracy.clone(),
                )
                .await
                .map_err(|source| PlanError::Lowering {
                    entry_index: Some(index),
                    source: LoweringError::Sql(source),
                })?;
                lowered.push(expr.into());
            }
            Ok(lowered)
        }
        #[cfg(feature = "promql")]
        FrontendInput::Promql { histograms, .. } => {
            let lowered = match histograms {
                Some(histograms) => {
                    lower_promql_query_workload_with_histograms(input.workload, histograms.clone())
                }
                None => lower_promql_query_workload(input.workload),
            }
            .map_err(|source| PlanError::Lowering {
                entry_index: None,
                source: LoweringError::Promql(source),
            })?;
            Ok(lowered)
        }
        #[cfg(feature = "metricsql")]
        FrontendInput::Metricsql => {
            lower_metricsql_query_workload(input.workload).map_err(|source| PlanError::Lowering {
                entry_index: None,
                source: LoweringError::Metricsql(source),
            })
        }
        #[cfg(not(feature = "sql"))]
        FrontendInput::NoSqlFeature(never, _) => match *never {},
    }
}
