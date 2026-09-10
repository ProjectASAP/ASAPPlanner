//! MetricsQL AST to canonical `QueryExpr` frontend.

use std::{rc::Rc, time::Duration};

use asap_types::pre_asap::{
    resolve_root, AggIntent, ArithmeticOpKind, BinaryOpKind, ColumnRef, CompareOpKind, GroupKeys,
    Predicate, PromQLVectorSetOpKind, QueryExpr, Reduction, ScalarValue, Source,
    UnresolvedQueryExpr as U,
};
use asap_types::types::AccuracyTarget;
use metricsql_parser::ast::{AggregateModifier, DurationExpr, Expr, MetricExpr, RollupExpr};
use metricsql_parser::functions::{AggregateFunction, BuiltinFunction, RollupFunction};
use metricsql_parser::label::{LabelFilter, LabelFilterOp, NAME_LABEL};
use thiserror::Error;

pub use metricsql_parser::ast::Expr as MetricsqlExpr;

#[derive(Debug, Error)]
pub enum MetricsqlError {
    #[error("MetricsQL parse error: {0}")]
    Parse(String),
    #[error("unsupported MetricsQL feature: {0}")]
    UnsupportedFeature(String),
    #[error("MetricsQL column resolution failed: {0}")]
    Resolve(String),
}

pub fn parse_metricsql(query: &str) -> Result<MetricsqlExpr, MetricsqlError> {
    metricsql_parser::parser::parse(query).map_err(|e| MetricsqlError::Parse(e.to_string()))
}

pub fn canonical_metricsql(query: &str) -> Result<String, MetricsqlError> {
    Ok(parse_metricsql(query)?.to_string())
}

pub fn lower_metricsql(query: &str, accuracy: AccuracyTarget) -> Result<QueryExpr, MetricsqlError> {
    let ast = parse_metricsql(query)?;
    let unresolved = Lowerer { accuracy }.lower(&ast)?;
    resolve_root(&unresolved).map_err(|e| MetricsqlError::Resolve(e.to_string()))
}

struct Lowerer {
    accuracy: AccuracyTarget,
}

impl Lowerer {
    fn lower(&self, expr: &Expr) -> Result<U, MetricsqlError> {
        match expr {
            Expr::MetricExpression(e) => self.metric(e),
            Expr::Rollup(e) => self.rollup(e),
            Expr::Function(e) => self.function(e),
            Expr::Aggregation(e) => self.aggregate(e),
            Expr::NumberLiteral(e) => Ok(U::promql_scalar(e.value)),
            Expr::UnaryOperator(e) => Ok(U::BinaryOp {
                op: BinaryOpKind::Arithmetic(ArithmeticOpKind::Mul),
                lhs: Rc::new(self.lower(&e.expr)?),
                rhs: Rc::new(U::promql_scalar(-1.0)),
                vector_match: None,
            }),
            Expr::BinaryOperator(e) => self.binary(e),
            Expr::Parens(e) if e.expressions.len() == 1 => self.lower(&e.expressions[0]),
            Expr::With(e) => self.lower(&e.expr),
            other => Err(unsupported(format!("AST node `{other}`"))),
        }
    }

    fn metric(&self, metric: &MetricExpr) -> Result<U, MetricsqlError> {
        if metric.has_or_matchers() {
            return Err(unsupported("or-delimited selector matchers"));
        }
        let name = metric
            .metric_name()
            .ok_or_else(|| unsupported("selector without one exact metric name"))?;
        let mut filters: Vec<_> = metric
            .matchers
            .filter_iter()
            .filter(|f| f.label != NAME_LABEL)
            .collect();
        filters.sort_by(|a, b| a.label.cmp(&b.label).then(a.value.cmp(&b.value)));
        Ok(U::Scan {
            source: Source::TimeSeries {
                metric: name.to_owned(),
            },
            predicates: filters
                .into_iter()
                .map(|f| Predicate(Rc::new(matcher(f))))
                .collect(),
            schema: None,
        })
    }

    fn rollup(&self, rollup: &RollupExpr) -> Result<U, MetricsqlError> {
        if rollup.offset.is_some() || rollup.at.is_some() {
            return Err(unsupported("offset and @ modifiers"));
        }
        if rollup.for_subquery() {
            return Err(unsupported("subquery step or inherited step"));
        }
        let child = self.lower(&rollup.expr)?;
        match &rollup.window {
            None => Ok(child),
            Some(window) => Ok(U::TimeRange {
                range: duration(window)?,
                child: Rc::new(child),
            }),
        }
    }

    fn function(
        &self,
        function: &metricsql_parser::ast::FunctionExpr,
    ) -> Result<U, MetricsqlError> {
        if function.keep_metric_names {
            return Err(unsupported(
                "keep_metric_names requires metric-name lineage",
            ));
        }
        let BuiltinFunction::Rollup(rollup) = function.function else {
            return Err(unsupported(format!("function `{}`", function.name())));
        };
        let expected_args = if rollup == RollupFunction::QuantileOverTime {
            2
        } else {
            1
        };
        require_arity(function.name(), function.args.len(), expected_args)?;
        let child_index = usize::from(rollup == RollupFunction::QuantileOverTime);
        let child = function
            .args
            .get(child_index)
            .ok_or_else(|| unsupported(format!("missing argument for `{}`", function.name())))?;
        let intent = match rollup {
            RollupFunction::DefaultRollup | RollupFunction::LastOverTime => AggIntent::LastOverTime,
            RollupFunction::FirstOverTime => AggIntent::FirstOverTime,
            RollupFunction::AvgOverTime => AggIntent::Avg { col: None },
            RollupFunction::MinOverTime => AggIntent::Min { col: None },
            RollupFunction::MaxOverTime => AggIntent::Max { col: None },
            RollupFunction::SumOverTime => AggIntent::Sum { col: None },
            RollupFunction::CountOverTime => AggIntent::Count {
                accuracy: self.accuracy.clone(),
            },
            RollupFunction::StddevOverTime => AggIntent::StdDev {
                col: None,
                population: true,
            },
            RollupFunction::StdvarOverTime => AggIntent::Variance {
                col: None,
                population: true,
            },
            RollupFunction::Rate => AggIntent::Rate,
            RollupFunction::IRate => AggIntent::IRate,
            RollupFunction::Increase => AggIntent::Increase,
            RollupFunction::Changes => AggIntent::Changes,
            RollupFunction::Delta => AggIntent::Delta,
            RollupFunction::IDelta => AggIntent::IDelta,
            RollupFunction::Deriv => AggIntent::Deriv,
            RollupFunction::Resets => AggIntent::Resets,
            RollupFunction::MadOverTime => AggIntent::MadOverTime,
            RollupFunction::PresentOverTime => AggIntent::PresentOverTime,
            RollupFunction::AbsentOverTime => AggIntent::AbsentOverTime,
            RollupFunction::QuantileOverTime => AggIntent::Quantile {
                col: None,
                q: number_arg(&function.args, 0)?,
                accuracy: self.accuracy.clone(),
            },
            _ => {
                return Err(unsupported(format!(
                    "rollup function `{}`",
                    function.name()
                )))
            }
        };
        let child = self.lower(child)?;
        if rollup == RollupFunction::DefaultRollup && !matches!(child, U::TimeRange { .. }) {
            return Err(unsupported(
                "default_rollup without an explicit range requires an evaluation step",
            ));
        }
        Ok(aggregate(Reduction::PerEntity, intent, child))
    }

    fn aggregate(
        &self,
        expr: &metricsql_parser::ast::AggregationExpr,
    ) -> Result<U, MetricsqlError> {
        if expr.limit != 0 || expr.keep_metric_names {
            return Err(unsupported("aggregate limit or keep_metric_names"));
        }
        let expected_args = if expr.function == AggregateFunction::Quantile {
            2
        } else {
            1
        };
        require_arity(expr.name(), expr.args.len(), expected_args)?;
        let child_index = expr
            .arg_idx_for_optimization()
            .ok_or_else(|| unsupported(format!("aggregate `{}` arguments", expr.name())))?;
        let intent = match expr.function {
            AggregateFunction::Sum => AggIntent::Sum { col: None },
            AggregateFunction::Avg => AggIntent::Avg { col: None },
            AggregateFunction::Min => AggIntent::Min { col: None },
            AggregateFunction::Max => AggIntent::Max { col: None },
            AggregateFunction::Count => AggIntent::Cardinality {
                col: None,
                accuracy: self.accuracy.clone(),
            },
            AggregateFunction::StdDev => AggIntent::StdDev {
                col: None,
                population: true,
            },
            AggregateFunction::StdVar => AggIntent::Variance {
                col: None,
                population: true,
            },
            AggregateFunction::Group => AggIntent::Group,
            AggregateFunction::Quantile => AggIntent::Quantile {
                col: None,
                q: number_arg(&expr.args, 0)?,
                accuracy: self.accuracy.clone(),
            },
            _ => return Err(unsupported(format!("aggregate `{}`", expr.name()))),
        };
        let reduction = match &expr.modifier {
            None => Reduction::by(vec![]),
            Some(AggregateModifier::By(v)) => Reduction::by(names(v)),
            Some(AggregateModifier::Without(v)) => Reduction::Reduce(GroupKeys::without(names(v))),
        };
        let child = expr
            .args
            .get(child_index)
            .ok_or_else(|| unsupported("missing aggregate input"))?;
        Ok(aggregate(reduction, intent, self.lower(child)?))
    }

    fn binary(&self, expr: &metricsql_parser::ast::BinaryExpr) -> Result<U, MetricsqlError> {
        if expr.modifier.is_some() {
            return Err(unsupported("binary vector matching modifiers"));
        }
        use metricsql_parser::ast::Operator as O;
        let op = match expr.op {
            O::Add => BinaryOpKind::Arithmetic(ArithmeticOpKind::Add),
            O::Sub => BinaryOpKind::Arithmetic(ArithmeticOpKind::Sub),
            O::Mul => BinaryOpKind::Arithmetic(ArithmeticOpKind::Mul),
            O::Div => BinaryOpKind::Arithmetic(ArithmeticOpKind::Div),
            O::Mod => BinaryOpKind::Arithmetic(ArithmeticOpKind::Mod),
            O::Pow => BinaryOpKind::Arithmetic(ArithmeticOpKind::Pow),
            O::Atan2 => BinaryOpKind::Arithmetic(ArithmeticOpKind::Atan2),
            O::Eql => BinaryOpKind::Compare(CompareOpKind::Eq),
            O::NotEq => BinaryOpKind::Compare(CompareOpKind::Ne),
            O::Lt => BinaryOpKind::Compare(CompareOpKind::Lt),
            O::Lte => BinaryOpKind::Compare(CompareOpKind::Le),
            O::Gt => BinaryOpKind::Compare(CompareOpKind::Gt),
            O::Gte => BinaryOpKind::Compare(CompareOpKind::Ge),
            O::And => BinaryOpKind::Set(PromQLVectorSetOpKind::And),
            O::Or => BinaryOpKind::Set(PromQLVectorSetOpKind::Or),
            O::Unless => BinaryOpKind::Set(PromQLVectorSetOpKind::Unless),
            O::If | O::IfNot | O::Default => {
                return Err(unsupported(format!("MetricsQL operator `{}`", expr.op)))
            }
        };
        Ok(U::BinaryOp {
            op,
            lhs: Rc::new(self.lower(&expr.left)?),
            rhs: Rc::new(self.lower(&expr.right)?),
            vector_match: None,
        })
    }
}

fn names(values: &[String]) -> Vec<ColumnRef> {
    values.iter().cloned().map(ColumnRef::Named).collect()
}

fn aggregate(reduction: Reduction<ColumnRef>, intent: AggIntent<ColumnRef>, child: U) -> U {
    U::Aggregate {
        reduction,
        measures: vec![intent],
        output_names: vec![String::new()],
        having: None,
        child: Rc::new(child),
    }
}

fn matcher(filter: &LabelFilter) -> U {
    let op = match filter.op {
        LabelFilterOp::Equal => CompareOpKind::Eq,
        LabelFilterOp::NotEqual => CompareOpKind::Ne,
        LabelFilterOp::RegexEqual => CompareOpKind::Regex,
        LabelFilterOp::RegexNotEqual => CompareOpKind::NotRegex,
    };
    U::Compare {
        left: Rc::new(U::Column(ColumnRef::Named(filter.label.clone()))),
        op,
        right: Rc::new(U::Literal(ScalarValue::Utf8(filter.value.clone()))),
    }
}

fn duration(value: &DurationExpr) -> Result<Duration, MetricsqlError> {
    match value {
        DurationExpr::Millis(ms) if *ms >= 0 => Ok(Duration::from_millis(*ms as u64)),
        DurationExpr::StepValue(_) => Err(unsupported("step-relative duration")),
        DurationExpr::Millis(_) => Err(unsupported("negative duration")),
    }
}

fn number_arg(args: &[Expr], index: usize) -> Result<f64, MetricsqlError> {
    match args.get(index) {
        Some(Expr::NumberLiteral(v)) if v.value.is_finite() => Ok(v.value),
        _ => Err(unsupported(format!("numeric argument #{index}"))),
    }
}

fn require_arity(name: &str, actual: usize, expected: usize) -> Result<(), MetricsqlError> {
    if actual == expected {
        Ok(())
    } else {
        Err(unsupported(format!(
            "`{name}` with {actual} arguments; canonical lowering requires exactly {expected}"
        )))
    }
}

fn unsupported(message: impl Into<String>) -> MetricsqlError {
    MetricsqlError::UnsupportedFeature(message.into())
}
