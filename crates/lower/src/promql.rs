//! Layers 1→3 lowering: PromQL string → intent-algebra `QueryExpr`.
//!
//! - **L1 (parse)** is delegated to the `promql-parser` crate, which produces a
//!   PromQL-specific AST (`promql_parser::parser::Expr`).
//! - **L2 (per-language tree)** is that same AST — `promql-parser` already hands
//!   us a typed, language-flavored tree (instant vs range vectors, aggregate
//!   operators, label matchers), so no separate L2 structure is materialised.
//!   This mirrors the SQL path (PR #4), which uses DataFusion's `LogicalPlan` as
//!   its free L2.
//! - **L3 (intent algebra)** is what this module emits: the language- and
//!   deployment-independent [`QueryExpr`] with intent-only [`AggIntent`]s and a
//!   `Source::TimeSeries` leaf. No sketch types, no sketch parameters.
//!
//! # PromQL → L3 mapping (summary)
//!
//! | PromQL | L3 shape |
//! |---|---|
//! | `quantile_over_time(φ, m{f}[w])` | `TimeWindow{w} → Aggregate{[Quantile{φ}]}` |
//! | `histogram_quantile(φ, rate(m{f}[w]))` | `TimeWindow{w} → Aggregate{[Quantile{φ}]}` |
//! | `avg/min/max/sum_over_time(m[w])` | `TimeWindow{w} → Aggregate{[Avg/Min/Max/Sum]}` |
//! | `stddev/stdvar_over_time(m[w])` | `TimeWindow{w} → Aggregate{[StdDev/Variance]}` |
//! | `count_over_time(m[w])` | `TimeWindow{w} → Aggregate{[Count]}` |
//! | `changes/resets(m[w])` | `TimeWindow{w} → Aggregate{[Count]}` |
//! | `rate/irate/increase(m[w])` | `Aggregate{[Rate{w}/Increase{w}]}` (window in intent) |
//! | `OUTER by (dims) (…)` | grouping `dims` flow onto the inner `Aggregate.by` |
//! | `count by (d) (… )` | `Aggregate{by:d, [Cardinality]}` |
//! | `topk(k, count_over_time(…))` | `Aggregate{[TopK{k}]}` (heavy-hitter, one pass) |
//! | `topk(k, <other>)` / `bottomk(k, …)` | generic `Sort{value} → Limit{k}` |
//! | `m{f}` bare | `Scan{TimeSeries, predicates}` |
//! | `a OP b` | `BinaryOp{vector_match}` |
//! | `expr[r:res]` | `TimeWindow{Sliding, r, res}` |

use std::sync::Arc;
use std::time::Duration;

use promql_parser::label::{MatchOp, Matcher};
use promql_parser::parser::{
    self, token, AggregateExpr, BinaryExpr, Call, Expr, LabelModifier, VectorMatchCardinality,
    VectorSelector,
};

use asap_control_core::intent_algebra::expr::{
    AggIntent, BinaryOpKind, ColumnRef, GroupKey, GroupSide, L3Node, MetricRef, Predicate,
    QueryExpr, SortKey, Source, TimeWindowKind, VectorGrouping, VectorMatch,
};
use asap_control_core::intent_algebra::schema::{L3Schema, SchemaCatalog};
use asap_control_core::intent_algebra::{CompareOp, L3Expr, L3Scalar};
use asap_control_core::types::AccuracyTarget;

use crate::error::LoweringError;

type Result<T> = std::result::Result<T, LoweringError>;

/// Lowers one PromQL query string into an intent-algebra `QueryExpr`.
///
/// `catalog` is consulted only to resolve `without(...)` grouping into an
/// explicit `by` list (which needs the metric's full label set); everything
/// else lowers without it. `accuracy` is attached to every approximate
/// `AggIntent` (`Quantile`, `Count`, `Cardinality`, `TopK`).
pub struct PromqlLowerer<'a> {
    catalog: &'a SchemaCatalog,
    accuracy: AccuracyTarget,
}

/// The aggregate "shape" an outer PromQL aggregator imposes on its argument.
#[derive(Debug, Clone)]
enum Outer {
    /// No enclosing aggregator (top-level call / bare selector).
    None,
    /// Value aggregator (`sum`/`avg`/`min`/`max`/`group`/`stddev`/`stdvar`/
    /// `quantile`). When the argument is itself an `*_over_time` call the inner
    /// function's intent wins; otherwise this op becomes the intent.
    Plain(OuterIntent),
    /// `count(...)` → set cardinality.
    Count,
    /// `topk` (`descending`) / `bottomk` (ascending) with limit `k`.
    TopK { k: usize, descending: bool },
}

#[derive(Debug, Clone)]
enum OuterIntent {
    Sum,
    Avg,
    Min,
    Max,
    StdDev,
    Variance,
    Quantile(f64),
}

/// The aggregation a PromQL function over a range vector implies.
#[derive(Debug, Clone)]
enum InnerFunc {
    Quantile(f64),
    Avg,
    Min,
    Max,
    Sum,
    StdDev,
    Variance,
    Count,
    Rate(Duration),
    Increase(Duration),
}

/// A lowered range/instant vector argument: the leaf metric, its label-matcher
/// predicates, an optional window (from a range vector), and the aggregation
/// implied by any enclosing function.
struct Inner {
    metric: String,
    predicates: Vec<Predicate>,
    window: Option<Duration>,
    func: Option<InnerFunc>,
}

impl<'a> PromqlLowerer<'a> {
    pub fn new(catalog: &'a SchemaCatalog, accuracy: AccuracyTarget) -> Self {
        Self { catalog, accuracy }
    }

    /// Parse (L1) and lower (L2→L3) a PromQL query string.
    pub fn lower(&self, query: &str) -> Result<QueryExpr> {
        let ast = parser::parse(query).map_err(LoweringError::Parse)?;
        self.walk(&ast)
    }

    fn walk(&self, expr: &Expr) -> Result<QueryExpr> {
        match expr {
            Expr::Aggregate(agg) => self.walk_aggregate(agg),
            Expr::Call(call) => {
                let inner = self.lower_inner_call(call)?;
                self.build(inner, vec![], Outer::None)
            }
            Expr::Binary(bin) => self.walk_binary(bin),
            Expr::Paren(p) => self.walk(&p.expr),
            Expr::Unary(u) => self.walk(&u.expr),
            // `expr[range:resolution]` — a sliding evaluation window.
            Expr::Subquery(sq) => {
                let inner = self.walk(&sq.expr)?;
                Ok(QueryExpr::TimeWindow {
                    child: node(inner),
                    kind: TimeWindowKind::Sliding,
                    size: sq.range,
                    slide: sq.step,
                })
            }
            // Bare instant selector → Scan with label-matcher predicates.
            Expr::VectorSelector(vs) => {
                let (metric, predicates) = vs_to_scan(vs);
                Ok(scan(metric, predicates))
            }
            // Bare range selector (no enclosing function) → windowed scan.
            Expr::MatrixSelector(ms) => {
                let (metric, predicates) = vs_to_scan(&ms.vs);
                Ok(QueryExpr::TimeWindow {
                    child: node(scan(metric, predicates)),
                    kind: TimeWindowKind::Tumbling,
                    size: ms.range,
                    slide: None,
                })
            }
            Expr::NumberLiteral(_) | Expr::StringLiteral(_) => Err(
                LoweringError::UnsupportedFeature("bare scalar/string at top level".into()),
            ),
            Expr::Extension(_) => Err(LoweringError::UnsupportedFeature(
                "extension expression".into(),
            )),
        }
    }

    fn walk_aggregate(&self, agg: &AggregateExpr) -> Result<QueryExpr> {
        let group = self.resolve_group(agg)?;
        let inner = self.lower_inner(&agg.expr)?;
        let op = agg.op.id();

        let outer = if op == token::T_TOPK {
            Outer::TopK {
                k: num_param(agg)? as usize,
                descending: true,
            }
        } else if op == token::T_BOTTOMK {
            Outer::TopK {
                k: num_param(agg)? as usize,
                descending: false,
            }
        } else if op == token::T_COUNT {
            Outer::Count
        } else if op == token::T_SUM || op == token::T_GROUP {
            Outer::Plain(OuterIntent::Sum)
        } else if op == token::T_AVG {
            Outer::Plain(OuterIntent::Avg)
        } else if op == token::T_MIN {
            Outer::Plain(OuterIntent::Min)
        } else if op == token::T_MAX {
            Outer::Plain(OuterIntent::Max)
        } else if op == token::T_STDDEV {
            Outer::Plain(OuterIntent::StdDev)
        } else if op == token::T_STDVAR {
            Outer::Plain(OuterIntent::Variance)
        } else if op == token::T_QUANTILE {
            Outer::Plain(OuterIntent::Quantile(num_param(agg)?))
        } else {
            return Err(LoweringError::UnsupportedAggregateOp(format!(
                "aggregate token {op}"
            )));
        };

        self.build(inner, group, outer)
    }

    fn walk_binary(&self, bin: &BinaryExpr) -> Result<QueryExpr> {
        let lhs = self.walk(&bin.lhs)?;
        let rhs = self.walk(&bin.rhs)?;
        let op = binop(bin.op.id())?;
        let vector_match = bin.modifier.as_ref().map(|m| {
            let (on, labels) = match &m.matching {
                Some(LabelModifier::Include(ls)) => (true, ls.labels.clone()),
                Some(LabelModifier::Exclude(ls)) => (false, ls.labels.clone()),
                None => (true, vec![]),
            };
            let grouping = match &m.card {
                VectorMatchCardinality::ManyToOne(ls) => Some(VectorGrouping {
                    side: GroupSide::Left,
                    labels: ls.labels.clone(),
                }),
                VectorMatchCardinality::OneToMany(ls) => Some(VectorGrouping {
                    side: GroupSide::Right,
                    labels: ls.labels.clone(),
                }),
                _ => None,
            };
            VectorMatch {
                on,
                labels,
                grouping,
            }
        });
        Ok(QueryExpr::BinaryOp {
            op,
            lhs: node(lhs),
            rhs: node(rhs),
            vector_match,
        })
    }

    /// Lower a function/selector argument into an `Inner`.
    fn lower_inner(&self, expr: &Expr) -> Result<Inner> {
        match expr {
            Expr::VectorSelector(vs) => {
                let (metric, predicates) = vs_to_scan(vs);
                Ok(Inner {
                    metric,
                    predicates,
                    window: None,
                    func: None,
                })
            }
            Expr::MatrixSelector(ms) => {
                let (metric, predicates) = vs_to_scan(&ms.vs);
                Ok(Inner {
                    metric,
                    predicates,
                    window: Some(ms.range),
                    func: None,
                })
            }
            Expr::Paren(p) => self.lower_inner(&p.expr),
            Expr::Call(call) => self.lower_inner_call(call),
            other => Err(LoweringError::UnsupportedFeature(format!(
                "aggregate argument: {:?}",
                std::mem::discriminant(other)
            ))),
        }
    }

    fn lower_inner_call(&self, call: &Call) -> Result<Inner> {
        let name = call.func.name;
        // Functions whose range-vector argument is at index 0.
        let at0 = |func: InnerFunc| -> Result<Inner> {
            let (metric, predicates, window) = extract_matrix(arg(call, 0)?)?;
            Ok(Inner {
                metric,
                predicates,
                window: Some(window),
                func: Some(func),
            })
        };
        match name {
            "rate" | "irate" => {
                let (metric, predicates, window) = extract_matrix(arg(call, 0)?)?;
                Ok(Inner {
                    metric,
                    predicates,
                    window: Some(window),
                    func: Some(InnerFunc::Rate(window)),
                })
            }
            "increase" => {
                let (metric, predicates, window) = extract_matrix(arg(call, 0)?)?;
                Ok(Inner {
                    metric,
                    predicates,
                    window: Some(window),
                    func: Some(InnerFunc::Increase(window)),
                })
            }
            "quantile_over_time" => {
                let phi = num_arg(call, 0)?;
                let (metric, predicates, window) = extract_matrix(arg(call, 1)?)?;
                Ok(Inner {
                    metric,
                    predicates,
                    window: Some(window),
                    func: Some(InnerFunc::Quantile(phi)),
                })
            }
            // Substitute histogram_quantile(φ, buckets) with a plain Quantile(φ)
            // over the bucket stream; bucket-aware physical reduction is an L4/L5
            // concern, not an L3 IR variant.
            "histogram_quantile" => {
                let phi = num_arg(call, 0)?;
                let (metric, predicates, window) = extract_matrix(arg(call, 1)?)?;
                Ok(Inner {
                    metric,
                    predicates,
                    window: Some(window),
                    func: Some(InnerFunc::Quantile(phi)),
                })
            }
            "avg_over_time" => at0(InnerFunc::Avg),
            "min_over_time" => at0(InnerFunc::Min),
            "max_over_time" => at0(InnerFunc::Max),
            "sum_over_time" => at0(InnerFunc::Sum),
            "stddev_over_time" => at0(InnerFunc::StdDev),
            "stdvar_over_time" => at0(InnerFunc::Variance),
            "count_over_time" | "changes" | "resets" => at0(InnerFunc::Count),
            other => Err(LoweringError::UnsupportedFunction(other.to_string())),
        }
    }

    /// Assemble the final `QueryExpr` from a lowered inner vector, the resolved
    /// group keys, and the enclosing aggregator shape.
    fn build(&self, inner: Inner, group: Vec<GroupKey>, outer: Outer) -> Result<QueryExpr> {
        match outer {
            Outer::None => match &inner.func {
                None => Ok(scan(inner.metric, inner.predicates)),
                Some(f) => {
                    let intent = self.inner_intent(f);
                    Ok(self.windowed_aggregate(inner, group, vec![intent]))
                }
            },
            Outer::Plain(outer_intent) => {
                let intent = match &inner.func {
                    Some(f) => self.inner_intent(f),
                    None => self.outer_intent(&outer_intent),
                };
                Ok(self.windowed_aggregate(inner, group, vec![intent]))
            }
            Outer::Count => Ok(self.windowed_aggregate(
                inner,
                group,
                vec![AggIntent::Cardinality {
                    accuracy: self.accuracy.clone(),
                }],
            )),
            Outer::TopK { k, descending } => {
                // Heavy-hitter only when ranking by frequency (`count`): a
                // dedicated sketch (SpaceSaving, CMS-with-heap) serves it in one
                // pass, so it earns the first-class `AggIntent::TopK`. Any other
                // ranking (`topk` over avg/quantile, all `bottomk`) is a generic
                // order-by-value + limit, which has no sketch primitive — per the
                // L3 design rule that keeps `Sort + Limit` distinct from `TopK`.
                let heavy_hitter = descending && matches!(inner.func, Some(InnerFunc::Count));
                if heavy_hitter {
                    let by: Vec<ColumnRef> = group.iter().map(|g| ColumnRef(g.0.clone())).collect();
                    let agg = vec![AggIntent::TopK {
                        k,
                        by,
                        accuracy: self.accuracy.clone(),
                    }];
                    // Window over Aggregate, same as `windowed_aggregate`, but
                    // with an empty group list (the heavy-hitter keys live in
                    // the `TopK` intent itself).
                    Ok(self.windowed_aggregate(inner, vec![], agg))
                } else {
                    let intent = match &inner.func {
                        Some(f) => self.inner_intent(f),
                        None => AggIntent::Sum,
                    };
                    let base = self.windowed_aggregate(inner, group, vec![intent]);
                    let sorted = QueryExpr::Sort {
                        child: node(base),
                        keys: vec![SortKey {
                            expr: L3Expr::Column(ColumnRef("value".into())),
                            ascending: !descending,
                            nulls_first: false,
                        }],
                    };
                    Ok(QueryExpr::Limit {
                        child: node(sorted),
                        n: Some(k as u64),
                        offset: 0,
                    })
                }
            }
        }
    }

    /// `[TimeWindow →] Aggregate → Scan`. The canonical windowed-aggregate
    /// shape is **Window over Aggregate** (the window defines the flush/reset
    /// lifecycle of the aggregate in its sub-DAG). Rate/Increase carry their own
    /// window in the intent, so no `TimeWindow` node is emitted for them.
    fn windowed_aggregate(
        &self,
        inner: Inner,
        group: Vec<GroupKey>,
        aggs: Vec<AggIntent>,
    ) -> QueryExpr {
        let skip_window = matches!(
            inner.func,
            Some(InnerFunc::Rate(_)) | Some(InnerFunc::Increase(_))
        );
        let window = inner.window;
        let base = scan(inner.metric, inner.predicates);
        let agg = QueryExpr::Aggregate {
            child: node(base),
            by: group,
            aggs,
            having: None,
        };
        match window {
            Some(w) if !skip_window => QueryExpr::TimeWindow {
                child: node(agg),
                kind: TimeWindowKind::Tumbling,
                size: w,
                slide: None,
            },
            _ => agg,
        }
    }

    fn inner_intent(&self, f: &InnerFunc) -> AggIntent {
        match f {
            InnerFunc::Quantile(q) => AggIntent::Quantile {
                q: *q,
                accuracy: self.accuracy.clone(),
            },
            InnerFunc::Avg => AggIntent::Avg,
            InnerFunc::Min => AggIntent::Min,
            InnerFunc::Max => AggIntent::Max,
            InnerFunc::Sum => AggIntent::Sum,
            InnerFunc::StdDev => AggIntent::StdDev { population: false },
            InnerFunc::Variance => AggIntent::Variance { population: false },
            InnerFunc::Count => AggIntent::Count {
                accuracy: self.accuracy.clone(),
            },
            InnerFunc::Rate(w) => AggIntent::Rate { window: *w },
            InnerFunc::Increase(w) => AggIntent::Increase { window: *w },
        }
    }

    fn outer_intent(&self, o: &OuterIntent) -> AggIntent {
        match o {
            OuterIntent::Sum => AggIntent::Sum,
            OuterIntent::Avg => AggIntent::Avg,
            OuterIntent::Min => AggIntent::Min,
            OuterIntent::Max => AggIntent::Max,
            OuterIntent::StdDev => AggIntent::StdDev { population: false },
            OuterIntent::Variance => AggIntent::Variance { population: false },
            OuterIntent::Quantile(q) => AggIntent::Quantile {
                q: *q,
                accuracy: self.accuracy.clone(),
            },
        }
    }

    /// Resolve `by(labels)` / `without(labels)` into an explicit group-key list.
    /// `without` needs the metric's full label set, looked up in the catalog.
    fn resolve_group(&self, agg: &AggregateExpr) -> Result<Vec<GroupKey>> {
        match &agg.modifier {
            None => Ok(vec![]),
            Some(LabelModifier::Include(ls)) => {
                Ok(ls.labels.iter().cloned().map(GroupKey).collect())
            }
            Some(LabelModifier::Exclude(ls)) => {
                let metric = find_metric(&agg.expr).ok_or_else(|| {
                    LoweringError::UnsupportedFeature(
                        "`without` over an expression with no metric selector".into(),
                    )
                })?;
                let meta = self.catalog.metrics.get(&metric).ok_or_else(|| {
                    LoweringError::UnsupportedFeature(format!(
                        "`without(...)` requires metric '{metric}' to be registered in the catalog \
                         (its full label set is needed to compute the kept labels)"
                    ))
                })?;
                Ok(meta
                    .labels
                    .iter()
                    .filter(|l| !ls.labels.contains(l))
                    .cloned()
                    .map(GroupKey)
                    .collect())
            }
        }
    }
}

// ── Free helpers ──────────────────────────────────────────────────────────────

/// Wrap a `QueryExpr` in an untyped `L3Node` (empty schema). The schema pass
/// (`crate::populate_schemas`) fills schemas in bottom-up afterwards.
fn node(expr: QueryExpr) -> Arc<L3Node> {
    Arc::new(L3Node {
        expr,
        schema: L3Schema {
            fields: vec![],
            time_index: None,
        },
    })
}

fn scan(metric: String, predicates: Vec<Predicate>) -> QueryExpr {
    QueryExpr::Scan {
        source: Source::TimeSeries {
            metric: MetricRef(metric),
            time: None,
        },
        predicates,
    }
}

/// Extract `(metric_name, label_predicates)` from a vector selector.
fn vs_to_scan(vs: &VectorSelector) -> (String, Vec<Predicate>) {
    let metric = vs.name.clone().unwrap_or_else(|| {
        vs.matchers
            .matchers
            .iter()
            .find(|m| m.name == "__name__")
            .map(|m| m.value.clone())
            .unwrap_or_default()
    });
    let predicates = vs
        .matchers
        .matchers
        .iter()
        .filter(|m| m.name != "__name__")
        .map(matcher_to_predicate)
        .collect();
    (metric, predicates)
}

fn matcher_to_predicate(m: &Matcher) -> Predicate {
    let op = match &m.op {
        MatchOp::Equal => CompareOp::Eq,
        MatchOp::NotEqual => CompareOp::Ne,
        MatchOp::Re(_) => CompareOp::Regex,
        MatchOp::NotRe(_) => CompareOp::NotRegex,
    };
    Predicate(L3Expr::Compare {
        left: Box::new(L3Expr::Column(ColumnRef(m.name.clone()))),
        op,
        right: Box::new(L3Expr::Literal(L3Scalar::Utf8(m.value.clone()))),
    })
}

/// Descend through `Call` / `Paren` wrappers to the `MatrixSelector` and pull
/// out `(metric, predicates, window)`.
fn extract_matrix(expr: &Expr) -> Result<(String, Vec<Predicate>, Duration)> {
    match expr {
        Expr::MatrixSelector(ms) => {
            let (metric, predicates) = vs_to_scan(&ms.vs);
            Ok((metric, predicates, ms.range))
        }
        Expr::Paren(p) => extract_matrix(&p.expr),
        Expr::Call(c) => extract_matrix(arg(c, 0)?),
        other => Err(LoweringError::UnsupportedFeature(format!(
            "expected range-vector argument, got {:?}",
            std::mem::discriminant(other)
        ))),
    }
}

/// First `VectorSelector` metric name reachable from `expr`.
fn find_metric(expr: &Expr) -> Option<String> {
    match expr {
        Expr::VectorSelector(vs) => Some(vs_to_scan(vs).0),
        Expr::MatrixSelector(ms) => Some(vs_to_scan(&ms.vs).0),
        Expr::Paren(p) => find_metric(&p.expr),
        Expr::Unary(u) => find_metric(&u.expr),
        Expr::Subquery(sq) => find_metric(&sq.expr),
        Expr::Aggregate(a) => find_metric(&a.expr),
        Expr::Call(c) => c.args.args.iter().find_map(|a| find_metric(a)),
        Expr::Binary(b) => find_metric(&b.lhs).or_else(|| find_metric(&b.rhs)),
        _ => None,
    }
}

fn arg(call: &Call, idx: usize) -> Result<&Expr> {
    call.args
        .args
        .get(idx)
        .map(|b| b.as_ref())
        .ok_or_else(|| LoweringError::MissingArgument(format!("{} arg #{idx}", call.func.name)))
}

fn num_arg(call: &Call, idx: usize) -> Result<f64> {
    num_expr(arg(call, idx)?)
}

fn num_param(agg: &AggregateExpr) -> Result<f64> {
    match &agg.param {
        Some(e) => num_expr(e),
        None => Err(LoweringError::MissingArgument(
            "aggregate parameter (k / φ)".into(),
        )),
    }
}

fn num_expr(expr: &Expr) -> Result<f64> {
    match expr {
        Expr::NumberLiteral(n) => Ok(n.val),
        other => Err(LoweringError::InvalidParameter(format!(
            "expected a numeric literal, got {:?}",
            std::mem::discriminant(other)
        ))),
    }
}

fn binop(id: token::TokenId) -> Result<BinaryOpKind> {
    Ok(if id == token::T_ADD {
        BinaryOpKind::Add
    } else if id == token::T_SUB {
        BinaryOpKind::Sub
    } else if id == token::T_MUL {
        BinaryOpKind::Mul
    } else if id == token::T_DIV {
        BinaryOpKind::Div
    } else if id == token::T_MOD {
        BinaryOpKind::Mod
    } else if id == token::T_POW {
        BinaryOpKind::Pow
    } else if id == token::T_ATAN2 {
        BinaryOpKind::Atan2
    } else if id == token::T_EQLC {
        BinaryOpKind::Eq
    } else if id == token::T_NEQ {
        BinaryOpKind::NotEq
    } else if id == token::T_LSS {
        BinaryOpKind::Lt
    } else if id == token::T_LTE {
        BinaryOpKind::LtEq
    } else if id == token::T_GTR {
        BinaryOpKind::Gt
    } else if id == token::T_GTE {
        BinaryOpKind::GtEq
    } else if id == token::T_LAND {
        BinaryOpKind::And
    } else if id == token::T_LOR {
        BinaryOpKind::Or
    } else if id == token::T_LUNLESS {
        BinaryOpKind::Unless
    } else {
        return Err(LoweringError::UnsupportedFeature(format!(
            "binary operator token {id}"
        )));
    })
}
