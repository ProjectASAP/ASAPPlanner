//! ASAPPlanner's library facade: one call from a prepared workload to the
//! selected post-ASAP DAG (issue #429).
//!
//! ```text
//! PlanningWorkload ──lowering──▶ ParsedWorkload ──optimization pass──▶ PlanOutput
//!                   (this crate)                 (asap-aware-mapping)
//! ```
//!
//! [`e2e_plan`] runs both stages. A caller who already holds pre-ASAP IR — a
//! new frontend, a deserialized plan, a test that does not want to build SQL
//! and a catalog — skips this crate and calls
//! [`asap_aware_mapping::optimize`] directly.

use std::rc::Rc;

use asap_types::parsed_workload::{ParsedWorkload, ParsedWorkloadError};
use asap_types::pre_asap::query_expr::QueryExpr;
use asap_types::workload::{PlanningWorkload, QueryLanguage, SqlDialect, WorkloadError};

use asap_frontend_metricsql::{lower_metricsql, MetricsqlError};
use asap_frontend_promql::{
    lower_promql_workload, lower_promql_workload_with_histograms, HistogramCatalog, PromqlError,
};
use asap_frontend_sql::{lower_sql_dialect, SqlCatalog, SqlError};

// The optimization stage's vocabulary is this facade's vocabulary too: a caller
// configures the same models and reads the same output whether it goes through
// `e2e_plan` or straight to `optimize`.
pub use asap_aware_mapping::pass::{
    optimize, LifecycleInput, MajorPass, OptimizationInput, OptimizationPass, OptimizeError,
    PassRegistry, PlanOutput, PlanningModels, QueryLifecyclePlan, QueryPlan,
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
        /// Planning clock. PromQL lowering resolves selection horizons against
        /// it and checks the ingestion-interval evidence's freshness at it.
        now_ms: u64,
        histograms: Option<HistogramCatalog>,
    },
    Metricsql,
}

#[non_exhaustive]
pub struct UserInput<'a> {
    pub workload: &'a PlanningWorkload,
    pub frontend_specific: FrontendInput<'a>,
    pub models: PlanningModels<'a>,
    /// `Some` asks the pass to also decide summary maintenance versus raw
    /// recomputation.
    pub lifecycle: Option<LifecycleInput>,
    /// `None` uses [`MajorPass`]. A black-box caller never sets this.
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
            lifecycle: None,
            pass: None,
        }
    }

    pub fn with_lifecycle(mut self, lifecycle: LifecycleInput) -> Self {
        self.lifecycle = Some(lifecycle);
        self
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
        );
        if !matches_frontend {
            return Err(UserInputError::FrontendMismatch {
                language: format!("{language:?}"),
                frontend: self.frontend_specific.name(),
            });
        }

        if let Some(lifecycle) = &self.lifecycle {
            if let Some(horizon) = lifecycle.horizon {
                if !horizon.0.is_finite() || horizon.0 <= 0.0 {
                    return Err(UserInputError::InvalidHorizon(horizon.0));
                }
            }
            // Two clocks would let the DAG be built for one instant and priced
            // for another, with neither stage able to notice.
            if let FrontendInput::Promql { now_ms, .. } = &self.frontend_specific {
                if *now_ms != lifecycle.now_ms {
                    return Err(UserInputError::PlanningTimeMismatch {
                        frontend: *now_ms,
                        lifecycle: lifecycle.now_ms,
                    });
                }
            }
        }
        Ok(())
    }
}

impl FrontendInput<'_> {
    fn name(&self) -> &'static str {
        match self {
            Self::Sql { .. } => "Sql",
            Self::Promql { .. } => "Promql",
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
    #[error("planning horizon must be finite and positive, got {0}")]
    InvalidHorizon(f64),
    #[error("frontend planning time {frontend} ms disagrees with lifecycle planning time {lifecycle} ms")]
    PlanningTimeMismatch { frontend: u64, lifecycle: u64 },
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
    /// than one query — the PromQL frontend resolves one ingestion interval for
    /// the workload before lowering anything.
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

/// Lower every query, then run the optimization pass over the result.
///
/// Async because the SQL frontend plans through DataFusion. One failed query
/// fails the whole call: the output is positionally aligned with
/// `QueryWorkload::entries()`, and a partial result would silently break that.
pub async fn e2e_plan(input: UserInput<'_>) -> Result<PlanOutput, PlanError> {
    input.validate()?;

    let exprs = lower(&input).await?;
    let parsed = ParsedWorkload::new(input.workload.clone(), exprs)?;

    let fallback = MajorPass;
    let pass: &dyn OptimizationPass = input.pass.unwrap_or(&fallback);

    let mut optimization = OptimizationInput::new(&parsed, input.models);
    if let Some(lifecycle) = input.lifecycle {
        optimization = optimization.with_lifecycle(lifecycle);
    }
    optimize(pass, optimization).map_err(PlanError::Optimize)
}

/// One expression per normalized entry, in `QueryWorkload::entries()` order.
///
/// The SQL and MetricsQL frontends are driven one entry at a time rather than
/// through `lower_sql_batch`, which walks `query_batch` alone and would drop
/// every repeating query — exactly the entries whose recurrence the lifecycle
/// stage needs.
async fn lower(input: &UserInput<'_>) -> Result<Vec<Rc<QueryExpr>>, PlanError> {
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
                    entry.requirements.accuracy.target(),
                )
                .await
                .map_err(|source| PlanError::Lowering {
                    entry_index: Some(index),
                    source: LoweringError::Sql(source),
                })?;
                lowered.push(Rc::new(expr));
            }
            Ok(lowered)
        }
        FrontendInput::Promql {
            now_ms, histograms, ..
        } => {
            let lowered = match histograms {
                Some(histograms) => lower_promql_workload_with_histograms(
                    input.workload,
                    histograms.clone(),
                    *now_ms,
                ),
                None => lower_promql_workload(input.workload, *now_ms),
            }
            .map_err(|source| PlanError::Lowering {
                entry_index: None,
                source: LoweringError::Promql(source),
            })?;
            Ok(lowered.into_iter().map(Rc::new).collect())
        }
        FrontendInput::Metricsql => {
            let mut lowered = Vec::new();
            for (index, entry) in entries().enumerate() {
                let expr = lower_metricsql(&entry.query.0, entry.requirements.accuracy.target())
                    .map_err(|source| PlanError::Lowering {
                        entry_index: Some(index),
                        source: LoweringError::Metricsql(source),
                    })?;
                lowered.push(Rc::new(expr));
            }
            Ok(lowered)
        }
    }
}
