use std::fmt;

use asap_ir::intent_algebra::ConvertError;

/// Errors from lowering a SQL query (L1 parse + plan via DataFusion → L2 →
/// shared L2→L3 convert).
///
/// Carries no PromQL type — the SQL front end never depends on the PromQL
/// parser. The language-neutral variants (`UnsupportedFeature` / `WrongLanguage`
/// / `Convert`) are mirrored by [`asap_frontend_promql::PromqlError`] rather
/// than shared, so neither front end pulls the other's parser.
#[derive(Debug)]
pub enum SqlError {
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
    /// A structural feature (JOIN type / subquery / derived table) not
    /// supported in this version.
    UnsupportedFeature(String),
    /// The workload's query language is not SQL.
    WrongLanguage(String),
    /// The L2→L3 converter failed (name resolution against the bound schema).
    Convert(ConvertError),
}

impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DataFusion(e) => write!(f, "DataFusion error: {e}"),
            Self::TableNotFound(t) => write!(f, "table not found in catalog: {t}"),
            Self::UnsupportedAggregate(n) => write!(f, "unsupported aggregate: {n}"),
            Self::InvalidExpression(m) => write!(f, "invalid expression: {m}"),
            Self::UnsupportedDialect(d) => write!(f, "unsupported SQL dialect: {d}"),
            Self::UnsupportedFeature(m) => write!(f, "unsupported feature: {m}"),
            Self::WrongLanguage(l) => write!(f, "unsupported query language: {l}"),
            Self::Convert(e) => write!(f, "L2→L3 conversion failed: {e}"),
        }
    }
}

impl std::error::Error for SqlError {}

impl From<ConvertError> for SqlError {
    fn from(e: ConvertError) -> Self {
        Self::Convert(e)
    }
}

impl From<datafusion::error::DataFusionError> for SqlError {
    fn from(e: datafusion::error::DataFusionError) -> Self {
        Self::DataFusion(e)
    }
}
