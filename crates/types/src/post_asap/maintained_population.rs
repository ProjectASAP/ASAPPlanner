//! Language-independent maintained populations and their evaluations.
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
pub enum PopulationStatistic {
    Quantile { q: f64 },
    TopK { k: usize },
    Sum,
    Count,
    Average,
}

/// Membership is part of state identity. Table rows must never acquire implicit
/// latest-per-series selection, stale markers, or a PromQL lookback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PopulationInput<N = crate::ir::OperatorNode> {
    CurrentSeries(CurrentSeriesInput),
    Rows {
        input: std::rc::Rc<N>,
        value_column: usize,
        grouping: crate::pre_asap::GroupKeys,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaintainedPopulation<N = crate::ir::OperatorNode> {
    pub input: PopulationInput<N>,
    pub max_k: usize,
    pub quantiles: bool,
}

impl CurrentSeriesInput {
    /// Verify the named contract against the canonical maintenance input.
    pub fn matches_node(&self, input: &crate::ir::OperatorNode) -> bool {
        use crate::ir::{NonASAPOp, Operator, ScalarExpr, TimeRangeKind};
        use crate::pre_asap::{CompareOpKind, DataType, ScalarValue, Source};
        let input = match &input.operator {
            Operator::NonASAP(NonASAPOp::TimeRange {
                range,
                kind: TimeRangeKind::Instant,
                child,
            }) if self.lookback_ms > 0
                && *range == std::time::Duration::from_millis(self.lookback_ms) =>
            {
                child.as_ref()
            }
            Operator::NonASAP(NonASAPOp::TimeRange { .. }) => return false,
            _ if self.lookback_ms == 300_000 => input,
            _ => return false,
        };
        let Operator::NonASAP(NonASAPOp::Scan {
            source: Source::TimeSeries { metric },
            predicates,
            schema,
        }) = &input.operator
        else {
            return false;
        };
        if self.metric.is_empty()
            || *metric != self.metric
            || (schema.closed && !schema.has_promql_series_identity())
            || schema.time_index.is_none()
        {
            return false;
        }
        if self.grouping.iter().any(|label| {
            !schema
                .fields
                .iter()
                .any(|c| c.name == *label && c.dtype == DataType::Utf8)
        }) {
            return false;
        }
        let mut matchers = Vec::new();
        for predicate in predicates {
            let ScalarExpr::Compare {
                left, op, right, ..
            } = &predicate.0
            else {
                return false;
            };
            let (ScalarExpr::Column(col), ScalarExpr::Literal(ScalarValue::Utf8(value))) =
                (left.as_ref(), right.as_ref())
            else {
                return false;
            };
            let Some(column) = schema.fields.get(*col) else {
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

impl MaintainedPopulation<crate::ir::OperatorNode> {
    /// Whether `input` is the maintenance input this population declares.
    pub fn matches_node(&self, input: &crate::ir::OperatorNode) -> bool {
        use crate::ir::{NonASAPOp, Operator};
        use crate::pre_asap::{DataType, Source};
        match &self.input {
            PopulationInput::CurrentSeries(spec) => spec.matches_node(input),
            PopulationInput::Rows {
                input: expected,
                value_column,
                grouping,
            } => {
                let Operator::NonASAP(NonASAPOp::Scan {
                    source: Source::Table { .. },
                    schema,
                    ..
                }) = &input.operator
                else {
                    return false;
                };
                // The same computation, whatever accuracy or timing has
                // been attached to the node since.
                let same_source =
                    expected.operator == input.operator && expected.schema == input.schema;
                same_source
                    && schema.closed
                    && schema
                        .fields
                        .get(*value_column)
                        .is_some_and(|c| c.dtype == DataType::Float64 && !c.nullable)
                    && !grouping.is_without()
                    && grouping.keys().iter().all(|k| *k < schema.fields.len())
            }
        }
    }

    pub fn supports(&self, evaluation: &PopulationStatistic) -> bool {
        match evaluation {
            PopulationStatistic::Quantile { q } => self.quantiles && q.is_finite(),
            PopulationStatistic::TopK { k } => *k <= self.max_k,
            PopulationStatistic::Sum
            | PopulationStatistic::Count
            | PopulationStatistic::Average => true,
        }
    }
}
