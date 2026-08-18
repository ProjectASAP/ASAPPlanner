use std::fmt;

use asap_types::pre_asap::ResolveTreeError;

/// Errors from lowering a SQL query (L1 parse + plan via DataFusion →
/// canonical L2, built directly →
/// [`resolve_root`](asap_types::pre_asap::resolve_root) binds it to L3, issue
/// #179).
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
    /// Resolving the canonical L2 tree to L3 failed (name resolution against
    /// the bound schema).
    Convert(ResolveTreeError),
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
            Self::Convert(e) => write!(f, "L2→L3 resolution failed: {e}"),
        }
    }
}

impl std::error::Error for SqlError {}

impl From<ResolveTreeError> for SqlError {
    fn from(e: ResolveTreeError) -> Self {
        Self::Convert(e)
    }
}

impl From<datafusion::error::DataFusionError> for SqlError {
    fn from(e: datafusion::error::DataFusionError) -> Self {
        Self::DataFusion(e)
    }
}
