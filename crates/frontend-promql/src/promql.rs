//! Layers 1→2 lowering: PromQL string → Layer-2 `relational::QueryExpr`.
//!
//! - **L1 (parse)** is delegated to `promql-parser` 0.8.
//! - **L2 (per-language tree)** is built here: the walk interprets PromQL
//!   semantics (range vectors, aggregate operators, label matchers) and emits
//!   the language-flavored [`relational::QueryExpr`] the controller's L2→L3
//!   converter ([`convert_root`](asap_l2::convert_root))
//!   consumes. Canonicalisation (window-over-aggregate fold, GROUP-BY →
//!   positional `Aggregate.by`, positional name binding) happens in that
//!   converter, not here.
//!
//! # PromQL → L2 mapping (summary)
//!
//! | PromQL | L2 shape (→ canonical via `convert_root`) |
//! |---|---|
//! | `quantile_over_time(φ, m{f}[w])` | `Aggregate{[Quantile(φ)], Window{w, Filter(Source)}}` |
//! | `histogram_quantile(φ, <classic buckets>)` | `Aggregate{[HistogramQuantile(φ)]}` — cumulative-bucket interpolation (classic form recognised by `by (le)` / a `_bucket` metric / an `le` matcher) |
//! | `histogram_quantile(φ, <native hist / raw>)` | `Aggregate{[Quantile(φ)]}` over the fully-lowered arg (generic, sketch-able with an accuracy target) |
//! | `histogram_count/sum/avg/stddev/stdvar(v)`, `histogram_fraction(l,u,v)` | `Aggregate{[Histogram*]}` — per-series native-histogram accessors (issue #43) |
//! | `OUTER_op(inner_func(m[w]))` (e.g. `sum(rate(m[w]))`) | `Aggregate{[OUTER_op]}` over `Aggregate{[inner_func]}` — two levels |
//! | `OUTER_op(<any expr>)` (e.g. `max(sum by (job) (rate(m[w])))`, `sum(rate(a[w]) + rate(b[w]))`) | `Aggregate{[OUTER_op]}` over the fully-lowered `<any expr>` — arbitrary function nesting (issue #27) |
//! | `topk(k, <non-count expr>)` / `bottomk(k, <any expr>)` | `Sort{value} → Limit{k}` over the fully-lowered argument |
//! | `avg/min/max/sum_over_time(m[w])` | `Aggregate{[Avg/Min/Max/Sum], Window{w}}` |
//! | `stddev/stdvar_over_time(m[w])` | `Aggregate{[StdDev/Variance], Window{w}}` |
//! | `count_over_time(m[w])` | `Aggregate{[Count], Window{w}}` |
//! | `rate/irate(m[w])` | `Aggregate{[Rate{w}]}` (no Window) — `irate` shares the `rate` *intent*; the avg-vs-last-two-samples difference is an L4 estimation method |
//! | `increase(m[w])` | `Aggregate{[Increase{w}]}` (no Window) |
//! | `changes`/`delta`/`idelta`/`deriv`/`resets`/`predict_linear`/`double_exponential_smoothing`(`m[w]`, …) | `Aggregate{[Changes/Delta/…], Window{w}}` — per-series counter-derivative intents (issue #44); `holt_winters` is the legacy alias of `double_exponential_smoothing` |
//! | `absent(v)` / `absent_over_time(m[w])` / `present_over_time(m[w])` | `Aggregate{[Absent/AbsentOverTime/PresentOverTime]}` — presence intents; the empty→synthesized-sample logic is L4 (issue #47) |
//! | `abs`/`ceil`/`sqrt`/`ln`/`clamp*`/`round`/trig(`v`), `pi()` | `Aggregate{[Math(f)]}` element-wise transform (issue #45); `pi()` → a `Scalar` leaf |
//! | `time()` / `timestamp`/`hour`/`day_of_week`/… (`v`) | `EvalTime` leaf / `Aggregate{[TimeFn(f)]}` (issue #46) |
//! | `vector(s)` / `scalar(v)` | `VectorFromScalar` / `ScalarFromVector` — the scalar⇄vector bridges (issue #48) |
//! | `group` / `offset` / `@` / `info` | **rejected** — distinct semantics with no intent-algebra representation yet (`info` label-join → #84) |
//! | `OUTER by (dims) (…)` | `Aggregate.keys = dims` (→ positional `Aggregate.by` in L3; generic `topk by`/`bottomk` grouping → `Sort.partition_by`) |
//! | `count by (d) (…)` | `Aggregate{[CountDistinct], …}` (→ `Cardinality`) |
//! | `group(v)` / `count_values("l", v)` | `Aggregate{[Group]}` (constant 1) / `Aggregate{[CountValues{l}]}` (group-by-value + count, new label `l`) — issue #49; `limitk`/`limit_ratio` series sampling → #86 |
//! | `topk(k, count_over_time(…))` | `TopK{k, by}` (heavy-hitter intent) |
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

use asap_ir::intent_algebra::query_expr::{
    BinaryOpKind, GroupSide, VectorGrouping, VectorMatch, VectorMatchKind,
};
use asap_l2::relational::{
    AggFunc, AggItem, L2SortKey, QueryExpr as L2, SourceSpec,
};
use asap_ir::intent_algebra::agg_intent::{MathFunc, TimeFunc};
use asap_ir::intent_algebra::{ArithOp, ColumnRef, CompareOp, L2Expr, L3Scalar};

use crate::error::PromqlError as LoweringError;

type Result<T> = std::result::Result<T, LoweringError>;

/// Parses (L1) and lowers (→ L2 relational) a PromQL query string.
pub struct PromqlLowerer;

#[derive(Debug, Clone)]
enum Outer {
    None,
    Plain(OuterIntent),
    Count,
    /// `count_values("l", v)` — group by value + count, emitting the value as a
    /// new label `l` (issue #49).
    CountValues { label: String },
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
    /// `group(v)` — constant 1 per group (issue #49).
    Group,
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
    // Counter-derivative range functions (issue #44). The window rides on the
    // enclosing L2 `Window` node (like `*_over_time`), so these carry only
    // their non-window scalar params.
    Changes,
    Delta,
    IDelta,
    Deriv,
    Resets,
    PredictLinear(f64),
    DoubleExp { smoothing: f64, trend: f64 },
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
        Expr::Call(call) if call.func.name.starts_with("histogram_") => walk_histogram(call),
        Expr::Call(call) if is_math_fn(call.func.name) => walk_math(call),
        Expr::Call(call) if is_presence_fn(call.func.name) => walk_presence(call),
        Expr::Call(call) if is_time_fn(call.func.name) => walk_time(call),
        Expr::Call(call) if is_typeconv_fn(call.func.name) => walk_typeconv(call),
        Expr::Call(call) => walk_call(call),
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
        // A number literal is a scalar leaf (`v > 5`, or a bare scalar query
        // `5`). String literals only appear as function args (`label_replace`,
        // …), which are not supported, so reject them (issue #35).
        Expr::NumberLiteral(n) => Ok(L2::Scalar(n.val)),
        Expr::StringLiteral(_) => Err(LoweringError::UnsupportedFeature(
            "bare string literal".into(),
        )),
        Expr::Extension(_) => Err(LoweringError::UnsupportedFeature(
            "extension expression".into(),
        )),
    }
}

/// Lower a bare function call (`rate(m[5m])`, `max_over_time(m[5m])`, …).
///
/// The common case routes through the flat `lower_inner_call` template. The one
/// exception is a `*_over_time`/`quantile_over_time` function applied to a
/// **sub-query** (`max_over_time(rate(m[5m])[1h:])`): its argument is a
/// `PromQLSubquery`, not a matrix selector, so the flat template's
/// `extract_matrix` can't accept it. Lower the sub-query recursively and reduce
/// it per series (issue #27).
fn walk_call(call: &Call) -> Result<L2> {
    if let Some(l2) = range_fn_over_subquery(call)? {
        return Ok(l2);
    }
    build(lower_inner_call(call)?, vec![], Outer::None)
}

/// A range-vector function applied to a **sub-query** — `f(<inst>[range:res])`.
///
/// Covers the whole range-vector family: the `*_over_time` reducers,
/// `rate`/`irate`/`increase`, and the counter-derivatives
/// (`changes`/`delta`/`idelta`/`deriv`/`resets`/`predict_linear`/
/// `double_exponential_smoothing`). Each lowers to a per-series `Aggregate{[f]}`
/// directly over the `PromQLSubquery` — the sub-query is the range context, so
/// there is no separate `Window`/`TimeRange` (the L2→L3 converter treats the
/// `Subquery` as the range marker). Returns `None` when `call` isn't a range
/// function or its argument isn't a sub-query, so the flat matrix-selector
/// template still handles `f(m[w])` (issues #42, #55).
fn range_fn_over_subquery(call: &Call) -> Result<Option<L2>> {
    // `rate`/`increase`/`irate` carry their window in the `AggFunc`; over a
    // sub-query that window is the sub-query's own range.
    if let "rate" | "irate" | "increase" = call.func.name {
        let arg_expr = arg(call, 0)?;
        let Some(range) = subquery_range(arg_expr) else {
            return Ok(None);
        };
        let inner = if call.func.name == "increase" {
            InnerFunc::Increase(range)
        } else {
            InnerFunc::Rate(range)
        };
        return Ok(Some(outer_aggregate(vec![], inner_func(&inner), walk(arg_expr)?)));
    }

    // `*_over_time` reducers + counter-derivatives: the func-kind, and the index
    // of the matrix/sub-query argument (`quantile_over_time` reads φ from arg 0,
    // so its matrix is arg 1; the rest take arg 0 + trailing scalar params).
    let (inner, matrix_idx): (InnerFunc, usize) = match call.func.name {
        "avg_over_time" => (InnerFunc::Avg, 0),
        "min_over_time" => (InnerFunc::Min, 0),
        "max_over_time" => (InnerFunc::Max, 0),
        "sum_over_time" => (InnerFunc::Sum, 0),
        "stddev_over_time" => (InnerFunc::StdDev, 0),
        "stdvar_over_time" => (InnerFunc::Variance, 0),
        "count_over_time" => (InnerFunc::Count, 0),
        "quantile_over_time" => (InnerFunc::Quantile(quantile_param(num_arg(call, 0)?)?), 1),
        "changes" => (InnerFunc::Changes, 0),
        "delta" => (InnerFunc::Delta, 0),
        "idelta" => (InnerFunc::IDelta, 0),
        "deriv" => (InnerFunc::Deriv, 0),
        "resets" => (InnerFunc::Resets, 0),
        "predict_linear" => (InnerFunc::PredictLinear(num_arg(call, 1)?), 0),
        "double_exponential_smoothing" | "holt_winters" => (
            InnerFunc::DoubleExp {
                smoothing: num_arg(call, 1)?,
                trend: num_arg(call, 2)?,
            },
            0,
        ),
        _ => return Ok(None),
    };
    let arg_expr = arg(call, matrix_idx)?;
    if !is_subquery(arg_expr) {
        return Ok(None);
    }
    Ok(Some(outer_aggregate(vec![], inner_func(&inner), walk(arg_expr)?)))
}

/// A (parenthesised) PromQL sub-query — `<inst>[range:res]`.
fn is_subquery(expr: &Expr) -> bool {
    subquery_range(expr).is_some()
}

/// The `range` of a (parenthesised) sub-query argument, if it is one.
fn subquery_range(expr: &Expr) -> Option<Duration> {
    match expr {
        Expr::Subquery(sq) => Some(sq.range),
        Expr::Paren(p) => subquery_range(&p.expr),
        _ => None,
    }
}

fn walk_aggregate(agg: &AggregateExpr) -> Result<L2> {
    let keys = resolve_group(agg)?;
    let outer = outer_kind(agg)?;

    // Fast path — the argument is a bare selector or a single range-vector
    // function (`rate`/`increase`/`*_over_time`). `lower_inner` lowers it via the
    // flat selector/call template, which also recognises the heavy-hitter
    // `topk(k, count_over_time(...))` shape. This is the common two-level case
    // (`sum by (job) (rate(m[5m]))`).
    if let Ok(inner) = lower_inner(&agg.expr) {
        return build(inner, keys, outer);
    }

    // General nesting — the argument is itself a composite expression: another
    // aggregate (`max(sum by (job) (rate(m[5m])))`), a binary op
    // (`sum(rate(a[5m]) + rate(b[5m]))`), a sub-query, or a function lowered
    // elsewhere (`sum(histogram_quantile(0.9, …))`). Lower it recursively with
    // the same `walk` used at the top level, then wrap it in the outer
    // aggregation. This is the path that lifts the old two-level limit to
    // arbitrary function nesting (issue #27). If the inner expression is itself
    // unsupported (e.g. unary negation), `walk` surfaces that error, so a
    // genuinely unsupported query is still cleanly rejected rather than
    // mislowered.
    let child = walk(&agg.expr)?;
    build_over_subtree(outer, keys, child)
}

/// Map an `AggregateExpr`'s operator (`sum`/`avg`/`topk`/…) to the [`Outer`]
/// shape, independent of what the argument is — so both the flat fast path and
/// the general recursive path share one operator-dispatch.
fn outer_kind(agg: &AggregateExpr) -> Result<Outer> {
    let op = agg.op.id();

    Ok(if op == token::T_TOPK {
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
        // values — a distinct intent, never folded onto `Sum` (issue #49).
        Outer::Plain(OuterIntent::Group)
    } else if op == token::T_COUNT_VALUES {
        // `count_values("l", v)` groups by sample value and counts, emitting the
        // value as a new label `l` (the string parameter) — issue #49.
        Outer::CountValues {
            label: str_param(agg)?,
        }
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
    })
}

/// Wrap an already-lowered L2 subtree in the outer aggregation. This is the
/// general-nesting counterpart to [`build`]: where `build` assembles the
/// two-level shape from a flat [`Inner`], this composes the outer operator over
/// an arbitrary child (`max(sum by (job) (…))`, `sum(a + b)`, …).
///
/// A heavy-hitter `TopK` is only recognised on the flat `count_over_time` shape
/// (handled in `build`); over a general subtree, `topk`/`bottomk` is a generic
/// order-by-value + limit — the same `Sort{partition_by} → Limit` pair `build`
/// emits for any non-heavy-hitter ranking.
fn build_over_subtree(outer: Outer, keys: Vec<ColumnRef>, child: L2) -> Result<L2> {
    Ok(match outer {
        // `walk_aggregate` always passes a real aggregator; `None` can't occur.
        Outer::None => child,
        Outer::Plain(intent) => outer_aggregate(keys, outer_func(&intent), child),
        Outer::Count => outer_aggregate(keys, AggFunc::CountDistinct, child),
        Outer::CountValues { label } => {
            outer_aggregate(keys, AggFunc::CountValues { label }, child)
        }
        Outer::TopK { k, descending } => {
            let sorted = L2::Sort {
                keys: vec![L2SortKey {
                    expr: L2Expr::Column(ColumnRef::SampleValue),
                    ascending: !descending,
                    nulls_first: false,
                }],
                partition_by: keys,
                input: Box::new(child),
            };
            L2::Limit {
                n: k,
                offset: 0,
                input: Box::new(sorted),
            }
        }
    })
}

/// `histogram_quantile(φ, <expr>)` lowers `<expr>` in full — preserving any
/// `sum by (le)` / `rate` structure inside it — and wraps the result in an
/// `Aggregate{[Quantile(φ)]}`. The φ-quantile reduces across the `le` buckets,
/// so the wrapper carries no grouping keys: the usage-derived schema can't
/// enumerate the non-`le` labels to group by (the same limitation that rejects
/// `without`). This handles the canonical
/// `histogram_quantile(φ, sum by (le) (rate(m_bucket[w])))` pattern, which the
/// old "extract the matrix and substitute a bare Quantile" path could not.
/// The `histogram_*` function family (issues #43, histogram_quantile).
///
/// `histogram_quantile(φ, <expr>)` lowers to a `Quantile` over the fully-lowered
/// argument — it also covers the classic `le`-bucket form (`sum by (le) (…)`).
/// The native-histogram accessors (`histogram_count`/`sum`/`avg`/`stddev`/
/// `stdvar`/`fraction`) each extract one float per series, lowering to a
/// per-series `Aggregate{[accessor]}` directly over the (instant) argument.
/// `histogram_fraction(lower, upper, v)` reads its bounds from args 0/1 and the
/// vector from arg 2; the rest take the vector at arg 0.
fn walk_histogram(call: &Call) -> Result<L2> {
    if call.func.name == "histogram_quantile" {
        let phi = quantile_param(num_arg(call, 0)?)?;
        let arg_expr = arg(call, 1)?;
        // Two lowerings of `histogram_quantile(φ, …)`:
        //  - classic `le`-bucket form (`… sum by (le) (rate(x_bucket[5m])) …`) →
        //    `HistogramQuantile`, exact interpolation over cumulative buckets.
        //  - native-histogram form (any other argument) → the generic `Quantile`
        //    intent (sketch-able).
        // We can't see sample types at lowering, so the classic form is
        // recognised by its distinctive `by (le)` grouping (issue #43).
        let func = if is_classic_bucket_arg(arg_expr) {
            AggFunc::HistogramQuantile(phi)
        } else {
            AggFunc::Quantile(phi)
        };
        return Ok(outer_aggregate(vec![], func, walk(arg_expr)?));
    }
    // (histogram_quantile handled above; accessors below)
    let (func, vec_idx) = match call.func.name {
        "histogram_count" => (AggFunc::HistogramCount, 0),
        "histogram_sum" => (AggFunc::HistogramSum, 0),
        "histogram_avg" => (AggFunc::HistogramAvg, 0),
        "histogram_stddev" => (AggFunc::HistogramStdDev, 0),
        "histogram_stdvar" => (AggFunc::HistogramStdVar, 0),
        "histogram_fraction" => (
            AggFunc::HistogramFraction {
                lower: num_arg(call, 0)?,
                upper: num_arg(call, 1)?,
            },
            2,
        ),
        other => return Err(LoweringError::UnsupportedFunction(other.to_string())),
    };
    Ok(outer_aggregate(vec![], func, walk(arg(call, vec_idx)?)?))
}

/// The time / calendar functions (issue #46).
fn is_time_fn(name: &str) -> bool {
    matches!(
        name,
        "time"
            | "timestamp"
            | "minute"
            | "hour"
            | "day_of_week"
            | "day_of_month"
            | "day_of_year"
            | "month"
            | "year"
            | "days_in_month"
    )
}

/// `time()` → the `EvalTime` leaf. `timestamp(v)` and the calendar accessors →
/// `Aggregate{[TimeFn(f)]}` over the argument vector, or over `EvalTime` for the
/// no-argument calendar forms (`hour()`, `day_of_week()`, …). Issue #46.
fn walk_time(call: &Call) -> Result<L2> {
    if call.func.name == "time" {
        return Ok(L2::EvalTime);
    }
    let func = match call.func.name {
        "timestamp" => TimeFunc::Timestamp,
        "minute" => TimeFunc::Minute,
        "hour" => TimeFunc::Hour,
        "day_of_week" => TimeFunc::DayOfWeek,
        "day_of_month" => TimeFunc::DayOfMonth,
        "day_of_year" => TimeFunc::DayOfYear,
        "month" => TimeFunc::Month,
        "year" => TimeFunc::Year,
        "days_in_month" => TimeFunc::DaysInMonth,
        other => return Err(LoweringError::UnsupportedFunction(other.to_string())),
    };
    // A calendar function with no argument reads the evaluation time; otherwise
    // it maps over each sample's timestamp in the argument vector.
    let inner = if call.args.args.is_empty() {
        L2::EvalTime
    } else {
        walk(arg(call, 0)?)?
    };
    Ok(outer_aggregate(vec![], AggFunc::TimeFn(func), inner))
}

/// The presence functions (issue #47).
fn is_presence_fn(name: &str) -> bool {
    matches!(name, "absent" | "absent_over_time" | "present_over_time")
}

/// `absent(v)` / `absent_over_time(m[w])` / `present_over_time(m[w])` — lowered
/// to an `Aggregate{[Absent/…]}` over the (instant or range) argument. The
/// empty-result → synthesized-1-sample logic is an L4/runtime concern; L3 only
/// marks the operation (issue #47).
fn walk_presence(call: &Call) -> Result<L2> {
    let func = match call.func.name {
        "absent" => AggFunc::Absent,
        "absent_over_time" => AggFunc::AbsentOverTime,
        "present_over_time" => AggFunc::PresentOverTime,
        other => return Err(LoweringError::UnsupportedFunction(other.to_string())),
    };
    // arg 0 is the instant vector (`absent`) or range vector (`*_over_time`);
    // `walk` produces a `Window` for the matrix-selector forms.
    Ok(outer_aggregate(vec![], func, walk(arg(call, 0)?)?))
}

/// The scalar⇄vector type-conversion functions (issue #48). `info` is *not*
/// here: it is a label-enrichment join against info metrics, not a type
/// conversion, so it falls through to the `UnsupportedFunction` path (#84).
fn is_typeconv_fn(name: &str) -> bool {
    matches!(name, "vector" | "scalar")
}

/// `vector(s)` — promote a scalar to a label-less instant vector. `scalar(v)`
/// — collapse a single-element vector to its value. Both are honest bridge
/// nodes in the IR; the "exactly one element → NaN otherwise" runtime rule of
/// `scalar` is an L4/runtime concern (issue #48).
fn walk_typeconv(call: &Call) -> Result<L2> {
    let inner = walk(arg(call, 0)?)?;
    Ok(match call.func.name {
        "vector" => L2::VectorFromScalar(Box::new(inner)),
        "scalar" => L2::ScalarFromVector(Box::new(inner)),
        other => return Err(LoweringError::UnsupportedFunction(other.to_string())),
    })
}

/// The element-wise math / trig functions (issue #45).
fn is_math_fn(name: &str) -> bool {
    matches!(
        name,
        "abs" | "ceil" | "floor" | "exp" | "ln" | "log2" | "log10" | "sqrt" | "sgn" | "sin" | "cos"
            | "tan" | "asin" | "acos" | "atan" | "sinh" | "cosh" | "tanh" | "asinh" | "acosh"
            | "atanh" | "deg" | "rad" | "pi" | "round" | "clamp" | "clamp_min" | "clamp_max"
    )
}

/// A math / trig function — a per-series element-wise value transform, lowered
/// to a per-series `Aggregate{[Math(f)]}` over the (instant) argument vector.
/// `pi()` is the constant π, lowered to a `Scalar` leaf (issue #45).
fn walk_math(call: &Call) -> Result<L2> {
    if call.func.name == "pi" {
        return Ok(L2::Scalar(std::f64::consts::PI));
    }
    let func = match call.func.name {
        "abs" => MathFunc::Abs,
        "ceil" => MathFunc::Ceil,
        "floor" => MathFunc::Floor,
        "exp" => MathFunc::Exp,
        "ln" => MathFunc::Ln,
        "log2" => MathFunc::Log2,
        "log10" => MathFunc::Log10,
        "sqrt" => MathFunc::Sqrt,
        "sgn" => MathFunc::Sgn,
        "sin" => MathFunc::Sin,
        "cos" => MathFunc::Cos,
        "tan" => MathFunc::Tan,
        "asin" => MathFunc::Asin,
        "acos" => MathFunc::Acos,
        "atan" => MathFunc::Atan,
        "sinh" => MathFunc::Sinh,
        "cosh" => MathFunc::Cosh,
        "tanh" => MathFunc::Tanh,
        "asinh" => MathFunc::Asinh,
        "acosh" => MathFunc::Acosh,
        "atanh" => MathFunc::Atanh,
        "deg" => MathFunc::Deg,
        "rad" => MathFunc::Rad,
        // `round(v)` defaults the step to 1; `round(v, to)` reads arg 1.
        "round" => MathFunc::Round {
            to_nearest: if call.args.args.len() >= 2 {
                num_arg(call, 1)?
            } else {
                1.0
            },
        },
        "clamp" => MathFunc::Clamp {
            min: num_arg(call, 1)?,
            max: num_arg(call, 2)?,
        },
        "clamp_min" => MathFunc::ClampMin {
            min: num_arg(call, 1)?,
        },
        "clamp_max" => MathFunc::ClampMax {
            max: num_arg(call, 1)?,
        },
        other => return Err(LoweringError::UnsupportedFunction(other.to_string())),
    };
    // The value being transformed is always arg 0 (a vector).
    let inner = walk(arg(call, 0)?)?;
    Ok(outer_aggregate(vec![], AggFunc::Math(func), inner))
}

/// Whether `expr` is a **classic cumulative-bucket** `histogram_quantile`
/// argument — as opposed to a native histogram or raw samples. Recognised
/// structurally, by any of:
///  - a `by (le)` grouping (`sum by (le) (…)`),
///  - a selector on a classic `_bucket` metric (`http_request_…_bucket`),
///  - a selector with an `le` label matcher (`{le="…"}`).
///
/// The bucket form must be *interpolated* (`HistogramQuantile`); everything
/// else is a sketch-able generic `Quantile`. This is a heuristic proxy for the
/// real signal — the argument's sample type — which isn't visible at lowering;
/// see the follow-up issue on the discrimination criteria (issue #43).
fn is_classic_bucket_arg(expr: &Expr) -> bool {
    match expr {
        Expr::Paren(p) => is_classic_bucket_arg(&p.expr),
        Expr::Unary(u) => is_classic_bucket_arg(&u.expr),
        Expr::Subquery(s) => is_classic_bucket_arg(&s.expr),
        Expr::Aggregate(agg) => {
            matches!(
                &agg.modifier,
                Some(LabelModifier::Include(ls)) if ls.labels.iter().any(|l| l == "le")
            ) || is_classic_bucket_arg(&agg.expr)
        }
        Expr::Binary(b) => is_classic_bucket_arg(&b.lhs) || is_classic_bucket_arg(&b.rhs),
        Expr::Call(c) => c.args.args.iter().any(|a| is_classic_bucket_arg(a)),
        Expr::VectorSelector(vs) => selector_is_bucket(vs),
        Expr::MatrixSelector(ms) => selector_is_bucket(&ms.vs),
        _ => false,
    }
}

/// A classic histogram bucket selector — a `_bucket`-named metric (via bare name
/// or `__name__` matcher) or an explicit `le` label matcher.
fn selector_is_bucket(vs: &VectorSelector) -> bool {
    let name = vs.name.as_deref().or_else(|| {
        vs.matchers
            .matchers
            .iter()
            .find(|m| m.name == "__name__")
            .map(|m| m.value.as_str())
    });
    name.is_some_and(|n| n.ends_with("_bucket"))
        || vs.matchers.matchers.iter().any(|m| m.name == "le")
}

fn walk_binary(bin: &BinaryExpr) -> Result<L2> {
    let lhs = scalar_or_vector(&bin.lhs)?;
    let rhs = scalar_or_vector(&bin.rhs)?;
    let op = binop(bin.op.id())?;
    let vector_match = bin.modifier.as_ref().map(|m| {
        let (kind, labels) = match &m.matching {
            Some(LabelModifier::Include(ls)) => (VectorMatchKind::On, ls.labels.clone()),
            Some(LabelModifier::Exclude(ls)) => (VectorMatchKind::Ignoring, ls.labels.clone()),
            // No explicit `on(…)`/`ignoring(…)` — the parser attaches a default
            // modifier to every set op (`and`/`or`/`unless`). The default is
            // "match on all shared labels", which is exactly `ignoring([])`
            // (ignore no labels). Representing it as `Ignoring([])` — not
            // `On([])` — keeps it distinct from an explicit `on()` (match on the
            // empty label set) while making it correctly equal to an explicit
            // `ignoring()` (issue #68).
            None => (VectorMatchKind::Ignoring, vec![]),
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
        // Counter-derivative range functions (issue #44). Each has its own
        // intent — `changes` (value-change count) and `resets` (counter-reset
        // count) are NOT sample counts, so they are not aliased to
        // `count_over_time`. The window is arg 0's matrix; scalar params follow.
        "changes" => at0(InnerFunc::Changes),
        "delta" => at0(InnerFunc::Delta),
        "idelta" => at0(InnerFunc::IDelta),
        "deriv" => at0(InnerFunc::Deriv),
        "resets" => at0(InnerFunc::Resets),
        "predict_linear" => {
            let (metric, matchers, window) = extract_matrix(arg(call, 0)?)?;
            let seconds = num_arg(call, 1)?;
            Ok(Inner {
                metric,
                matchers,
                window: Some(window),
                func: Some(InnerFunc::PredictLinear(seconds)),
            })
        }
        // `holt_winters` is the legacy spelling of `double_exponential_smoothing`.
        "double_exponential_smoothing" | "holt_winters" => {
            let (metric, matchers, window) = extract_matrix(arg(call, 0)?)?;
            let smoothing = num_arg(call, 1)?;
            let trend = num_arg(call, 2)?;
            Ok(Inner {
                metric,
                matchers,
                window: Some(window),
                func: Some(InnerFunc::DoubleExp { smoothing, trend }),
            })
        }
        other => Err(LoweringError::UnsupportedFunction(other.to_string())),
    }
}

/// Assemble the Layer-2 tree from a lowered inner vector, the resolved group
/// keys, and the enclosing aggregator shape.
fn build(inner: Inner, keys: Vec<ColumnRef>, outer: Outer) -> Result<L2> {
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
        Outer::CountValues { label } => {
            let func = AggFunc::CountValues { label };
            Ok(match &inner.func {
                None => windowed_aggregate(inner, keys, func),
                Some(f) => {
                    let inner_f = inner_func(f);
                    let inner_agg = windowed_aggregate(inner, vec![], inner_f);
                    outer_aggregate(keys, func, inner_agg)
                }
            })
        }
        Outer::TopK { k, descending } => {
            // Heavy-hitter only when ranking by frequency (`count`): that is a
            // first-class aggregate intent → `TopK`. Any other ranking (topk
            // over avg/quantile, all bottomk) is a generic order-by-value +
            // limit and stays as the `Sort + Limit` operator pair.
            let heavy_hitter = descending && matches!(inner.func, Some(InnerFunc::Count));
            if heavy_hitter {
                // Preserve the Count intent in L3 so the intent algebra is
                // explicit about what is being computed. L4 may fuse the Count
                // and TopK into a single-pass heavy-hitter sketch (SpaceSaving /
                // CMS-with-heap), but that is a cost-model decision, not an L3
                // concern.
                let count_agg = windowed_aggregate(inner, vec![], inner_func(&InnerFunc::Count));
                Ok(L2::TopK {
                    k,
                    by: keys,
                    input: Box::new(count_agg),
                })
            } else {
                // The base over which we rank. A range-vector-function argument
                // (`topk(k, rate(m[5m]))`) reduces *per series* first — that is
                // label-preserving, so the `by (host)` partition labels survive.
                // A **bare instant selector** (`topk(k, m)`) ranks its own
                // samples directly: it must NOT be wrapped in a reducing
                // aggregate. Defaulting it to `Sum` was both semantically wrong
                // (PromQL `topk` ranks the raw samples, it does not sum them) and
                // destructive — the cross-series `Sum` collapses every label,
                // including the `by (…)` partition keys, so they no longer
                // resolve at L3 (issue #30). Keep the selector label-preserving so
                // `Sort.partition_by` can rank within each group (issue #12).
                let base = match inner.func.as_ref().map(inner_func) {
                    Some(func) => windowed_aggregate(inner, vec![], func),
                    None => filtered_source(inner.metric, inner.matchers),
                };
                let sorted = L2::Sort {
                    keys: vec![L2SortKey {
                        expr: L2Expr::Column(ColumnRef::SampleValue),
                        ascending: !descending,
                        nulls_first: false,
                    }],
                    partition_by: keys,
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
fn windowed_aggregate(inner: Inner, keys: Vec<ColumnRef>, func: AggFunc) -> L2 {
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
            // None alias → the converter keeps PromQL's intent-keyed output
            // names ("sum", "quantile_0_99", …) instead of overriding them.
            alias: None,
            func,
            col: ColumnRef::SampleValue,
        }],
        having: None,
        input: Box::new(input),
    }
}

/// `Aggregate{keys, [func]}` directly over an existing L2 subtree — the OUTER
/// level of a two-level aggregation such as `sum(rate(…))` or the
/// `Aggregate{[Quantile]}` that wraps a `histogram_quantile` argument.
fn outer_aggregate(keys: Vec<ColumnRef>, func: AggFunc, input: L2) -> L2 {
    L2::Aggregate {
        keys,
        aggs: vec![AggItem {
            // None alias → the converter keeps PromQL's intent-keyed output
            // names ("sum", "quantile_0_99", …) instead of overriding them.
            alias: None,
            func,
            col: ColumnRef::SampleValue,
        }],
        having: None,
        input: Box::new(input),
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
        InnerFunc::StdDev => AggFunc::StdDev { population: true },
        InnerFunc::Variance => AggFunc::Variance { population: true },
        InnerFunc::Count => AggFunc::Count,
        InnerFunc::Rate(w) => AggFunc::Rate { window: *w },
        InnerFunc::Increase(w) => AggFunc::Increase { window: *w },
        InnerFunc::Changes => AggFunc::Changes,
        InnerFunc::Delta => AggFunc::Delta,
        InnerFunc::IDelta => AggFunc::IDelta,
        InnerFunc::Deriv => AggFunc::Deriv,
        InnerFunc::Resets => AggFunc::Resets,
        InnerFunc::PredictLinear(s) => AggFunc::PredictLinear { seconds: *s },
        InnerFunc::DoubleExp { smoothing, trend } => AggFunc::DoubleExpSmoothing {
            smoothing: *smoothing,
            trend: *trend,
        },
    }
}

fn outer_func(o: &OuterIntent) -> AggFunc {
    match o {
        OuterIntent::Sum => AggFunc::Sum,
        OuterIntent::Avg => AggFunc::Avg,
        OuterIntent::Min => AggFunc::Min,
        OuterIntent::Max => AggFunc::Max,
        OuterIntent::StdDev => AggFunc::StdDev { population: true },
        OuterIntent::Variance => AggFunc::Variance { population: true },
        OuterIntent::Quantile(q) => AggFunc::Quantile(*q),
        OuterIntent::Group => AggFunc::Group,
    }
}

/// A `count_values` string parameter (the synthesized label name). PromQL wraps
/// it in a `StringLiteral`, possibly parenthesised (`count_values((("v")), …)`).
fn str_param(agg: &AggregateExpr) -> Result<String> {
    fn unwrap_str(expr: &Expr) -> Result<String> {
        match expr {
            Expr::StringLiteral(s) => Ok(s.val.clone()),
            Expr::Paren(p) => unwrap_str(&p.expr),
            other => Err(LoweringError::InvalidParameter(format!(
                "`count_values` label must be a string literal, got {:?}",
                std::mem::discriminant(other)
            ))),
        }
    }
    match &agg.param {
        Some(e) => unwrap_str(e),
        None => Err(LoweringError::MissingArgument(
            "`count_values` label parameter".into(),
        )),
    }
}

/// Resolve `by(labels)` into a key list. `without(...)` needs the metric's
/// full label set, which the usage-derived schema model doesn't carry, so it
/// is rejected (a registry-backed `SchemaCatalog` would lift this).
fn resolve_group(agg: &AggregateExpr) -> Result<Vec<ColumnRef>> {
    match &agg.modifier {
        None => Ok(vec![]),
        Some(LabelModifier::Include(ls)) => {
            // Grouping labels are a set: `by (a, b)` ≡ `by (b, a)`. Canonicalise
            // so equivalent groupings lower to identical keys. PromQL labels have
            // no table qualifier → `ColumnRef::Named`.
            let mut keys = ls.labels.clone();
            keys.sort();
            keys.dedup();
            Ok(keys.into_iter().map(ColumnRef::Named).collect())
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
    // A non-equality `__name__` matcher (`=~` / `!~` / `!=`) selects *across*
    // metric names. The L3 `Source::TimeSeries { metric }` carries a single
    // concrete metric name, so there is no representation for a regex/negated
    // name match — reject rather than mislower it to a literal metric named
    // after the pattern (issue #67). An equality `__name__` (`{__name__="up"}`)
    // still names the metric below.
    if let Some(m) = vs
        .matchers
        .matchers
        .iter()
        .find(|m| m.name == "__name__" && !matches!(m.op, MatchOp::Equal))
    {
        return Err(LoweringError::UnsupportedFeature(format!(
            "non-equality `__name__` matcher ({}{:?}) selects across metric names, \
             which has no single-metric L3 representation",
            m.name, m.op
        )));
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
        Expr::Paren(p) => num_expr(&p.expr),
        // Constant-fold a pure scalar arithmetic expression — the parser does
        // not fold `10*1024*1024` / `24 * 3600`. A `modifier` (vector matching)
        // or a non-arithmetic operator means it is not a pure scalar.
        Expr::Binary(b) if b.modifier.is_none() => {
            let (l, r) = (num_expr(&b.lhs)?, num_expr(&b.rhs)?);
            let id = b.op.id();
            if id == token::T_ADD {
                Ok(l + r)
            } else if id == token::T_SUB {
                Ok(l - r)
            } else if id == token::T_MUL {
                Ok(l * r)
            } else if id == token::T_DIV {
                Ok(l / r)
            } else if id == token::T_MOD {
                Ok(l % r)
            } else if id == token::T_POW {
                Ok(l.powf(r))
            } else {
                Err(LoweringError::InvalidParameter(
                    "non-arithmetic operator in scalar expression".into(),
                ))
            }
        }
        other => Err(LoweringError::InvalidParameter(format!(
            "expected a numeric scalar, got {:?}",
            std::mem::discriminant(other)
        ))),
    }
}

/// A `BinaryOp` operand: fold a pure-scalar expression (`5`, `10*1024*1024`) to
/// a `Scalar` leaf, otherwise walk it as a vector (issue #35).
fn scalar_or_vector(expr: &Expr) -> Result<L2> {
    match num_expr(expr) {
        Ok(v) => Ok(L2::Scalar(v)),
        Err(_) => walk(expr),
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
