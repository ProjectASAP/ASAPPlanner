//! Layers 1→2 lowering: PromQL string → Layer-2 `relational::QueryExpr`.
//!
//! - **L1 (parse)** is delegated to `promql-parser` 0.8.
//! - **L2 (per-language tree)** is built here: the walk interprets PromQL
//!   semantics (range vectors, aggregate operators, label matchers) and emits
//!   the language-flavored [`relational::QueryExpr`] the controller's L2→L3
//!   converter ([`convert_root`](asap_control_core::intent_algebra::convert_root))
//!   consumes. Canonicalisation (window-over-aggregate fold, GROUP-BY →
//!   `Partition`, positional name binding) happens in that converter, not here.
//!
//! # PromQL → L2 mapping (summary)
//!
//! | PromQL | L2 shape (→ canonical via `convert_root`) |
//! |---|---|
//! | `quantile_over_time(φ, m{f}[w])` | `Aggregate{[Quantile(φ)], Window{w, Filter(Source)}}` |
//! | `histogram_quantile(φ, <expr>)` | `Aggregate{[Quantile(φ)]}` over the fully-lowered `<expr>` (preserves any `sum by (le)`/`rate`) |
//! | `OUTER_op(inner_func(m[w]))` (e.g. `sum(rate(m[w]))`) | `Aggregate{[OUTER_op]}` over `Aggregate{[inner_func]}` — two levels |
//! | `avg/min/max/sum_over_time(m[w])` | `Aggregate{[Avg/Min/Max/Sum], Window{w}}` |
//! | `stddev/stdvar_over_time(m[w])` | `Aggregate{[StdDev/Variance], Window{w}}` |
//! | `count_over_time(m[w])` | `Aggregate{[Count], Window{w}}` |
//! | `rate/irate(m[w])` | `Aggregate{[Rate{w}]}` (no Window) — `irate` shares the `rate` *intent*; the avg-vs-last-two-samples difference is an L4 estimation method |
//! | `increase(m[w])` | `Aggregate{[Increase{w}]}` (no Window) |
//! | `changes` / `resets` / `group` / `offset` / `@` | **rejected** — distinct semantics with no intent-algebra representation yet |
//! | `OUTER by (dims) (…)` | `Aggregate.keys = dims` (→ `Partition` in L3) |
//! | `count by (d) (…)` | `Aggregate{[CountDistinct], …}` (→ `Cardinality`) |
//! | `topk(k, count_over_time(…))` | `TopK{k, by}` (heavy-hitter, one pass) |
//! | `topk(k, <other>)` / `bottomk(k, …)` | `Sort{value} → Limit{k}` |
//! | `m{f}` | `Filter(Source)` |
//! | `a OP b` | `BinaryOp{vector_match}` |
//! | `expr[r:res]` | `PromQLSubquery{r, res}` |

use std::time::Duration;

use promql_parser::label::{MatchOp, Matcher};
use promql_parser::parser::{
    self, token, AggregateExpr, BinaryExpr, Call, Expr, LabelModifier, VectorMatchCardinality,
    VectorSelector,
};

use asap_control_core::intent_algebra::query_expr::{
    BinaryOpKind, ColumnRef, GroupSide, VectorGrouping, VectorMatch, VectorMatchKind,
};
use asap_control_core::intent_algebra::relational::{
    AggFunc, AggItem, L2SortKey, QueryExpr as L2, SourceSpec,
};
use asap_control_core::intent_algebra::{ArithOp, CompareOp, L2Expr, L3Scalar};

use crate::error::LoweringError;

type Result<T> = std::result::Result<T, LoweringError>;

/// Parses (L1) and lowers (→ L2 relational) a PromQL query string.
pub struct PromqlLowerer;

#[derive(Debug, Clone)]
enum Outer {
    None,
    Plain(OuterIntent),
    Count,
    TopK { k: u64, descending: bool },
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

struct Inner {
    metric: String,
    matchers: Vec<L2Expr>,
    window: Option<Duration>,
    func: Option<InnerFunc>,
}

/// Maximum PromQL expression nesting depth the walker accepts. Real queries
/// nest only a handful deep; this bounds the recursive descent (`walk` and the
/// mutually-recursive helpers) so a pathologically nested query is rejected
/// rather than overflowing the stack.
const MAX_DEPTH: usize = 256;

impl PromqlLowerer {
    pub fn lower(query: &str) -> Result<L2> {
        let ast = parser::parse(query).map_err(LoweringError::Parse)?;
        // Reject over-deep nesting up front, so the (mutually-recursive) walk
        // below cannot blow the stack. The check itself recurses at most
        // `MAX_DEPTH` frames before erroring, so it is bounded too.
        check_depth(&ast, MAX_DEPTH)?;
        walk(&ast)
    }
}

/// Bounded depth check over the parser AST: errors once nesting would exceed
/// `budget` frames, descending into every child expression.
fn check_depth(expr: &Expr, budget: usize) -> Result<()> {
    let Some(budget) = budget.checked_sub(1) else {
        return Err(LoweringError::UnsupportedFeature(format!(
            "query nesting exceeds the {MAX_DEPTH}-level limit"
        )));
    };
    match expr {
        Expr::Aggregate(a) => {
            check_depth(&a.expr, budget)?;
            if let Some(p) = &a.param {
                check_depth(p, budget)?;
            }
        }
        Expr::Unary(u) => check_depth(&u.expr, budget)?,
        Expr::Binary(b) => {
            check_depth(&b.lhs, budget)?;
            check_depth(&b.rhs, budget)?;
        }
        Expr::Paren(p) => check_depth(&p.expr, budget)?,
        Expr::Subquery(s) => check_depth(&s.expr, budget)?,
        Expr::Call(c) => {
            for arg in &c.args.args {
                check_depth(arg, budget)?;
            }
        }
        Expr::MatrixSelector(_)
        | Expr::VectorSelector(_)
        | Expr::NumberLiteral(_)
        | Expr::StringLiteral(_)
        | Expr::Extension(_) => {}
    }
    Ok(())
}

fn walk(expr: &Expr) -> Result<L2> {
    match expr {
        Expr::Aggregate(agg) => walk_aggregate(agg),
        Expr::Call(call) if call.func.name == "histogram_quantile" => walk_histogram_quantile(call),
        Expr::Call(call) => build(lower_inner_call(call)?, vec![], Outer::None),
        Expr::Binary(bin) => walk_binary(bin),
        Expr::Paren(p) => walk(&p.expr),
        // `UnaryExpr` is built only by negation (`Neg`); unary `+` is folded to
        // identity and `-<literal>` to a negated `NumberLiteral`, so this always
        // wraps a vector expression whose samples must be sign-flipped. The L2
        // PromQL path has no scalar/negate node to express that (there's no
        // `-1 * x`, since `walk` rejects bare scalar operands), so reject it
        // rather than silently dropping the sign and computing `+expr`.
        Expr::Unary(_) => Err(LoweringError::UnsupportedFeature(
            "unary negation (`-expr`): no negate/scalar node in the L2 PromQL path".into(),
        )),
        Expr::Subquery(sq) => Ok(L2::PromQLSubquery {
            range: sq.range,
            resolution: sq.step,
            input: Box::new(walk(&sq.expr)?),
        }),
        Expr::VectorSelector(vs) => {
            let (metric, matchers) = vs_parts(vs)?;
            Ok(filtered_source(metric, matchers))
        }
        Expr::MatrixSelector(ms) => {
            let (metric, matchers) = vs_parts(&ms.vs)?;
            Ok(L2::Window {
                duration: ms.range,
                slide: None,
                input: Box::new(filtered_source(metric, matchers)),
            })
        }
        Expr::NumberLiteral(_) | Expr::StringLiteral(_) => Err(LoweringError::UnsupportedFeature(
            "bare scalar/string at top level".into(),
        )),
        Expr::Extension(_) => Err(LoweringError::UnsupportedFeature(
            "extension expression".into(),
        )),
    }
}

fn walk_aggregate(agg: &AggregateExpr) -> Result<L2> {
    let keys = resolve_group(agg)?;
    let inner = lower_inner(&agg.expr)?;
    let op = agg.op.id();

    let outer = if op == token::T_TOPK {
        Outer::TopK {
            k: count_param(agg)?,
            descending: true,
        }
    } else if op == token::T_BOTTOMK {
        Outer::TopK {
            k: count_param(agg)?,
            descending: false,
        }
    } else if op == token::T_COUNT {
        Outer::Count
    } else if op == token::T_SUM {
        Outer::Plain(OuterIntent::Sum)
    } else if op == token::T_GROUP {
        // `group(v)` yields a constant 1 per group (presence), not a sum of
        // values. Folding it onto `Sum` changed the result; reject until a
        // distinct group-presence intent exists.
        return Err(LoweringError::UnsupportedAggregateOp(
            "`group` (constant-1 presence) is not `sum`; no distinct intent yet".into(),
        ));
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
        Outer::Plain(OuterIntent::Quantile(quantile_param(num_param(agg)?)?))
    } else {
        return Err(LoweringError::UnsupportedAggregateOp(format!(
            "aggregate token {op}"
        )));
    };

    build(inner, keys, outer)
}

/// `histogram_quantile(φ, <expr>)` lowers `<expr>` in full — preserving any
/// `sum by (le)` / `rate` structure inside it — and wraps the result in an
/// `Aggregate{[Quantile(φ)]}`. The φ-quantile reduces across the `le` buckets,
/// so the wrapper carries no grouping keys: the usage-derived schema can't
/// enumerate the non-`le` labels to group by (the same limitation that rejects
/// `without`). This handles the canonical
/// `histogram_quantile(φ, sum by (le) (rate(m_bucket[w])))` pattern, which the
/// old "extract the matrix and substitute a bare Quantile" path could not.
fn walk_histogram_quantile(call: &Call) -> Result<L2> {
    let phi = quantile_param(num_arg(call, 0)?)?;
    let inner = walk(arg(call, 1)?)?;
    Ok(outer_aggregate(vec![], AggFunc::Quantile(phi), inner))
}

fn walk_binary(bin: &BinaryExpr) -> Result<L2> {
    let lhs = walk(&bin.lhs)?;
    let rhs = walk(&bin.rhs)?;
    let op = binop(bin.op.id())?;
    let vector_match = bin.modifier.as_ref().map(|m| {
        let (kind, labels) = match &m.matching {
            Some(LabelModifier::Include(ls)) => (VectorMatchKind::On, ls.labels.clone()),
            Some(LabelModifier::Exclude(ls)) => (VectorMatchKind::Ignoring, ls.labels.clone()),
            None => (VectorMatchKind::On, vec![]),
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
            kind,
            labels,
            grouping,
        }
    });
    Ok(L2::BinaryOp {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
        vector_match,
    })
}

fn lower_inner(expr: &Expr) -> Result<Inner> {
    match expr {
        Expr::VectorSelector(vs) => {
            let (metric, matchers) = vs_parts(vs)?;
            Ok(Inner {
                metric,
                matchers,
                window: None,
                func: None,
            })
        }
        Expr::MatrixSelector(ms) => {
            let (metric, matchers) = vs_parts(&ms.vs)?;
            Ok(Inner {
                metric,
                matchers,
                window: Some(ms.range),
                func: None,
            })
        }
        Expr::Paren(p) => lower_inner(&p.expr),
        Expr::Call(call) => lower_inner_call(call),
        other => Err(LoweringError::UnsupportedFeature(format!(
            "aggregate argument: {:?}",
            std::mem::discriminant(other)
        ))),
    }
}

fn lower_inner_call(call: &Call) -> Result<Inner> {
    let name = call.func.name;
    let at0 = |func: InnerFunc| -> Result<Inner> {
        let (metric, matchers, window) = extract_matrix(arg(call, 0)?)?;
        Ok(Inner {
            metric,
            matchers,
            window: Some(window),
            func: Some(func),
        })
    };
    match name {
        "rate" | "irate" => {
            let (metric, matchers, window) = extract_matrix(arg(call, 0)?)?;
            Ok(Inner {
                metric,
                matchers,
                window: Some(window),
                func: Some(InnerFunc::Rate(window)),
            })
        }
        "increase" => {
            let (metric, matchers, window) = extract_matrix(arg(call, 0)?)?;
            Ok(Inner {
                metric,
                matchers,
                window: Some(window),
                func: Some(InnerFunc::Increase(window)),
            })
        }
        "quantile_over_time" => {
            let phi = quantile_param(num_arg(call, 0)?)?;
            let (metric, matchers, window) = extract_matrix(arg(call, 1)?)?;
            Ok(Inner {
                metric,
                matchers,
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
        "count_over_time" => at0(InnerFunc::Count),
        // `changes` (value changes) and `resets` (counter resets) are NOT
        // sample counts — aliasing them to `count_over_time` silently produced
        // the wrong number. Reject until they have distinct intents.
        other => Err(LoweringError::UnsupportedFunction(other.to_string())),
    }
}

/// Assemble the Layer-2 tree from a lowered inner vector, the resolved group
/// keys, and the enclosing aggregator shape.
fn build(inner: Inner, keys: Vec<String>, outer: Outer) -> Result<L2> {
    match outer {
        Outer::None => match &inner.func {
            None => Ok(filtered_source(inner.metric, inner.matchers)),
            Some(f) => {
                let func = inner_func(f);
                Ok(windowed_aggregate(inner, keys, func))
            }
        },
        // An OUTER aggregation operator (`sum`/`avg`/…/`count`) over an inner
        // range-vector function (`rate`/`increase`/`*_over_time`) is a
        // two-level reduction: the inner func runs per series, the outer op
        // then aggregates across series. Collapsing them into one aggregate
        // silently drops a level — e.g. `sum(rate(m[w]))` must keep the `sum`.
        Outer::Plain(outer_intent) => Ok(match &inner.func {
            None => windowed_aggregate(inner, keys, outer_func(&outer_intent)),
            Some(f) => {
                let inner_f = inner_func(f);
                let inner_agg = windowed_aggregate(inner, vec![], inner_f);
                outer_aggregate(keys, outer_func(&outer_intent), inner_agg)
            }
        }),
        Outer::Count => Ok(match &inner.func {
            None => windowed_aggregate(inner, keys, AggFunc::CountDistinct),
            Some(f) => {
                let inner_f = inner_func(f);
                let inner_agg = windowed_aggregate(inner, vec![], inner_f);
                outer_aggregate(keys, AggFunc::CountDistinct, inner_agg)
            }
        }),
        Outer::TopK { k, descending } => {
            // Heavy-hitter only when ranking by frequency (`count`): a dedicated
            // sketch serves it in one pass → first-class `TopK`. Any other
            // ranking (topk over avg/quantile, all bottomk) is generic
            // order-by-value + limit.
            let heavy_hitter = descending && matches!(inner.func, Some(InnerFunc::Count));
            if heavy_hitter {
                let scan = window_scan(inner);
                Ok(L2::TopK {
                    k,
                    by: keys,
                    input: Box::new(scan),
                })
            } else {
                let func = match &inner.func {
                    Some(f) => inner_func(f),
                    None => AggFunc::Sum,
                };
                let base = windowed_aggregate(inner, keys, func);
                let sorted = L2::Sort {
                    keys: vec![L2SortKey {
                        expr: L2Expr::Column(ColumnRef::SampleValue),
                        ascending: !descending,
                        nulls_first: false,
                    }],
                    input: Box::new(base),
                };
                Ok(L2::Limit {
                    n: k,
                    offset: 0,
                    input: Box::new(sorted),
                })
            }
        }
    }
}

/// `Aggregate{keys, [func]}` over `[Window{w}] → Filter(Source)`. Rate/Increase
/// carry their own window in the func, so no `Window` node is emitted.
fn windowed_aggregate(inner: Inner, keys: Vec<String>, func: AggFunc) -> L2 {
    let skip_window = matches!(func, AggFunc::Rate { .. } | AggFunc::Increase { .. });
    let window = inner.window;
    let base = filtered_source(inner.metric, inner.matchers);
    let input = match window {
        Some(w) if !skip_window => L2::Window {
            duration: w,
            slide: None,
            input: Box::new(base),
        },
        _ => base,
    };
    L2::Aggregate {
        keys,
        aggs: vec![AggItem {
            // Empty alias → the converter keeps PromQL's intent-keyed output
            // names ("sum", "quantile_0_99", …) instead of overriding them.
            alias: String::new(),
            func,
            col: ColumnRef::SampleValue,
            distinct: false,
        }],
        having: None,
        input: Box::new(input),
    }
}

/// `Aggregate{keys, [func]}` directly over an existing L2 subtree — the OUTER
/// level of a two-level aggregation such as `sum(rate(…))` or the
/// `Aggregate{[Quantile]}` that wraps a `histogram_quantile` argument.
fn outer_aggregate(keys: Vec<String>, func: AggFunc, input: L2) -> L2 {
    L2::Aggregate {
        keys,
        aggs: vec![AggItem {
            // Empty alias → the converter keeps PromQL's intent-keyed output
            // names ("sum", "quantile_0_99", …) instead of overriding them.
            alias: String::new(),
            func,
            col: ColumnRef::SampleValue,
            distinct: false,
        }],
        having: None,
        input: Box::new(input),
    }
}

/// `[Window{w}] → Filter(Source)` with no aggregate (the heavy-hitter TopK
/// child — the sketch counts directly off the scan).
fn window_scan(inner: Inner) -> L2 {
    let base = filtered_source(inner.metric, inner.matchers);
    match inner.window {
        Some(w) => L2::Window {
            duration: w,
            slide: None,
            input: Box::new(base),
        },
        None => base,
    }
}

fn filtered_source(metric: String, matchers: Vec<L2Expr>) -> L2 {
    let source = L2::Source(SourceSpec::new(metric));
    if matchers.is_empty() {
        source
    } else {
        let pred = if matchers.len() == 1 {
            matchers.into_iter().next().unwrap()
        } else {
            L2Expr::BoolAnd(matchers)
        };
        L2::Filter {
            pred,
            input: Box::new(source),
        }
    }
}

fn inner_func(f: &InnerFunc) -> AggFunc {
    match f {
        InnerFunc::Quantile(q) => AggFunc::Quantile(*q),
        InnerFunc::Avg => AggFunc::Avg,
        InnerFunc::Min => AggFunc::Min,
        InnerFunc::Max => AggFunc::Max,
        InnerFunc::Sum => AggFunc::Sum,
        InnerFunc::StdDev => AggFunc::StdDev { population: false },
        InnerFunc::Variance => AggFunc::Variance { population: false },
        InnerFunc::Count => AggFunc::Count,
        InnerFunc::Rate(w) => AggFunc::Rate { window: *w },
        InnerFunc::Increase(w) => AggFunc::Increase { window: *w },
    }
}

fn outer_func(o: &OuterIntent) -> AggFunc {
    match o {
        OuterIntent::Sum => AggFunc::Sum,
        OuterIntent::Avg => AggFunc::Avg,
        OuterIntent::Min => AggFunc::Min,
        OuterIntent::Max => AggFunc::Max,
        OuterIntent::StdDev => AggFunc::StdDev { population: false },
        OuterIntent::Variance => AggFunc::Variance { population: false },
        OuterIntent::Quantile(q) => AggFunc::Quantile(*q),
    }
}

/// Resolve `by(labels)` into a key list. `without(...)` needs the metric's
/// full label set, which the usage-derived schema model doesn't carry, so it
/// is rejected (a registry-backed `SchemaCatalog` would lift this).
fn resolve_group(agg: &AggregateExpr) -> Result<Vec<String>> {
    match &agg.modifier {
        None => Ok(vec![]),
        Some(LabelModifier::Include(ls)) => {
            // Grouping labels are a set: `by (a, b)` ≡ `by (b, a)`. Canonicalise
            // so equivalent groupings lower to identical keys.
            let mut keys = ls.labels.clone();
            keys.sort();
            keys.dedup();
            Ok(keys)
        }
        Some(LabelModifier::Exclude(_)) => Err(LoweringError::UnsupportedFeature(
            "`without(...)` grouping requires a registry-backed catalog of the \
             metric's label set (the usage-derived schema can't enumerate the \
             complement)"
                .into(),
        )),
    }
}

// ── Free helpers ──────────────────────────────────────────────────────────────

fn vs_parts(vs: &VectorSelector) -> Result<(String, Vec<L2Expr>)> {
    // `offset` / `@` shift the evaluation/lookback time. The intent algebra has
    // no representation for either, so silently lowering them (as if absent)
    // would change the query's meaning. Reject rather than mislower.
    if vs.offset.is_some() || vs.at.is_some() {
        return Err(LoweringError::UnsupportedFeature(
            "`offset` / `@` time-shift modifiers have no intent-algebra representation".into(),
        ));
    }
    let metric = vs.name.clone().unwrap_or_else(|| {
        vs.matchers
            .matchers
            .iter()
            .find(|m| m.name == "__name__")
            .map(|m| m.value.clone())
            .unwrap_or_default()
    });
    // Label matchers are an unordered set: `{a="1",b="2"}` and `{b="2",a="1"}`
    // select the same series. Canonicalise by (name, value) so equivalent
    // selectors lower to identical predicates.
    let mut ms: Vec<&Matcher> = vs
        .matchers
        .matchers
        .iter()
        .filter(|m| m.name != "__name__")
        .collect();
    ms.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.value.cmp(&b.value)));
    let matchers = ms.into_iter().map(matcher_to_l3expr).collect();
    Ok((metric, matchers))
}

fn matcher_to_l3expr(m: &Matcher) -> L2Expr {
    let op = match &m.op {
        MatchOp::Equal => CompareOp::Eq,
        MatchOp::NotEqual => CompareOp::Ne,
        MatchOp::Re(_) => CompareOp::Regex,
        MatchOp::NotRe(_) => CompareOp::NotRegex,
    };
    L2Expr::Compare {
        left: Box::new(L2Expr::Column(ColumnRef::Named(m.name.clone()))),
        op,
        right: Box::new(L2Expr::Literal(L3Scalar::Utf8(m.value.clone()))),
    }
}

fn extract_matrix(expr: &Expr) -> Result<(String, Vec<L2Expr>, Duration)> {
    match expr {
        Expr::MatrixSelector(ms) => {
            let (metric, matchers) = vs_parts(&ms.vs)?;
            Ok((metric, matchers, ms.range))
        }
        Expr::Paren(p) => extract_matrix(&p.expr),
        // A range-vector function argument must be a (parenthesised) matrix
        // selector. Do NOT descend through an arbitrary `Call` — that would
        // silently strip an unsupported wrapper (`rate(deriv(m[5m]))` lowering
        // as `rate(m[5m])`). Reject instead.
        other => Err(LoweringError::UnsupportedFeature(format!(
            "expected a range-vector (matrix) argument, got {:?}",
            std::mem::discriminant(other)
        ))),
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

/// `topk`/`bottomk` count parameter — a non-negative integer. Rejects
/// fractional / negative / non-finite values rather than silently truncating
/// or saturating them via `as u64` (`topk(2.7, …)` ≠ `topk(2, …)`).
fn count_param(agg: &AggregateExpr) -> Result<u64> {
    let v = num_param(agg)?;
    if v.is_finite() && v >= 0.0 && v.fract() == 0.0 && v <= u64::MAX as f64 {
        Ok(v as u64)
    } else {
        Err(LoweringError::InvalidParameter(format!(
            "topk/bottomk k must be a non-negative integer, got {v}"
        )))
    }
}

/// Quantile φ — must be a finite value in `[0, 1]`. Rejects NaN/∞ and
/// out-of-range φ (which would otherwise propagate into a bogus intent and
/// output-column name like `quantile_NaN`).
fn quantile_param(q: f64) -> Result<f64> {
    if q.is_finite() && (0.0..=1.0).contains(&q) {
        Ok(q)
    } else {
        Err(LoweringError::InvalidParameter(format!(
            "quantile φ must be in [0, 1], got {q}"
        )))
    }
}

fn binop(id: token::TokenId) -> Result<BinaryOpKind> {
    Ok(if id == token::T_ADD {
        BinaryOpKind::Arith(ArithOp::Add)
    } else if id == token::T_SUB {
        BinaryOpKind::Arith(ArithOp::Sub)
    } else if id == token::T_MUL {
        BinaryOpKind::Arith(ArithOp::Mul)
    } else if id == token::T_DIV {
        BinaryOpKind::Arith(ArithOp::Div)
    } else if id == token::T_MOD {
        BinaryOpKind::Arith(ArithOp::Mod)
    } else if id == token::T_POW {
        BinaryOpKind::Pow
    } else if id == token::T_ATAN2 {
        BinaryOpKind::Atan2
    } else if id == token::T_EQLC {
        BinaryOpKind::Compare(CompareOp::Eq)
    } else if id == token::T_NEQ {
        BinaryOpKind::Compare(CompareOp::Ne)
    } else if id == token::T_LSS {
        BinaryOpKind::Compare(CompareOp::Lt)
    } else if id == token::T_LTE {
        BinaryOpKind::Compare(CompareOp::Le)
    } else if id == token::T_GTR {
        BinaryOpKind::Compare(CompareOp::Gt)
    } else if id == token::T_GTE {
        BinaryOpKind::Compare(CompareOp::Ge)
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
