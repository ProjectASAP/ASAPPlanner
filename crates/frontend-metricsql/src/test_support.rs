//! Entry points only tests use, exposed under the `test-support` feature:
//! parsing and printing a query. `lower_metricsql` and `lower_metricsql_query`
//! are public API (a stable external dependency, checked by
//! `tools/verify_metricsql_external_consumer.sh`) and re-exported here for
//! existing test paths.
use crate::{MetricsqlError, MetricsqlExpr};

/// Parse one MetricsQL query.
pub fn parse_metricsql(query: &str) -> Result<MetricsqlExpr, MetricsqlError> {
    crate::parse_metricsql(query)
}

/// The query printed back from its parse.
pub fn canonical_metricsql(query: &str) -> Result<String, MetricsqlError> {
    Ok(crate::parse_metricsql(query)?.to_string())
}

pub use crate::{lower_metricsql, lower_metricsql_query};
