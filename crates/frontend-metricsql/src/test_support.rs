//! Entry points only tests use, exposed under the `test-support` feature:
//! parsing and printing a query, and lowering one query whose metrics share
//! one sample interval. Production lowers whole workloads with
//! [`lower_metricsql_query_workload`](crate::lower_metricsql_query_workload).
use std::rc::Rc;
use std::time::Duration;

use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::{OperatorNode, QueryRoot};

use crate::{MetricsqlError, MetricsqlExpr};

/// Parse one MetricsQL query.
pub fn parse_metricsql(query: &str) -> Result<MetricsqlExpr, MetricsqlError> {
    crate::parse_metricsql(query)
}

/// The query printed back from its parse.
pub fn canonical_metricsql(query: &str) -> Result<String, MetricsqlError> {
    Ok(crate::parse_metricsql(query)?.to_string())
}

/// Lower one MetricsQL query whose metrics are all sampled every
/// `sample_interval`, to an operator root; a scalar query is an error (use
/// [`lower_metricsql_query`]).
pub fn lower_metricsql(
    query: &str,
    accuracy: AccuracyTarget,
    sample_interval: Duration,
) -> Result<Rc<OperatorNode>, MetricsqlError> {
    match lower_metricsql_query(query, accuracy, sample_interval)? {
        QueryRoot::Operator(node) => Ok(node),
        _ => Err(MetricsqlError::UnsupportedFeature(
            "scalar root: use lower_metricsql_query".into(),
        )),
    }
}

/// Lower scalar constants without fabricating a relational operator. Every
/// metric is sampled every `sample_interval`: a bare instant selector looks
/// back that long for its latest sample.
pub fn lower_metricsql_query(
    query: &str,
    accuracy: AccuracyTarget,
    sample_interval: Duration,
) -> Result<QueryRoot, MetricsqlError> {
    crate::lower_with_sample_intervals(query, accuracy, &|_| Some(sample_interval))
}
