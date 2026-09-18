//! Language-independent maintained populations and their readouts.
//! Resource limits, ingestion placement and data structures belong to the executor.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentSeriesInput {
    pub metric: String,
    pub matchers: Vec<CurrentSeriesMatcher>,
    pub grouping: Vec<String>,
    pub without: bool,
    pub lookback_ms: u64,
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
pub enum PopulationReadout {
    Quantile { q: f64 },
    TopK { k: usize },
    Sum,
    Count,
    Average,
}

impl CurrentSeriesInput {
    /// Verify the named contract against the canonical maintenance input.
    pub fn matches_input(&self, input: &crate::pre_asap::QueryExpr) -> bool {
        use crate::pre_asap::{CompareOpKind, DataType, QueryExpr, ScalarValue, Source};
        // PromQL instant selectors carry an ingestion-interval `TimeRange` as
        // their input scope. The population must use the same expiry horizon;
        // shifted and otherwise transformed inputs still fail below.
        let input = match input {
            QueryExpr::TimeRange { range, child }
                if self.lookback_ms > 0
                    && *range == std::time::Duration::from_millis(self.lookback_ms) =>
            {
                child.as_ref()
            }
            QueryExpr::TimeRange { .. } => return false,
            other if self.lookback_ms == 300_000 => other,
            _ => return false,
        };
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
}

/// Membership is part of state identity. Table rows must never acquire implicit
/// latest-per-series selection, stale markers, or a PromQL lookback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PopulationInput {
    CurrentSeries(CurrentSeriesInput),
    Rows {
        input: std::rc::Rc<crate::pre_asap::QueryExpr>,
        value_column: usize,
        grouping: crate::pre_asap::GroupKeys,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaintainedPopulation {
    pub input: PopulationInput,
    pub max_k: usize,
    pub quantiles: bool,
}

impl MaintainedPopulation {
    pub fn matches_input(&self, input: &crate::pre_asap::QueryExpr) -> bool {
        match &self.input {
            PopulationInput::CurrentSeries(spec) => spec.matches_input(input),
            PopulationInput::Rows {
                input: expected,
                value_column,
                grouping,
            } => {
                use crate::pre_asap::{DataType, QueryExpr, Source};
                expected.as_ref() == input
                    && matches!(input, QueryExpr::Scan { source: Source::Table { .. }, schema, .. }
                        if schema.closed && schema.columns.get(*value_column).is_some_and(|c| c.dtype == DataType::Float64 && !c.nullable)
                            && !grouping.is_without() && grouping.keys().iter().all(|k| *k < schema.columns.len()))
            }
        }
    }

    pub fn supports(&self, readout: &PopulationReadout) -> bool {
        match readout {
            PopulationReadout::Quantile { q } => self.quantiles && q.is_finite(),
            PopulationReadout::TopK { k } => *k <= self.max_k,
            PopulationReadout::Sum | PopulationReadout::Count | PopulationReadout::Average => true,
        }
    }
}
