//! MetricsQL frontend: parse into a MetricsQL-owned AST, then lower directly
//! into the canonical [`QueryExpr`](asap_types::pre_asap::QueryExpr).
//!
//! PromQL-compatible syntax shares the established AST-to-canonical lowering.
//! MetricsQL-only syntax remains explicit in [`MetricsqlExpr`], so the frontend
//! never turns a MetricsQL source string into a different PromQL source string.

use std::rc::Rc;

use asap_frontend_promql::PromqlLowerer;
use asap_types::pre_asap::{resolve_root, AggIntent, QueryExpr, Reduction, UnresolvedQueryExpr};
use asap_types::types::AccuracyTarget;
use promql_parser::parser::{self, Expr};
use thiserror::Error;

/// Parsed MetricsQL expression. Extension nodes remain distinct from the
/// PromQL-compatible AST so their semantics cannot be silently discarded.
#[derive(Debug)]
pub enum MetricsqlExpr {
    Compatible(Expr),
    DefaultRollup(Box<MetricsqlExpr>),
    KeepMetricNames(Box<MetricsqlExpr>),
}

#[derive(Debug, Error)]
pub enum MetricsqlError {
    #[error("MetricsQL parse error: {0}")]
    Parse(String),
    #[error("unsupported MetricsQL feature: {0}")]
    UnsupportedFeature(String),
    #[error("MetricsQL canonical lowering failed: {0}")]
    Lower(String),
    #[error("MetricsQL column resolution failed: {0}")]
    Resolve(String),
}

/// Parse MetricsQL into its own AST.
pub fn parse_metricsql(query: &str) -> Result<MetricsqlExpr, MetricsqlError> {
    let query = query.trim();
    if let Some(inner) = strip_postfix_keyword(query, "keep_metric_names") {
        return Ok(MetricsqlExpr::KeepMetricNames(Box::new(parse_metricsql(
            inner,
        )?)));
    }
    if let Some(inner) = root_call_argument(query, "default_rollup")? {
        return Ok(MetricsqlExpr::DefaultRollup(Box::new(parse_metricsql(
            inner,
        )?)));
    }
    parser::parse(query)
        .map(MetricsqlExpr::Compatible)
        .map_err(MetricsqlError::Parse)
}

/// Parse and lower a MetricsQL query to the same canonical tree used by the
/// PromQL and SQL frontends.
pub fn lower_metricsql(query: &str, accuracy: AccuracyTarget) -> Result<QueryExpr, MetricsqlError> {
    let parsed = parse_metricsql(query)?;
    let unresolved = lower_expr(&parsed, &accuracy)?;
    resolve_root(&unresolved).map_err(|e| MetricsqlError::Resolve(e.to_string()))
}

fn lower_expr(
    expr: &MetricsqlExpr,
    accuracy: &AccuracyTarget,
) -> Result<UnresolvedQueryExpr, MetricsqlError> {
    match expr {
        MetricsqlExpr::Compatible(expr) => PromqlLowerer::lower_expr(expr, accuracy)
            .map_err(|e| MetricsqlError::Lower(e.to_string())),
        MetricsqlExpr::DefaultRollup(child) => {
            let lowered = lower_expr(child, accuracy)?;
            if !matches!(lowered, UnresolvedQueryExpr::TimeRange { .. }) {
                return Err(MetricsqlError::UnsupportedFeature(
                    "default_rollup without an explicit range requires an evaluation step"
                        .into(),
                ));
            }
            Ok(UnresolvedQueryExpr::Aggregate {
                reduction: Reduction::PerEntity,
                measures: vec![AggIntent::LastOverTime],
                output_names: vec![],
                having: None,
                child: Rc::new(lowered),
            })
        }
        MetricsqlExpr::KeepMetricNames(_) => Err(MetricsqlError::UnsupportedFeature(
            "keep_metric_names requires metric-name lineage, which canonical QueryExpr does not represent"
                .into(),
        )),
    }
}

fn strip_postfix_keyword<'a>(query: &'a str, keyword: &str) -> Option<&'a str> {
    let prefix = query.strip_suffix(keyword)?.trim_end();
    (!prefix.is_empty()).then_some(prefix)
}

fn root_call_argument<'a>(
    query: &'a str,
    function: &str,
) -> Result<Option<&'a str>, MetricsqlError> {
    let Some(rest) = query.strip_prefix(function) else {
        return Ok(None);
    };
    let rest = rest.trim_start();
    if !rest.starts_with('(') {
        return Ok(None);
    }
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    let mut close = None;
    for (index, ch) in rest.char_indices() {
        if quoted {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                quoted = false;
            }
            continue;
        }
        match ch {
            '"' => quoted = true,
            '(' => depth += 1,
            ')' => {
                depth = depth.checked_sub(1).ok_or_else(|| {
                    MetricsqlError::Parse("unbalanced default_rollup call".into())
                })?;
                if depth == 0 {
                    close = Some(index);
                    break;
                }
            }
            _ => {}
        }
    }
    let close =
        close.ok_or_else(|| MetricsqlError::Parse("unclosed default_rollup call".into()))?;
    if !rest[close + 1..].trim().is_empty() {
        return Ok(None);
    }
    let inner = rest[1..close].trim();
    if inner.is_empty() {
        return Err(MetricsqlError::Parse(
            "default_rollup requires one expression".into(),
        ));
    }
    Ok(Some(inner))
}
