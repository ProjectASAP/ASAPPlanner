use std::fmt;

use asap_types::pre_asap::ResolveTreeError;

/// Errors from lowering a PromQL query (L1 parse → canonical L2, built
/// directly → [`resolve_root`](asap_types::pre_asap::resolve_root) binds it
/// to L3, issue #179).
///
/// Carries no DataFusion type — the PromQL front end never depends on the SQL
/// stack. The language-neutral variants (`UnsupportedFeature` / `WrongLanguage`
/// / `Convert`) are mirrored by [`asap_frontend_sql::SqlError`] rather than
/// shared, so neither front end pulls the other's parser.
#[derive(Debug)]
pub enum PromqlError {
    /// The `promql-parser` crate rejected the query string (L1 parse failure).
    Parse(String),
    /// A PromQL function (`rate`, `*_over_time`, …) not supported in this version.
    UnsupportedFunction(String),
    /// A PromQL aggregation operator (`sum`, `topk`, …) not supported.
    UnsupportedAggregateOp(String),
    /// A structural feature (offset / `@` / `without`) not supported in this
    /// version.
    UnsupportedFeature(String),
    /// A required function / aggregator argument was missing.
    MissingArgument(String),
    /// An argument had the wrong shape (e.g. a non-numeric `topk` parameter).
    InvalidParameter(String),
    /// The workload's query language is not PromQL.
    WrongLanguage(String),
    /// Resolving the canonical L2 tree to L3 failed (name resolution against
    /// the bound schema).
    Convert(ResolveTreeError),
}

impl fmt::Display for PromqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "PromQL parse error: {e}"),
            Self::UnsupportedFunction(n) => write!(f, "unsupported PromQL function: {n}"),
            Self::UnsupportedAggregateOp(n) => write!(f, "unsupported PromQL aggregate op: {n}"),
            Self::UnsupportedFeature(m) => write!(f, "unsupported feature: {m}"),
            Self::MissingArgument(m) => write!(f, "missing argument: {m}"),
            Self::InvalidParameter(m) => write!(f, "invalid parameter: {m}"),
            Self::WrongLanguage(l) => write!(f, "unsupported query language: {l}"),
            Self::Convert(e) => write!(f, "L2→L3 resolution failed: {e}"),
        }
    }
}

impl std::error::Error for PromqlError {}

impl From<ResolveTreeError> for PromqlError {
    fn from(e: ResolveTreeError) -> Self {
        Self::Convert(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_feature_label_is_language_neutral() {
        // `UnsupportedFeature` shares a Display label with the SQL side, so it
        // must not hardcode "PromQL".
        let msg = PromqlError::UnsupportedFeature("subquery".into()).to_string();
        assert_eq!(msg, "unsupported feature: subquery");
        assert!(!msg.contains("PromQL"), "got: {msg}");
    }
}
