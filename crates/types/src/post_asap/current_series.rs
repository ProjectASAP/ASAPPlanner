//! Semantic contract for a retractable population of current PromQL series values.
//! Resource limits, ingestion placement and data structures belong to the executor.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentSeriesPopulation {
    pub metric: String,
    pub matchers: Vec<CurrentSeriesMatcher>,
    pub grouping: Vec<String>,
    pub without: bool,
    pub lookback_ms: u64,
    pub max_k: usize,
    pub quantiles: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CurrentSeriesMatcher {
    pub label: String,
    pub value: String,
    pub operation: CurrentSeriesMatch,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum CurrentSeriesMatch {
    Equal,
    NotEqual,
    Regex,
    NotRegex,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CurrentSeriesReadout {
    Quantile { q: f64 },
    TopK { k: usize },
    Sum,
    Count,
    Average,
}

impl CurrentSeriesPopulation {
    /// Verify the named contract against the canonical maintenance input.
    pub fn matches_input(&self, input: &crate::pre_asap::QueryExpr) -> bool {
        use crate::pre_asap::{CompareOpKind, DataType, QueryExpr, ScalarValue, Source};
        let QueryExpr::Scan {
            source: Source::TimeSeries { metric },
            predicates,
            schema,
        } = input
        else {
            return false;
        };
        if self.metric.is_empty()
            || *metric != self.metric
            || schema.closed
            || schema.time_index.is_none()
            || self.lookback_ms != 300_000
        {
            return false;
        }
        if self.grouping.iter().any(|label| {
            !schema
                .columns
                .iter()
                .any(|c| c.name == *label && c.dtype == DataType::Utf8)
        }) {
            return false;
        }
        let mut matchers = Vec::new();
        for predicate in predicates {
            let QueryExpr::Compare { left, op, right } = predicate.0.as_ref() else {
                return false;
            };
            let (QueryExpr::Column(col), QueryExpr::Literal(ScalarValue::Utf8(value))) =
                (left.as_ref(), right.as_ref())
            else {
                return false;
            };
            let Some(column) = schema.columns.get(*col) else {
                return false;
            };
            if column.dtype != DataType::Utf8 {
                return false;
            }
            let operation = match op {
                CompareOpKind::Eq => CurrentSeriesMatch::Equal,
                CompareOpKind::Ne => CurrentSeriesMatch::NotEqual,
                CompareOpKind::Regex => CurrentSeriesMatch::Regex,
                CompareOpKind::NotRegex => CurrentSeriesMatch::NotRegex,
                _ => return false,
            };
            matchers.push(CurrentSeriesMatcher {
                label: column.name.clone(),
                value: value.clone(),
                operation,
            });
        }
        matchers.sort();
        matchers.dedup();
        self.matchers == matchers && self.grouping.windows(2).all(|w| w[0] < w[1])
    }
    pub fn supports(&self, readout: &CurrentSeriesReadout) -> bool {
        match readout {
            CurrentSeriesReadout::Quantile { q } => self.quantiles && q.is_finite(),
            CurrentSeriesReadout::TopK { k } => *k <= self.max_k,
            CurrentSeriesReadout::Sum
            | CurrentSeriesReadout::Count
            | CurrentSeriesReadout::Average => true,
        }
    }
}
