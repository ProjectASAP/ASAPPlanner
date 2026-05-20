use std::fmt;

#[derive(Debug)]
pub enum LoweringError {
    /// The `promql-parser` crate rejected the query string (L1 parse failure).
    Parse(String),
    /// A PromQL function (`rate`, `*_over_time`, …) not supported in this version.
    UnsupportedFunction(String),
    /// A PromQL aggregation operator (`sum`, `topk`, …) not supported.
    UnsupportedAggregateOp(String),
    /// A structural PromQL feature (offset, `@`, `without` w/o catalog, …) not supported.
    UnsupportedFeature(String),
    /// A required function / aggregator argument was missing.
    MissingArgument(String),
    /// An argument had the wrong shape (e.g. a non-numeric `topk` parameter).
    InvalidParameter(String),
    /// The workload's query language is not handled by this lowerer.
    WrongLanguage(String),
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
        }
    }
}

impl std::error::Error for LoweringError {}
