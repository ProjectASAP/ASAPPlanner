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
//! [`e2e_plan`] runs both steps. A caller who already holds pre-ASAP IR — a
//! new frontend, a deserialized plan, a test that does not want to build SQL
//! and a catalog — skips this crate and calls
//! [`asap_plan_selection::optimize`] directly.

use asap_types::workload::parsed_workload::{ParsedWorkload, ParsedWorkloadError};
use asap_types::workload::{PlanningWorkload, QueryLanguage, SqlDialect, WorkloadError};

use asap_frontend_metricsql::{lower_metricsql_query_workload, MetricsqlError};
use asap_frontend_promql::{
    lower_promql_query_workload, lower_promql_query_workload_with_histograms, HistogramCatalog,
    PromqlError,
};
use asap_frontend_sql::{lower_sql_dialect, SqlCatalog, SqlError};

// The optimization stage's vocabulary is this facade's vocabulary too: a caller
// configures the same models and reads the same output whether it goes through
// `e2e_plan` or straight to `optimize`.
pub use asap_plan_selection::pass::{
    optimize, run_stage1_to_stage3, OptimizationInput, OptimizationPass, OptimizeError,
    PassRegistry, PlanOutput, PlanningModels, QueryPlan, Stage1ToStage3Pass, Stage1ToStage3Run,
};

// ── Input ────────────────────────────────────────────────────────────────

/// The lowering dependencies of one frontend. Which variant applies is fixed by
/// `PlanningWorkload::query_workload.language`; [`UserInput::validate`] checks
/// that they agree.
#[non_exhaustive]
pub enum FrontendInput<'a> {
    Sql {
        catalog: &'a SqlCatalog,
    },
    Promql {
        histograms: Option<HistogramCatalog>,
    },
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
        let matches_frontend = matches!(
            (language, &self.frontend_specific),
            (
                QueryLanguage::SQL(_) | QueryLanguage::DataFusion,
                FrontendInput::Sql { .. }
            ) | (QueryLanguage::PromQL, FrontendInput::Promql { .. })
                | (QueryLanguage::MetricsQL, FrontendInput::Metricsql)
        );
        if !matches_frontend {
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
            Self::Promql { .. } => "Promql",
            Self::Metricsql { .. } => "Metricsql",
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
/// The SQL frontend is driven one entry at a time rather than
/// through `lower_sql_batch`, which walks `query_batch` alone and would drop
/// every repeating query, and the output must cover every entry.
async fn lower(input: &UserInput<'_>) -> Result<Vec<asap_types::ir::QueryRoot>, PlanError> {
    let entries = || input.workload.query_workload.entries();

    match &input.frontend_specific {
        FrontendInput::Sql { catalog } => {
            let dialect = match &input.workload.query_workload.language {
                QueryLanguage::SQL(dialect) => dialect.clone(),
                // `DataFusion` is the legacy alias for `SQL(DataFusionSQL)`.
                _ => SqlDialect::DataFusionSQL,
            };
            let mut lowered = Vec::new();
            for (index, entry) in entries().enumerate() {
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
        FrontendInput::Metricsql => {
            lower_metricsql_query_workload(input.workload).map_err(|source| PlanError::Lowering {
                entry_index: None,
                source: LoweringError::Metricsql(source),
            })
        }
    }
}
