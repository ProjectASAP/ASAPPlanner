use std::fmt;

#[derive(Debug)]
pub enum LoweringError {
    /// The `promql-parser` crate rejected the query string (L1 parse failure).
    Parse(String),
    /// A PromQL function (`rate`, `*_over_time`, …) not supported in this version.
    UnsupportedFunction(String),
    /// A PromQL aggregation operator (`sum`, `topk`, …) not supported.
    UnsupportedAggregateOp(String),
    /// A structural feature (PromQL offset/`@`/`without`; SQL JOIN/subquery/…)
    /// not supported in this version.
    UnsupportedFeature(String),
    /// A required function / aggregator argument was missing.
    MissingArgument(String),
    /// An argument had the wrong shape (e.g. a non-numeric `topk` parameter).
    InvalidParameter(String),
    /// The workload's query language is not handled by this lowerer.
    WrongLanguage(String),
    /// The L2→L3 converter failed (name resolution against the bound schema).
    Convert(asap_control_core::intent_algebra::ConvertError),

    // ── SQL front end (DataFusion) ───────────────────────────────────────────
    /// DataFusion failed to parse / plan the SQL query.
    DataFusion(datafusion::error::DataFusionError),
    /// A table referenced by the query is absent from the catalog.
    TableNotFound(String),
    /// A SQL aggregate function not supported in this version.
    UnsupportedAggregate(String),
    /// A SQL scalar expression that could not be lowered.
    InvalidExpression(String),
    /// The SQL dialect is not supported (only DataFusionSQL is implemented).
    UnsupportedDialect(String),
}

impl fmt::Display for LoweringError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "PromQL parse error: {e}"),
            Self::UnsupportedFunction(n) => write!(f, "unsupported PromQL function: {n}"),
            Self::UnsupportedAggregateOp(n) => write!(f, "unsupported PromQL aggregate op: {n}"),
            Self::UnsupportedFeature(m) => write!(f, "unsupported PromQL feature: {m}"),
            Self::MissingArgument(m) => write!(f, "missing argument: {m}"),
            Self::InvalidParameter(m) => write!(f, "invalid parameter: {m}"),
            Self::WrongLanguage(l) => write!(f, "unsupported query language: {l}"),
            Self::Convert(e) => write!(f, "L2→L3 conversion failed: {e}"),
            Self::DataFusion(e) => write!(f, "DataFusion error: {e}"),
            Self::TableNotFound(t) => write!(f, "table not found in catalog: {t}"),
            Self::UnsupportedAggregate(n) => write!(f, "unsupported aggregate: {n}"),
            Self::InvalidExpression(m) => write!(f, "invalid expression: {m}"),
            Self::UnsupportedDialect(d) => write!(f, "unsupported SQL dialect: {d}"),
        }
    }
}

impl std::error::Error for LoweringError {}

impl From<asap_control_core::intent_algebra::ConvertError> for LoweringError {
    fn from(e: asap_control_core::intent_algebra::ConvertError) -> Self {
        Self::Convert(e)
    }
}

impl From<datafusion::error::DataFusionError> for LoweringError {
    fn from(e: datafusion::error::DataFusionError) -> Self {
        Self::DataFusion(e)
    }
}
