use std::fmt;

#[derive(Debug)]
pub enum LoweringError {
    DataFusion(datafusion::error::DataFusionError),
    TableNotFound(String),
    ColumnNotFound { table: String, column: String },
    /// A SQL feature (JOIN, subquery, etc.) not supported in this version.
    UnsupportedFeature(String),
    UnsupportedAggregate(String),
    InvalidExpression(String),
    /// The workload's query language is not handled by this lowerer.
    WrongLanguage(String),
    /// The SQL dialect is not supported (only DataFusionSQL is implemented).
    UnsupportedDialect(String),
}

impl fmt::Display for LoweringError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DataFusion(e) => write!(f, "DataFusion error: {e}"),
            Self::TableNotFound(t) => write!(f, "table not found in catalog: {t}"),
            Self::ColumnNotFound { table, column } => {
                write!(f, "column not found: {table}.{column}")
            }
            Self::UnsupportedFeature(msg) => write!(f, "unsupported SQL feature: {msg}"),
            Self::UnsupportedAggregate(name) => write!(f, "unsupported aggregate: {name}"),
            Self::InvalidExpression(msg) => write!(f, "invalid expression: {msg}"),
            Self::WrongLanguage(lang) => write!(f, "unsupported query language: {lang}"),
            Self::UnsupportedDialect(d) => write!(f, "unsupported SQL dialect: {d}"),
        }
    }
}

impl std::error::Error for LoweringError {}

impl From<datafusion::error::DataFusionError> for LoweringError {
    fn from(e: datafusion::error::DataFusionError) -> Self {
        Self::DataFusion(e)
    }
}
