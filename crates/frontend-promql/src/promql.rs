//! Layers 1→2 lowering: PromQL string → the canonical, unresolved
//! [`L2QueryExpr`](asap_types::pre_asap::query_expr::L2QueryExpr)
//! (`QueryExpr<ColumnRef>`).
//!
//! - **L1 (parse)** is delegated to `promql-parser` 0.8.
//! - **L2** is built *directly in canonical shape* here (issue #179): the walk
//!   interprets PromQL semantics (range vectors, aggregate operators, label
//!   matchers) and emits `L2QueryExpr` nodes with unresolved `ColumnRef`s —
//!   the same tree shape [`resolve_root`](asap_types::pre_asap::resolve_root)
//!   later binds to canonical, positional `QueryExpr<ColumnId>`. The
//!   structural decisions a two-layer design would otherwise defer to a
//!   separate converter (heavy-hitter `topk` recognition, the
//!   `PerEntity`/`Reduce` reduction choice, `without(...)` grouping) are made
//!   right here, since a front end building this shape already knows the
//!   answer at parse time — see `reduction_for` and `mark_without`.
//!   `resolve_root` is left with exactly the schema-*dependent* work: binding
//!   every `ColumnRef` to its positional `ColumnId`.
//!
//! # PromQL → canonical L2 mapping (summary)
//!
//! | PromQL | Canonical shape |
//! |---|---|
//! | `quantile_over_time(φ, m{f}[w])` | `Aggregate{[Quantile(φ)], TimeRange{w, Scan{predicates}}}` |
//! | `histogram_quantile(φ, <classic buckets>)` | `Aggregate{[HistogramQuantile(φ)]}` — cumulative-bucket interpolation (classic form recognised by `by (le)` / a `_bucket` metric / an `le` matcher) |
//! | `histogram_quantile(φ, <native hist / raw>)` | `Aggregate{[Quantile(φ)]}` over the fully-lowered arg (generic, sketch-able with an accuracy target) |
//! | `histogram_quantiles(v, "l", φ…)` | `Merge{Relabel{l=φᵢ, <the histogram_quantile(φᵢ, v) branch>}…}` — one branch per φ (issue #109) |
//! | `histogram_count/sum/avg/stddev/stdvar(v)`, `histogram_fraction(l,u,v)` | `Aggregate{[Histogram*]}` — per-series native-histogram accessors (issue #43) |
//! | `OUTER_op(inner_func(m[w]))` (e.g. `sum(rate(m[w]))`) | `Aggregate{[OUTER_op]}` over `Aggregate{[inner_func]}` — two levels |
//! | `OUTER_op(<any expr>)` (e.g. `max(sum by (job) (rate(m[w])))`, `sum(rate(a[w]) + rate(b[w]))`) | `Aggregate{[OUTER_op]}` over the fully-lowered `<any expr>` — arbitrary function nesting (issue #27) |
//! | `topk(k, <non-count expr>)` / `bottomk(k, <any expr>)` | `Sort{value} → Limit{k}` over the fully-lowered argument |
//! | `avg/min/max/sum_over_time(m[w])` | `Aggregate{[Avg/Min/Max/Sum], TimeRange{w}}` |
//! | `stddev/stdvar_over_time(m[w])` | `Aggregate{[StdDev/Variance], TimeRange{w}}` |
//! | `count_over_time(m[w])` | `Aggregate{[Count], TimeRange{w}}` |
//! | `last/first/mad/ts_of_min/ts_of_max/ts_of_first/ts_of_last_over_time(m[w])` | `Aggregate{[Last/First/Mad/TsOf…OverTime], TimeRange{w}}` — per-series range reducers (issue #51) |
//! | `sort`/`sort_desc(v)`, `sort_by_label[_desc](v,"l"…)` | `Sort{value \| label…}` (no `Limit`) — row-preserving reorder (issue #51); `min_of`/`max_of` scalar reducers → #89 |
//! | `rate/irate(m[w])` | `Aggregate{[Rate], TimeRange{w}}` — `irate` shares the `rate` *intent*; the avg-vs-last-two-samples difference is an L4 estimation method |
//! | `increase(m[w])` | `Aggregate{[Increase], TimeRange{w}}` |
//! | `changes`/`delta`/`idelta`/`deriv`/`resets`/`predict_linear`/`double_exponential_smoothing`(`m[w]`, …) | `Aggregate{[Changes/Delta/…], TimeRange{w}}` — per-series counter-derivative intents (issue #44) |
//! | `absent(v)` / `absent_over_time(m[w])` / `present_over_time(m[w])` | `Aggregate{[Absent/AbsentOverTime/PresentOverTime]}` — presence intents; the empty→synthesized-sample logic is L4 (issue #47) |
//! | `abs`/`ceil`/`sqrt`/`ln`/`clamp*`/`round`/trig(`v`), `pi()` | `Aggregate{[Math(f)]}` element-wise transform (issue #45); `pi()` → a `Scalar` leaf |
//! | `time()` / `timestamp`/`hour`/`day_of_week`/… (`v`) | `EvalTime` leaf / `Aggregate{[TimeFn(f)]}` (issue #46) |
//! | `vector(s)` / `scalar(v)` | `VectorFromScalar` / `ScalarFromVector` — the scalar⇄vector bridges (issue #48) |
//! | `label_replace(v,…)` / `label_join(v,…)` | `Relabel{dst, value}` — per-series label rewrite; value unchanged (issue #50) |
//! | `info(v, [selector])` | `InfoJoin{selector}` — label-enrichment join against the info metric(s); join keys resolved at L4 (issue #84) |
//! | `group` / `offset` / `@` / `info` | **rejected** — distinct semantics with no intent-algebra representation yet (`info` label-join → #84) |
//! | `OUTER by (dims) (…)` | `Aggregate.reduction = Reduce(by = dims)` (generic `topk by`/`bottomk` grouping → `Sort.partition_by`) |
//! | `count by (d) (…)` | `Aggregate{[Cardinality], …}` |
//! | `group(v)` / `count_values("l", v)` | `Aggregate{[Group]}` (constant 1) / `Aggregate{[CountValues{l}]}` (group-by-value + count, new label `l`) — issue #49 |
//! | `limitk(k, v)` / `limit_ratio(r, v)` | `Sample{LimitK(k) \| LimitRatio(r)}` — series-sampling selection, whole series kept unchanged (issue #86) |
//! | `topk(k, count_over_time(…))` | `Aggregate{[TopK{k}]}` (heavy-hitter intent) over the explicit inner `Aggregate{[Count]}` |
//! | `topk(k, <other>)` / `bottomk(k, …)` | `Sort{value} → Limit{k}` |
//! | `m{f}` | `Scan{predicates}` |
//! | `a OP b` | `BinaryOp{vector_match}` |
//! | `expr[r:res]` | `Subquery{r, res}` |
//! | `<selector> offset <d>` / `<selector> @ <ts>`/`start()`/`end()` | `TimeShift{shift}` over the selector's `Scan` — pass-through schema; a ranged selector shifts under its `TimeRange` (issue #40) |

use std::time::{Duration, SystemTime};

use promql_parser::label::{MatchOp, Matcher};
use promql_parser::parser::{
    self, token, AggregateExpr, AtModifier, BinaryExpr, Call, Expr, LabelModifier, Offset,
    VectorMatchCardinality, VectorSelector,
};

use asap_types::pre_asap::agg_intent::{
    is_frequency_heavy_hitter, AggIntent, MathFunc, RankingMeasure, TimeFunc,
};
use asap_types::pre_asap::query_expr::{
    AtModifier as L3AtModifier, BinaryOpKind, GroupKeys, GroupSide, L2QueryExpr as L2, Predicate,
    Reduction, SortKey, Source, TimeShift, VectorGrouping, VectorMatch, VectorMatchKind,
};
use asap_types::pre_asap::{
    ArithOp, ColumnRef, CompareOp, InfoMatcher, L2Expr, L3Scalar, SampleKind,
};
use asap_types::types::AccuracyTarget;

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
    CountValues {
        label: String,
    },
    TopK {
        k: u64,
        descending: bool,
    },
    /// `limitk`/`limit_ratio` — series-sampling selection (issue #86).
    Sample {
        kind: SampleKind,
    },
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
    // `Rate`/`Increase` carry no window of their own — unlike the old L2
    // `AggFunc::Rate{window}`, canonical `AggIntent::Rate`/`Increase` have no
    // window field either; `windowed_aggregate` reads `Inner.window`
    // uniformly for every intent, so it would be a redundant duplicate here.
    Rate,
    Increase,
    // Counter-derivative range functions (issue #44). The window rides on the
    // enclosing `TimeRange` node (like `*_over_time`), so these carry only
    // their non-window scalar params.
    Changes,
    Delta,
    IDelta,
    Deriv,
    Resets,
    PredictLinear(f64),
    DoubleExp { smoothing: f64, trend: f64 },
    // Additional range-vector reducers (issue #51). Per-series over the window
    // (like `*_over_time`); the window rides on the enclosing L2 `Window`.
    LastOverTime,
    FirstOverTime,
    MadOverTime,
    TsOfMinOverTime,
    TsOfMaxOverTime,
    TsOfFirstOverTime,
    TsOfLastOverTime,
}

struct Inner {
    metric: String,
    matchers: Vec<L2Expr>,
    window: Option<Duration>,
    func: Option<InnerFunc>,
    /// `offset` / `@` on the selector, carried to the `Source` (issue #40).
    shift: TimeShift,
}

/// Maximum PromQL expression nesting depth the walker accepts. Real queries
/// nest only a handful deep; this bounds the recursive descent (`walk` and the
/// mutually-recursive helpers) so a pathologically nested query is rejected
/// rather than overflowing the stack.
const MAX_DEPTH: usize = 256;

impl PromqlLowerer {
    /// Lower `query` to the canonical L2 tree, threading `accuracy` onto every
    /// approximate intent (`Count`, `Quantile`, `Cardinality`, `TopK`) as it is
    /// built — this front end constructs the canonical shape directly (issue
    /// #179), so accuracy is baked in here rather than threaded through a
    /// later, separate converter pass. `accuracy` rides the same ambient,
    /// thread-local mechanism as `histogram::CatalogGuard`
    /// — synchronous, one-query-at-a-time lowering, injected into the deep
    /// `walk` recursion without a parameter on every one of its ~30 mutually
    /// recursive signatures; consulted only at the handful of sites that build
    /// an accuracy-bearing `AggIntent`.
    pub fn lower(query: &str, accuracy: &AccuracyTarget) -> Result<L2> {
        let _guard = AccuracyGuard::install(accuracy.clone());
        let ast = parser::parse(query).map_err(LoweringError::Parse)?;
        // Reject over-deep nesting up front, so the (mutually-recursive) walk
        // below cannot blow the stack. The check itself recurses at most
        // `MAX_DEPTH` frames before erroring, so it is bounded too.
        check_depth(&ast, MAX_DEPTH)?;
        walk(&ast)
    }
}

std::thread_local! {
    static ACCURACY: std::cell::RefCell<AccuracyTarget> =
        const { std::cell::RefCell::new(AccuracyTarget::Exact) };
}

/// RAII guard installing `accuracy` as the ambient accuracy target for the
/// current thread's lowering, restoring the prior value on drop — same shape
/// as `histogram::CatalogGuard`.
struct AccuracyGuard(AccuracyTarget);

impl AccuracyGuard {
    fn install(accuracy: AccuracyTarget) -> Self {
        let prev = ACCURACY.with(|a| a.replace(accuracy));
        AccuracyGuard(prev)
    }
}

impl Drop for AccuracyGuard {
    fn drop(&mut self) {
        ACCURACY.with(|a| *a.borrow_mut() = std::mem::replace(&mut self.0, AccuracyTarget::Exact));
    }
}

/// The ambient accuracy target installed by the current [`PromqlLowerer::lower`] call.
fn current_accuracy() -> AccuracyTarget {
    ACCURACY.with(|a| a.borrow().clone())
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
        Expr::Call(call) if is_label_fn(call.func.name) => walk_label(call),
        Expr::Call(call) if is_sort_fn(call.func.name) => walk_sort(call),
        // A bare `min_of`/`max_of(consts…)` scalar query folds to a `Scalar`
        // leaf; a non-constant argument makes `num_expr` fail → rejected (#89).
        Expr::Call(call) if is_scalar_reducer_fn(call.func.name) => Ok(L2::Scalar(num_expr(expr)?)),
        Expr::Call(call) if call.func.name == "info" => walk_info(call),
        Expr::Call(call) => walk_call(call),
        Expr::Binary(bin) => walk_binary(bin),
        Expr::Paren(p) => walk(&p.expr),
        // `UnaryExpr` is built only by negation (`Neg`); unary `+` is folded to
        // identity and `-<literal>` to a negated `NumberLiteral`, so this wraps a
        // sub-expression whose samples must be sign-flipped. Now that a scalar
        // operand exists (#35), express it as `x * -1` — a constant-foldable
        // operand (`-(10*1024)`) collapses to a negated `Scalar` leaf; anything
        // else is a vector, sign-flipped by a `Mul` against `Scalar(-1)`. `Mul`
        // is commutative, so operand order carries no hazard (#36).
        Expr::Unary(u) => match num_expr(&u.expr) {
            Ok(v) => Ok(L2::Scalar(-v)),
            Err(_) => Ok(L2::BinaryOp {
                op: BinaryOpKind::Arith(ArithOp::Mul),
                lhs: Box::new(walk(&u.expr)?),
                rhs: Box::new(L2::Scalar(-1.0)),
                vector_match: None,
            }),
        },
        Expr::Subquery(sq) => Ok(L2::Subquery {
            range: sq.range,
            resolution: sq.step,
            child: Box::new(walk(&sq.expr)?),
        }),
        Expr::VectorSelector(vs) => {
            let (metric, matchers, shift) = vs_parts(vs)?;
            Ok(filtered_source(metric, matchers, shift))
        }
        Expr::MatrixSelector(ms) => {
            let (metric, matchers, shift) = vs_parts(&ms.vs)?;
            Ok(L2::TimeRange {
                range: ms.range,
                child: Box::new(filtered_source(metric, matchers, shift)),
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
        if subquery_range(arg_expr).is_none() {
            return Ok(None);
        }
        let inner = if call.func.name == "increase" {
            InnerFunc::Increase
        } else {
            InnerFunc::Rate
        };
        return Ok(Some(outer_aggregate(
            vec![],
            inner_intent(&inner),
            walk(arg_expr)?,
        )));
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
        "last_over_time" => (InnerFunc::LastOverTime, 0),
        "first_over_time" => (InnerFunc::FirstOverTime, 0),
        "mad_over_time" => (InnerFunc::MadOverTime, 0),
        "ts_of_min_over_time" => (InnerFunc::TsOfMinOverTime, 0),
        "ts_of_max_over_time" => (InnerFunc::TsOfMaxOverTime, 0),
        "ts_of_first_over_time" => (InnerFunc::TsOfFirstOverTime, 0),
        "ts_of_last_over_time" => (InnerFunc::TsOfLastOverTime, 0),
        "predict_linear" => (InnerFunc::PredictLinear(num_arg(call, 1)?), 0),
        "double_exponential_smoothing" => (
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
    Ok(Some(outer_aggregate(
        vec![],
        inner_intent(&inner),
        walk(arg_expr)?,
    )))
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
    let (keys, without) = resolve_group(agg)?;
    let outer = outer_kind(agg)?;

    // `without(...)` grouping is modelled only for the reducing aggregations
    // (sum/avg/count/…), whose grouping lives on an `Aggregate` node. `topk`/
    // `bottomk` (→ `Sort.partition_by`) and `limitk`/`limit_ratio` (→ `Sample`)
    // would need without-partitioning too; reject rather than silently lower
    // them as a `by` grouping (issue #39).
    if without && matches!(outer, Outer::TopK { .. } | Outer::Sample { .. }) {
        return Err(LoweringError::UnsupportedFeature(
            "`without(...)` is only supported on reducing aggregations, not \
             topk/bottomk/limitk"
                .into(),
        ));
    }

    // Fast path — the argument is a bare selector or a single range-vector
    // function (`rate`/`increase`/`*_over_time`). `lower_inner` lowers it via the
    // flat selector/call template, which also recognises the heavy-hitter
    // `topk(k, count_over_time(...))` shape. This is the common two-level case
    // (`sum by (job) (rate(m[5m]))`).
    //
    // General nesting — the argument is itself a composite expression: another
    // aggregate (`max(sum by (job) (rate(m[5m])))`), a binary op, a sub-query, or
    // a function lowered elsewhere. Lower it recursively with the same `walk`
    // used at the top level, then wrap it in the outer aggregation (issue #27; a
    // negated argument `sum(-m)` lowers here too, #36). A genuinely unsupported
    // inner expression surfaces its own error rather than being mislowered.
    //
    // Either way, `mark_without` flips the resulting outer `Aggregate` to the
    // exclusion form when the modifier was `without(...)`.
    let built = match lower_inner(&agg.expr) {
        Ok(inner) => build(inner, keys, outer)?,
        Err(_) => build_over_subtree(outer, keys, walk(&agg.expr)?)?,
    };
    Ok(mark_without(built, without))
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
    } else if op == token::T_LIMITK {
        // `limitk(k, v)` — up to k series per group (issue #86).
        Outer::Sample {
            kind: SampleKind::LimitK(count_param(agg)? as usize),
        }
    } else if op == token::T_LIMIT_RATIO {
        // `limit_ratio(r, v)` — an r-fraction of series per group (issue #86).
        Outer::Sample {
            kind: SampleKind::LimitRatio(ratio_param(agg)?),
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
/// Flip the outer `Aggregate` produced for a `without(...)` grouping into the
/// exclusion form. The reducing-aggregation `build` paths place that aggregate
/// at the root; `walk_aggregate` has already rejected the non-aggregate outers
/// (topk/limitk), so a `without` grouping always has an `Aggregate` here (issue
/// #39). A no-op when the modifier was `by`.
/// Flip the outer `Aggregate` produced for a `without(...)` grouping into the
/// exclusion form. A no-op when the modifier was `by`.
///
/// `reduction_for` (used by [`windowed_aggregate`]/[`outer_aggregate`] to
/// build this node) decides `PerEntity` vs `Reduce(by)` *without* knowing
/// about `without` yet — it only ever sees `by`-mode keys, since `without`'s
/// excluded-labels list is applied here, after the fact, exactly like the old
/// L2 `relational` tree's `mark_without` did (the L2→L3 converter read
/// `without` only after this front-end step had already set it). Whether
/// `reduction_for` picked `PerEntity` (only possible when `keys` was empty)
/// or `Reduce(by)`, the correct answer under `without(...)` is always
/// `Reduce(without(keys))`: a `without` grouping is never label-preserving —
/// per-entity requires `!by.is_without()` — so this both re-tags an existing
/// `Reduce` and upgrades a wrongly-early `PerEntity` guess, uniformly.
fn mark_without(l2: L2, without: bool) -> L2 {
    if !without {
        return l2;
    }
    match l2 {
        L2::Aggregate {
            reduction,
            measures,
            output_names,
            having,
            child,
        } => {
            let keys = match reduction {
                Reduction::Reduce(by) => by.keys().to_vec(),
                Reduction::PerEntity => vec![],
            };
            L2::Aggregate {
                reduction: Reduction::Reduce(GroupKeys::without(keys)),
                measures,
                output_names,
                having,
                child,
            }
        }
        other => other,
    }
}

fn build_over_subtree(outer: Outer, keys: Vec<ColumnRef>, child: L2) -> Result<L2> {
    Ok(match outer {
        // `walk_aggregate` always passes a real aggregator; `None` can't occur.
        Outer::None => child,
        Outer::Plain(intent) => outer_aggregate(keys, outer_intent(&intent), child),
        Outer::Count => outer_aggregate(
            keys,
            AggIntent::Cardinality {
                col: None,
                accuracy: current_accuracy(),
            },
            child,
        ),
        Outer::CountValues { label } => {
            outer_aggregate(keys, AggIntent::CountValues { label }, child)
        }
        Outer::Sample { kind } => L2::Sample {
            by: keys.into(),
            kind,
            child: Box::new(child),
        },
        Outer::TopK { k, descending } => {
            let sorted = L2::Sort {
                keys: vec![SortKey {
                    expr: L2Expr::Column(ColumnRef::SampleValue),
                    ascending: !descending,
                    nulls_first: false,
                }],
                partition_by: keys.into(),
                child: Box::new(child),
            };
            L2::Limit {
                n: k as usize,
                offset: 0,
                child: Box::new(sorted),
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
    if call.func.name == "histogram_quantiles" {
        return walk_histogram_quantiles(call);
    }
    if call.func.name == "histogram_quantile" {
        let phi = quantile_param(num_arg(call, 0)?)?;
        let arg_expr = arg(call, 1)?;
        // Two lowerings of `histogram_quantile(φ, …)`:
        //  - classic `le`-bucket form → `HistogramQuantile`, exact interpolation
        //    over cumulative buckets (not sketch-able).
        //  - native-histogram / raw-samples form → the generic `Quantile` intent
        //    (sketch-able).
        // The true signal is the argument's sample type: a declared
        // `HistogramKind` (issue #79) drives the choice when available, else we
        // fall back to the structural `by (le)`/`_bucket` heuristic (issue #43).
        let func = if histogram_arg_is_sketchable(arg_expr) {
            AggIntent::Quantile {
                col: None,
                q: phi,
                accuracy: current_accuracy(),
            }
        } else {
            AggIntent::HistogramQuantile { q: phi }
        };
        return Ok(outer_aggregate(vec![], func, walk(arg_expr)?));
    }
    // (histogram_quantile handled above; accessors below)
    let (func, vec_idx) = match call.func.name {
        "histogram_count" => (AggIntent::HistogramCount, 0),
        "histogram_sum" => (AggIntent::HistogramSum, 0),
        "histogram_avg" => (AggIntent::HistogramAvg, 0),
        "histogram_stddev" => (AggIntent::HistogramStdDev, 0),
        "histogram_stdvar" => (AggIntent::HistogramStdVar, 0),
        "histogram_fraction" => (
            AggIntent::HistogramFraction {
                lower: num_arg(call, 0)?,
                upper: num_arg(call, 1)?,
            },
            2,
        ),
        other => return Err(LoweringError::UnsupportedFunction(other.to_string())),
    };
    Ok(outer_aggregate(vec![], func, walk(arg(call, vec_idx)?)?))
}

/// `histogram_quantiles(v, "label", φ₀, φ₁, …)` — the experimental multi-quantile
/// form (issue #109). It is `histogram_quantile(φᵢ, v)` fanned out over the
/// quantiles, each branch's output series tagged with `label = φᵢ`.
///
/// Lowers to a `Merge` of one `Relabel`-wrapped quantile branch per φ, reusing
/// the single-quantile decision — classic `le`-buckets interpolate
/// (`HistogramQuantile`), native histograms / raw samples take the sketch-able
/// `Quantile` (issues #43 / #79) — so the two functions cannot diverge.
///
/// The vector argument is lowered once per branch, duplicating the subtree —
/// a future workload-level reuse pass could hoist it back into a single
/// producer.
///
/// Each branch aliases its value column to `value` rather than taking the
/// intent-keyed name (`quantile_0_5`, `quantile_0_9`, …). `Merge` derives its
/// schema from the first child, so branches that disagree on a column *name*
/// would make the merged schema silently misdescribe every branch but one. The
/// quantile is carried by the `label` column, which is exactly where Prometheus
/// puts it.
fn walk_histogram_quantiles(call: &Call) -> Result<L2> {
    let vec_expr = arg(call, 0)?;
    let label = str_arg(call, 1)?;
    if call.args.args.len() < 3 {
        return Err(LoweringError::MissingArgument(
            "histogram_quantiles(v, label, φ…) needs at least one quantile".into(),
        ));
    }
    // The bucket-vs-native choice is a property of the argument, not of φ.
    let sketchable = histogram_arg_is_sketchable(vec_expr);
    let branches = (2..call.args.args.len())
        .map(|i| {
            let phi = quantile_param(num_arg(call, i)?)?;
            let intent = if sketchable {
                AggIntent::Quantile {
                    col: None,
                    q: phi,
                    accuracy: current_accuracy(),
                }
            } else {
                AggIntent::HistogramQuantile { q: phi }
            };
            let child = walk(vec_expr)?;
            let reduction = reduction_for(&[], &intent, &child);
            let quantile = L2::Aggregate {
                reduction,
                measures: vec![intent],
                // Each branch aliases its value column to "value" (not the
                // intent-keyed default) so `Merge` — which derives its schema
                // from the first branch — doesn't silently misdescribe the rest.
                output_names: vec!["value".into()],
                having: None,
                child: Box::new(child),
            };
            Ok(L2::Relabel {
                dst: label.clone(),
                value: L2Expr::Literal(L3Scalar::Utf8(open_metrics_float(phi))),
                child: Box::new(quantile),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(L2::Merge { children: branches })
}

/// Prometheus's `labels.FormatOpenMetricsFloat` — how `histogram_quantiles`
/// renders each φ into its label value. Go's `%g` shortest round-trip, switching
/// to exponent form outside `[1e-4, 1e21)`, with `.0` appended when the result
/// would otherwise look like an integer.
fn open_metrics_float(v: f64) -> String {
    // The cases upstream hardcodes.
    if v == 1.0 {
        return "1.0".into();
    }
    if v == 0.0 {
        return "0.0".into();
    }
    if v == -1.0 {
        return "-1.0".into();
    }
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v.is_sign_positive() { "+Inf" } else { "-Inf" }.into();
    }
    let sci = format!("{v:e}");
    let exp: i32 = sci
        .split_once('e')
        .and_then(|(_, e)| e.parse().ok())
        .unwrap_or(0);
    if !(-4..21).contains(&exp) {
        // Go writes a signed, zero-padded two-digit exponent: `1e-05`.
        let (mantissa, _) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", exp.abs());
    }
    let s = format!("{v}");
    if s.contains(['e', '.']) {
        s
    } else {
        format!("{s}.0")
    }
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
    Ok(outer_aggregate(vec![], AggIntent::TimeFn(func), inner))
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
        "absent" => AggIntent::Absent,
        "absent_over_time" => AggIntent::AbsentOverTime,
        "present_over_time" => AggIntent::PresentOverTime,
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

/// The instant-vector reordering functions (issue #51).
fn is_sort_fn(name: &str) -> bool {
    matches!(
        name,
        "sort" | "sort_desc" | "sort_by_label" | "sort_by_label_desc"
    )
}

/// `sort`/`sort_desc(v)` reorder an instant vector by sample value;
/// `sort_by_label`/`sort_by_label_desc(v, "l"…)` reorder by label values. All
/// lower to a bare `Sort` (no `Limit`) over the vector argument — a faithful,
/// row-preserving reordering (issue #51).
fn walk_sort(call: &Call) -> Result<L2> {
    let child = Box::new(walk(arg(call, 0)?)?);
    let (by_value, ascending) = match call.func.name {
        "sort" => (true, true),
        "sort_desc" => (true, false),
        "sort_by_label" => (false, true),
        "sort_by_label_desc" => (false, false),
        other => return Err(LoweringError::UnsupportedFunction(other.to_string())),
    };
    let sort_key = |expr| SortKey {
        expr,
        ascending,
        nulls_first: false,
    };
    let keys = if by_value {
        vec![sort_key(L2Expr::Column(ColumnRef::SampleValue))]
    } else {
        // `sort_by_label(v, "l1", "l2", …)` — one key per label arg, in order.
        if call.args.args.len() < 2 {
            return Err(LoweringError::MissingArgument(
                "sort_by_label needs at least one label".into(),
            ));
        }
        (1..call.args.args.len())
            .map(|i| {
                Ok(sort_key(L2Expr::Column(ColumnRef::Named(str_arg(
                    call, i,
                )?))))
            })
            .collect::<Result<Vec<_>>>()?
    };
    Ok(L2::Sort {
        keys,
        partition_by: GroupKeys::none(),
        child,
    })
}

/// `info(v, [selector])` — a label-enrichment join. Lowers the input vector and
/// wraps it in an `InfoJoin` carrying the (optional) data-label selector's
/// matchers; the actual join against the info metric — on shared identifying
/// labels — is resolved at L4 (issue #84).
fn walk_info(call: &Call) -> Result<L2> {
    let child = Box::new(walk(arg(call, 0)?)?);
    let selector = match call.args.args.get(1) {
        Some(sel) => info_selector(sel)?,
        None => Vec::new(), // default: enrich from `target_info`
    };
    Ok(L2::InfoJoin { selector, child })
}

/// Extract the `info` data-label selector's matchers. Unlike an ordinary
/// selector these are **info-metric-side** and may carry regex / multiple
/// `__name__` matchers (which pick the info metric(s)), so they bypass the
/// single-metric `vs_parts` restriction and are kept symbolic.
fn info_selector(expr: &Expr) -> Result<Vec<InfoMatcher>> {
    match expr {
        Expr::VectorSelector(vs) => Ok(vs
            .matchers
            .matchers
            .iter()
            .map(|m| InfoMatcher {
                label: m.name.clone(),
                op: match &m.op {
                    MatchOp::Equal => CompareOp::Eq,
                    MatchOp::NotEqual => CompareOp::Ne,
                    MatchOp::Re(_) => CompareOp::Regex,
                    MatchOp::NotRe(_) => CompareOp::NotRegex,
                },
                value: m.value.clone(),
            })
            .collect()),
        Expr::Paren(p) => info_selector(&p.expr),
        other => Err(LoweringError::UnsupportedFeature(format!(
            "`info` data-label selector must be a label-matcher set, got {:?}",
            std::mem::discriminant(other)
        ))),
    }
}

/// The label-rewrite functions (issue #50).
fn is_label_fn(name: &str) -> bool {
    matches!(name, "label_replace" | "label_join")
}

/// `label_replace(v, dst, replacement, src, regex)` /
/// `label_join(v, dst, sep, src…)` — per-series label rewrites. Both lower to a
/// `Relabel` over the fully-lowered vector argument, differing only in the
/// expression that computes the destination label: `label_replace` a regex
/// capture-expansion, `label_join` a separator-joined concatenation. Sample
/// values are untouched; the regex-match-or-passthrough and capture-expansion
/// are L4/runtime concerns (issue #50).
fn walk_label(call: &Call) -> Result<L2> {
    let child = Box::new(walk(arg(call, 0)?)?);
    match call.func.name {
        "label_replace" => {
            let dst = str_arg(call, 1)?;
            let replacement = str_arg(call, 2)?;
            let src = str_arg(call, 3)?;
            let regex = str_arg(call, 4)?;
            let value = L2Expr::FunctionCall {
                name: "label_replace".into(),
                args: vec![
                    L2Expr::Column(ColumnRef::Named(src)),
                    L2Expr::Literal(L3Scalar::Utf8(regex)),
                    L2Expr::Literal(L3Scalar::Utf8(replacement)),
                ],
            };
            Ok(L2::Relabel { dst, value, child })
        }
        "label_join" => {
            // label_join(v, dst, sep, src_1, …, src_n) — needs ≥1 source label.
            if call.args.args.len() < 4 {
                return Err(LoweringError::MissingArgument(
                    "label_join(v, dst, sep, src…) needs at least one source label".into(),
                ));
            }
            let dst = str_arg(call, 1)?;
            let sep = str_arg(call, 2)?;
            let mut args = vec![L2Expr::Literal(L3Scalar::Utf8(sep))];
            for i in 3..call.args.args.len() {
                args.push(L2Expr::Column(ColumnRef::Named(str_arg(call, i)?)));
            }
            let value = L2Expr::FunctionCall {
                name: "label_join".into(),
                args,
            };
            Ok(L2::Relabel { dst, value, child })
        }
        other => Err(LoweringError::UnsupportedFunction(other.to_string())),
    }
}

/// The element-wise math / trig functions (issue #45).
fn is_math_fn(name: &str) -> bool {
    matches!(
        name,
        "abs"
            | "ceil"
            | "floor"
            | "exp"
            | "ln"
            | "log2"
            | "log10"
            | "sqrt"
            | "sgn"
            | "sin"
            | "cos"
            | "tan"
            | "asin"
            | "acos"
            | "atan"
            | "sinh"
            | "cosh"
            | "tanh"
            | "asinh"
            | "acosh"
            | "atanh"
            | "deg"
            | "rad"
            | "pi"
            | "round"
            | "clamp"
            | "clamp_min"
            | "clamp_max"
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
    Ok(outer_aggregate(vec![], AggIntent::Math(func), inner))
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
/// Whether `histogram_quantile(φ, arg)` lowers to the sketch-able generic
/// `Quantile` (`true`) or exact classic-bucket interpolation (`false`).
///
/// Metadata wins: if any metric referenced in `arg` has a declared
/// [`HistogramKind`](crate::histogram::HistogramKind), that decides it (issue
/// #79) — this fixes both the false-positive (a `…_bucket`-named non-histogram
/// declared `RawSamples`) and the false-negative (a suffix-less classic
/// histogram declared `ClassicBucket`) of the structural heuristic. With no
/// declaration, fall back to the structural `by (le)`/`_bucket` heuristic.
fn histogram_arg_is_sketchable(arg: &Expr) -> bool {
    let mut metrics = Vec::new();
    collect_metric_names(arg, &mut metrics);
    for metric in &metrics {
        if let Some(kind) = crate::histogram::current_kind_of(metric) {
            return kind.is_sketchable();
        }
    }
    !is_classic_bucket_arg(arg)
}

/// Collect the metric names of every vector/matrix selector reachable in `expr`
/// (for the metadata lookup in [`histogram_arg_is_sketchable`]). Skips
/// name-less selectors like `{le="…"}`.
fn collect_metric_names(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::VectorSelector(vs) => {
            if let Ok((metric, ..)) = vs_parts(vs) {
                if !metric.is_empty() {
                    out.push(metric);
                }
            }
        }
        Expr::MatrixSelector(ms) => {
            if let Ok((metric, ..)) = vs_parts(&ms.vs) {
                if !metric.is_empty() {
                    out.push(metric);
                }
            }
        }
        Expr::Paren(p) => collect_metric_names(&p.expr, out),
        Expr::Unary(u) => collect_metric_names(&u.expr, out),
        Expr::Subquery(s) => collect_metric_names(&s.expr, out),
        Expr::Aggregate(a) => collect_metric_names(&a.expr, out),
        Expr::Binary(b) => {
            collect_metric_names(&b.lhs, out);
            collect_metric_names(&b.rhs, out);
        }
        Expr::Call(c) => c
            .args
            .args
            .iter()
            .for_each(|a| collect_metric_names(a, out)),
        _ => {}
    }
}

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
            let (metric, matchers, shift) = vs_parts(vs)?;
            Ok(Inner {
                metric,
                matchers,
                window: None,
                func: None,
                shift,
            })
        }
        Expr::MatrixSelector(ms) => {
            let (metric, matchers, shift) = vs_parts(&ms.vs)?;
            Ok(Inner {
                metric,
                matchers,
                window: Some(ms.range),
                func: None,
                shift,
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
        let (metric, matchers, window, shift) = extract_matrix(arg(call, 0)?)?;
        Ok(Inner {
            metric,
            matchers,
            window: Some(window),
            func: Some(func),
            shift,
        })
    };
    match name {
        "rate" | "irate" => {
            let (metric, matchers, window, shift) = extract_matrix(arg(call, 0)?)?;
            Ok(Inner {
                metric,
                matchers,
                window: Some(window),
                func: Some(InnerFunc::Rate),
                shift,
            })
        }
        "increase" => {
            let (metric, matchers, window, shift) = extract_matrix(arg(call, 0)?)?;
            Ok(Inner {
                metric,
                matchers,
                window: Some(window),
                func: Some(InnerFunc::Increase),
                shift,
            })
        }
        "quantile_over_time" => {
            let phi = quantile_param(num_arg(call, 0)?)?;
            let (metric, matchers, window, shift) = extract_matrix(arg(call, 1)?)?;
            Ok(Inner {
                metric,
                matchers,
                window: Some(window),
                func: Some(InnerFunc::Quantile(phi)),
                shift,
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
        // Additional range-vector reducers (issue #51) — same windowed
        // per-series shape as the `*_over_time` family above.
        "last_over_time" => at0(InnerFunc::LastOverTime),
        "first_over_time" => at0(InnerFunc::FirstOverTime),
        "mad_over_time" => at0(InnerFunc::MadOverTime),
        "ts_of_min_over_time" => at0(InnerFunc::TsOfMinOverTime),
        "ts_of_max_over_time" => at0(InnerFunc::TsOfMaxOverTime),
        "ts_of_first_over_time" => at0(InnerFunc::TsOfFirstOverTime),
        "ts_of_last_over_time" => at0(InnerFunc::TsOfLastOverTime),
        "predict_linear" => {
            let (metric, matchers, window, shift) = extract_matrix(arg(call, 0)?)?;
            let seconds = num_arg(call, 1)?;
            Ok(Inner {
                metric,
                matchers,
                window: Some(window),
                func: Some(InnerFunc::PredictLinear(seconds)),
                shift,
            })
        }
        "double_exponential_smoothing" => {
            let (metric, matchers, window, shift) = extract_matrix(arg(call, 0)?)?;
            let smoothing = num_arg(call, 1)?;
            let trend = num_arg(call, 2)?;
            Ok(Inner {
                metric,
                matchers,
                window: Some(window),
                func: Some(InnerFunc::DoubleExp { smoothing, trend }),
                shift,
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
            None => Ok(filtered_source(inner.metric, inner.matchers, inner.shift)),
            Some(f) => {
                let intent = inner_intent(f);
                Ok(windowed_aggregate(inner, keys, intent))
            }
        },
        // An OUTER aggregation operator (`sum`/`avg`/…/`count`) over an inner
        // range-vector function (`rate`/`increase`/`*_over_time`) is a
        // two-level reduction: the inner func runs per series, the outer op
        // then aggregates across series. Collapsing them into one aggregate
        // silently drops a level — e.g. `sum(rate(m[w]))` must keep the `sum`.
        Outer::Plain(intent) => Ok(match &inner.func {
            None => windowed_aggregate(inner, keys, outer_intent(&intent)),
            Some(f) => {
                let inner_i = inner_intent(f);
                let inner_agg = windowed_aggregate(inner, vec![], inner_i);
                outer_aggregate(keys, outer_intent(&intent), inner_agg)
            }
        }),
        Outer::Count => Ok(match &inner.func {
            None => windowed_aggregate(inner, keys, cardinality()),
            Some(f) => {
                let inner_i = inner_intent(f);
                let inner_agg = windowed_aggregate(inner, vec![], inner_i);
                outer_aggregate(keys, cardinality(), inner_agg)
            }
        }),
        Outer::CountValues { label } => Ok(match &inner.func {
            None => windowed_aggregate(inner, keys, AggIntent::CountValues { label }),
            Some(f) => {
                let inner_i = inner_intent(f);
                let inner_agg = windowed_aggregate(inner, vec![], inner_i);
                outer_aggregate(keys, AggIntent::CountValues { label }, inner_agg)
            }
        }),
        Outer::Sample { kind } => {
            // Series sampling selects whole series unchanged — like generic
            // `topk`, a range-vector argument reduces per series first (label-
            // preserving), a bare selector is sampled directly; neither is
            // wrapped in a reducing aggregate (issue #86).
            let base = match inner.func.as_ref().map(inner_intent) {
                Some(intent) => windowed_aggregate(inner, vec![], intent),
                None => filtered_source(inner.metric, inner.matchers, inner.shift),
            };
            Ok(L2::Sample {
                by: keys.into(),
                kind,
                child: Box::new(base),
            })
        }
        Outer::TopK { k, descending } => {
            // Heavy-hitter only when ranking by frequency (`count`): that is a
            // first-class aggregate intent → `TopK`. Any other ranking (topk
            // over avg/quantile/rate, a bare selector's raw value, all bottomk)
            // is a generic order-by-value + limit and stays as the `Sort + Limit`
            // operator pair. The descending-plus-measure rule is shared with the
            // L3 canonicalize promotion so the two cannot drift (issue #38).
            let measure = match inner.func {
                Some(InnerFunc::Count) => RankingMeasure::Frequency,
                _ => RankingMeasure::NonAdditive,
            };
            let heavy_hitter = is_frequency_heavy_hitter(descending, measure);
            if heavy_hitter {
                // Preserve the Count intent in L3 so the intent algebra is
                // explicit about what is being computed. L4 may fuse the Count
                // and TopK into a single-pass heavy-hitter sketch (SpaceSaving /
                // CMS-with-heap), but that is a cost-model decision, not an L3
                // concern.
                let count_agg = windowed_aggregate(inner, vec![], inner_intent(&InnerFunc::Count));
                Ok(L2::Aggregate {
                    // A ranking always reduces (a `by`-empty TopK ranks the
                    // whole input into one ordering, never per-entity) — same
                    // as `lower::convert`'s `TopK` arm.
                    reduction: Reduction::Reduce(keys.into()),
                    measures: vec![AggIntent::TopK {
                        k: k as usize,
                        accuracy: current_accuracy(),
                    }],
                    output_names: vec![],
                    having: None,
                    child: Box::new(count_agg),
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
                let base = match inner.func.as_ref().map(inner_intent) {
                    Some(intent) => windowed_aggregate(inner, vec![], intent),
                    None => filtered_source(inner.metric, inner.matchers, inner.shift),
                };
                let sorted = L2::Sort {
                    keys: vec![SortKey {
                        expr: L2Expr::Column(ColumnRef::SampleValue),
                        ascending: !descending,
                        nulls_first: false,
                    }],
                    partition_by: keys.into(),
                    child: Box::new(base),
                };
                Ok(L2::Limit {
                    n: k as usize,
                    offset: 0,
                    child: Box::new(sorted),
                })
            }
        }
    }
}

/// Decide `PerEntity` vs `Reduce(by)` for a canonical `Aggregate` — the same
/// decision `asap_types::pre_asap::lower::convert`'s `Aggregate` arm makes
/// from schema, made here schema-independently instead (issue #179): it only
/// needs the keys list, the intent's own `is_per_series` flag, and whether
/// the child being wrapped is already a range/subquery marker. `without()` is
/// applied separately, post-hoc, by `mark_without` — see its doc for why
/// that's still correct here.
fn reduction_for(keys: &[ColumnRef], intent: &AggIntent<ColumnRef>, child: &L2) -> Reduction<ColumnRef> {
    let is_range_child = matches!(child, L2::TimeRange { .. } | L2::Subquery { .. });
    if keys.is_empty() && (intent.is_per_series() || is_range_child) {
        Reduction::PerEntity
    } else {
        Reduction::Reduce(GroupKeys::by(keys.to_vec()))
    }
}

/// `Aggregate{reduction, [intent]}` over `[TimeRange{w}] → Scan`. Always wraps
/// in `TimeRange` when there's a window — including for `Rate`/`Increase`,
/// whose window rides on `inner.window` too (set redundantly alongside the
/// intent itself): canonical `AggIntent::Rate`/`Increase` carry no window
/// field of their own, unlike the old L2 `AggFunc::Rate{window}` — "the range
/// is on the enclosing `TimeRange` node" is now true unconditionally, so
/// there's no more `skip_window` special case.
fn windowed_aggregate(inner: Inner, keys: Vec<ColumnRef>, intent: AggIntent<ColumnRef>) -> L2 {
    let base = filtered_source(inner.metric, inner.matchers, inner.shift);
    let child = match inner.window {
        Some(w) => L2::TimeRange {
            range: w,
            child: Box::new(base),
        },
        None => base,
    };
    let reduction = reduction_for(&keys, &intent, &child);
    L2::Aggregate {
        reduction,
        measures: vec![intent],
        // A single empty entry — never an override — so the resolver keeps
        // PromQL's intent-keyed output names ("sum", "quantile_0_99", …)
        // instead. Matches the shape `lower::convert` always produced for a
        // PromQL `AggItem` (`alias: None` → `.unwrap_or_default()` → `""`).
        output_names: vec![String::new()],
        having: None,
        child: Box::new(child),
    }
}

/// `Aggregate{reduction, [intent]}` directly over an existing L2 subtree — the
/// OUTER level of a two-level aggregation such as `sum(rate(…))` or the
/// `Aggregate{[Quantile]}` that wraps a `histogram_quantile` argument.
fn outer_aggregate(keys: Vec<ColumnRef>, intent: AggIntent<ColumnRef>, child: L2) -> L2 {
    let reduction = reduction_for(&keys, &intent, &child);
    L2::Aggregate {
        reduction,
        measures: vec![intent],
        output_names: vec![String::new()],
        having: None,
        child: Box::new(child),
    }
}

fn filtered_source(metric: String, matchers: Vec<L2Expr>, shift: TimeShift) -> L2 {
    let scan = L2::Scan {
        source: Source::TimeSeries { metric },
        predicates: matchers.into_iter().map(Predicate).collect(),
        // Usage-derived (PromQL is schemaless) — the Binder fills this in.
        schema: None,
    };
    if shift.is_identity() {
        scan
    } else {
        L2::TimeShift {
            shift,
            child: Box::new(scan),
        }
    }
}

/// `count(v)` / `count by (…) (v)` — SQL `COUNT(DISTINCT col)`'s PromQL
/// counterpart, over the (always implicit) sample value.
fn cardinality() -> AggIntent<ColumnRef> {
    AggIntent::Cardinality {
        col: None,
        accuracy: current_accuracy(),
    }
}

fn inner_intent(f: &InnerFunc) -> AggIntent<ColumnRef> {
    match f {
        InnerFunc::Quantile(q) => AggIntent::Quantile {
            col: None,
            q: *q,
            accuracy: current_accuracy(),
        },
        InnerFunc::Avg => AggIntent::Avg { col: None },
        InnerFunc::Min => AggIntent::Min { col: None },
        InnerFunc::Max => AggIntent::Max { col: None },
        InnerFunc::Sum => AggIntent::Sum { col: None },
        InnerFunc::StdDev => AggIntent::StdDev {
            col: None,
            population: true,
        },
        InnerFunc::Variance => AggIntent::Variance {
            col: None,
            population: true,
        },
        InnerFunc::Count => AggIntent::Count {
            accuracy: current_accuracy(),
        },
        InnerFunc::Rate => AggIntent::Rate,
        InnerFunc::Increase => AggIntent::Increase,
        InnerFunc::Changes => AggIntent::Changes,
        InnerFunc::Delta => AggIntent::Delta,
        InnerFunc::IDelta => AggIntent::IDelta,
        InnerFunc::Deriv => AggIntent::Deriv,
        InnerFunc::Resets => AggIntent::Resets,
        InnerFunc::PredictLinear(s) => AggIntent::PredictLinear { seconds: *s },
        InnerFunc::DoubleExp { smoothing, trend } => AggIntent::DoubleExpSmoothing {
            smoothing: *smoothing,
            trend: *trend,
        },
        InnerFunc::LastOverTime => AggIntent::LastOverTime,
        InnerFunc::FirstOverTime => AggIntent::FirstOverTime,
        InnerFunc::MadOverTime => AggIntent::MadOverTime,
        InnerFunc::TsOfMinOverTime => AggIntent::TsOfMinOverTime,
        InnerFunc::TsOfMaxOverTime => AggIntent::TsOfMaxOverTime,
        InnerFunc::TsOfFirstOverTime => AggIntent::TsOfFirstOverTime,
        InnerFunc::TsOfLastOverTime => AggIntent::TsOfLastOverTime,
    }
}

fn outer_intent(o: &OuterIntent) -> AggIntent<ColumnRef> {
    match o {
        OuterIntent::Sum => AggIntent::Sum { col: None },
        OuterIntent::Avg => AggIntent::Avg { col: None },
        OuterIntent::Min => AggIntent::Min { col: None },
        OuterIntent::Max => AggIntent::Max { col: None },
        OuterIntent::StdDev => AggIntent::StdDev {
            col: None,
            population: true,
        },
        OuterIntent::Variance => AggIntent::Variance {
            col: None,
            population: true,
        },
        OuterIntent::Quantile(q) => AggIntent::Quantile {
            col: None,
            q: *q,
            accuracy: current_accuracy(),
        },
        OuterIntent::Group => AggIntent::Group,
    }
}

/// Unwrap a (possibly parenthesised) string literal — `count_values` labels and
/// `label_replace`/`label_join` arguments are all string literals, sometimes
/// wrapped in parens (`count_values((("v")), …)`).
fn expr_str(expr: &Expr) -> Result<String> {
    match expr {
        Expr::StringLiteral(s) => Ok(s.val.clone()),
        Expr::Paren(p) => expr_str(&p.expr),
        other => Err(LoweringError::InvalidParameter(format!(
            "expected a string literal, got {:?}",
            std::mem::discriminant(other)
        ))),
    }
}

/// A `count_values` string parameter (the synthesized label name).
fn str_param(agg: &AggregateExpr) -> Result<String> {
    match &agg.param {
        Some(e) => expr_str(e),
        None => Err(LoweringError::MissingArgument(
            "`count_values` label parameter".into(),
        )),
    }
}

/// A call's `idx`-th argument as a string literal (`label_replace`/`label_join`).
fn str_arg(call: &Call, idx: usize) -> Result<String> {
    expr_str(arg(call, idx)?)
}

/// Resolve an aggregation's grouping modifier into a `(keys, without)` pair.
///
/// `by(labels)` → the kept labels, `without = false`. `without(labels)` → the
/// **excluded** labels, `without = true`: the kept set (the complement) can't be
/// enumerated under an open usage-derived schema, so it is deferred to the
/// runtime and only the excluded positions are carried (issue #39). Both forms
/// canonicalise their label set (sort + dedup) so equivalent groupings lower
/// identically. PromQL labels have no table qualifier → `ColumnRef::Named`.
fn resolve_group(agg: &AggregateExpr) -> Result<(Vec<ColumnRef>, bool)> {
    let canon = |labels: &[String]| -> Vec<ColumnRef> {
        let mut keys = labels.to_vec();
        keys.sort();
        keys.dedup();
        keys.into_iter().map(ColumnRef::Named).collect()
    };
    match &agg.modifier {
        None => Ok((vec![], false)),
        Some(LabelModifier::Include(ls)) => Ok((canon(&ls.labels), false)),
        Some(LabelModifier::Exclude(ls)) => Ok((canon(&ls.labels), true)),
    }
}

// ── Free helpers ──────────────────────────────────────────────────────────────

fn vs_parts(vs: &VectorSelector) -> Result<(String, Vec<L2Expr>, TimeShift)> {
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
    let shift = time_shift(vs.offset.as_ref(), vs.at.as_ref())?;
    Ok((metric, matchers, shift))
}

/// Convert the parser's `offset` / `@` modifiers into a [`TimeShift`] (issue
/// #40). Offset is signed milliseconds; `@ <ts>` (parser seconds → ms) becomes
/// an absolute anchor, `@ start()`/`@ end()` the range bounds.
fn time_shift(offset: Option<&Offset>, at: Option<&AtModifier>) -> Result<TimeShift> {
    let offset_ms = match offset {
        None => 0,
        Some(Offset::Pos(d)) => duration_ms(*d)?,
        Some(Offset::Neg(d)) => -duration_ms(*d)?,
    };
    let at = match at {
        None => None,
        Some(AtModifier::Start) => Some(L3AtModifier::Start),
        Some(AtModifier::End) => Some(L3AtModifier::End),
        Some(AtModifier::At(t)) => Some(L3AtModifier::Timestamp(system_time_ms(*t)?)),
    };
    Ok(TimeShift { offset_ms, at })
}

/// A `Duration` as `i64` milliseconds, rejecting an overflow rather than
/// silently truncating a pathologically large `offset`.
fn duration_ms(d: Duration) -> Result<i64> {
    i64::try_from(d.as_millis()).map_err(|_| {
        LoweringError::InvalidParameter("offset duration overflows i64 milliseconds".into())
    })
}

/// A `SystemTime` (`@ <ts>`) as `i64` milliseconds since the Unix epoch, signed
/// so pre-epoch anchors (the parser permits them) are preserved.
fn system_time_ms(t: SystemTime) -> Result<i64> {
    let ms = match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()),
        Err(e) => i64::try_from(e.duration().as_millis()).map(|ms| -ms),
    };
    ms.map_err(|_| {
        LoweringError::InvalidParameter("`@` timestamp overflows i64 milliseconds".into())
    })
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

fn extract_matrix(expr: &Expr) -> Result<(String, Vec<L2Expr>, Duration, TimeShift)> {
    match expr {
        Expr::MatrixSelector(ms) => {
            let (metric, matchers, shift) = vs_parts(&ms.vs)?;
            Ok((metric, matchers, ms.range, shift))
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
        // `min_of`/`max_of` are n-ary *scalar* reducers (issue #89). Fold them
        // when every argument is itself a constant scalar — this is the only
        // form the intent algebra can hold (there is no scalar min/max node). A
        // non-constant argument (`min_of(step(), 1s)`) fails the recursive fold
        // and propagates the error, so it stays rejected. `f64::min`/`max`
        // ignore NaN, matching PromQL's `min`/`max` NaN semantics.
        Expr::Call(c) if is_scalar_reducer_fn(c.func.name) => {
            let reduce = if c.func.name == "min_of" {
                f64::min
            } else {
                f64::max
            };
            c.args
                .args
                .iter()
                .map(|a| num_expr(a))
                .reduce(|acc, v| Ok(reduce(acc?, v?)))
                .ok_or_else(|| {
                    LoweringError::MissingArgument(format!("{} needs an argument", c.func.name))
                })?
        }
        other => Err(LoweringError::InvalidParameter(format!(
            "expected a numeric scalar, got {:?}",
            std::mem::discriminant(other)
        ))),
    }
}

/// The n-ary scalar min/max reducers, foldable when all arguments are constant
/// scalars (issue #89).
fn is_scalar_reducer_fn(name: &str) -> bool {
    matches!(name, "min_of" | "max_of")
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

/// `limit_ratio` ratio parameter — a finite value; Prometheus clamps it to
/// `[-1, 1]` (a negative ratio selects the complementary fraction). A non-finite
/// ratio (`limit_ratio(NaN, …)`) or a dynamic one (`time() % 17/17`, which
/// `num_param` can't fold) is rejected (issue #86).
fn ratio_param(agg: &AggregateExpr) -> Result<f64> {
    let r = num_param(agg)?;
    if !r.is_finite() {
        return Err(LoweringError::InvalidParameter(format!(
            "limit_ratio ratio must be finite, got {r}"
        )));
    }
    Ok(r.clamp(-1.0, 1.0))
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
