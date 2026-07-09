//! Layer 3 aggregation-intent vocabulary — "what to compute, not how".
//!
//! L3 carries intent ("compute a quantile to ε=0.01 accuracy"); the choice
//! between `HashAgg` / `SortAgg` / `SketchAgg(KLL{k=200})` is an L4 cost-aware
//! decision, not encoded here.
//!
//! `AggIntent::TopK` is a first-class *intent*: "the k most frequent keys by
//! value, to accuracy ε." Like `Quantile`, the exact-vs-approximate
//! realisation — an exact heap / sort+limit when `accuracy: Exact`, a
//! heavy-hitter sketch when approximate — is an L4 cost-aware decision, not
//! encoded here. The semantic distinction that *is* made at lowering is
//! intent vs operator: a heavy-hitter aggregate becomes `TopK`, whereas a
//! generic `ORDER BY value LIMIT k` stays as the `QueryExpr::Sort + Limit`
//! operator pair.

use serde::{Deserialize, Serialize};

use crate::intent_algebra::query_expr::DataModel;
use crate::intent_algebra::schema::{Column, ColumnId, DataType};
use crate::types::AccuracyTarget;

/// "What to compute" at L3 — the vocabulary the planner pivots on.
///
/// Grouping for `TopK` rides on the enclosing `QueryExpr::Aggregate.by`
/// (positional `ColumnId`s), like every other aggregate; the intent itself
/// carries only `k` + the accuracy target.
///
/// The single-column reducers (`Sum` / `Min` / `Max` / `Avg` / `StdDev` /
/// `Variance` / `Quantile` / `Cardinality`) carry `col: Option<ColumnId>` — the
/// positional input column they reduce. `None` is the PromQL convention "the
/// time-series sample value"; SQL `SUM(bytes), AVG(latency)` sets distinct
/// `Some(id)`s so a multi-aggregate node binds each reducer to the right
/// column, and `plan::bind` knows which column to summarise over (issue #115).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AggIntent {
    // ── Data-model-agnostic ──────────────────────────────────────────────
    Count {
        accuracy: AccuracyTarget,
    },
    Sum {
        #[serde(default)]
        col: Option<ColumnId>,
    },
    Min {
        #[serde(default)]
        col: Option<ColumnId>,
    },
    Max {
        #[serde(default)]
        col: Option<ColumnId>,
    },
    Avg {
        #[serde(default)]
        col: Option<ColumnId>,
    },
    /// Sample standard deviation when `population == false`; population stddev
    /// otherwise. PromQL `stddev` / `stddev_over_time`; SQL `STDDEV(col)`.
    StdDev {
        #[serde(default)]
        col: Option<ColumnId>,
        population: bool,
    },
    /// Variance — PromQL `stdvar` / `stdvar_over_time`; SQL `VARIANCE(col)`.
    Variance {
        #[serde(default)]
        col: Option<ColumnId>,
        population: bool,
    },
    /// φ-quantile of `col`. SQL `approx_percentile_cont(col, φ)` and
    /// `median(col)` (φ=0.5); PromQL `quantile(φ, …)` leaves `col` as `None`
    /// (the sample value).
    Quantile {
        #[serde(default)]
        col: Option<ColumnId>,
        q: f64,
        accuracy: AccuracyTarget,
    },
    /// Heavy-hitter top-k to the given accuracy. The group-by keys live on
    /// the enclosing `Aggregate.by`.
    //
    // Unlike `Quantile`/`Cardinality`, `TopK` ranks by the *aggregate output*
    // rather than a base column, so it carries no `col` — see #13 / #25.
    TopK {
        k: usize,
        accuracy: AccuracyTarget,
    },
    /// Distinct-value count of `col`. SQL `COUNT(DISTINCT col)`; PromQL
    /// `count_values` leaves `col` as `None` (the sample value).
    Cardinality {
        #[serde(default)]
        col: Option<ColumnId>,
        accuracy: AccuracyTarget,
    },

    // ── Time-series streaming derivatives ────────────────────────────────
    // Counter-reset adjustment; not equivalent to Sum/Count over a window.
    // The temporal range lives on the enclosing `QueryExpr::TimeRange` node,
    // not in the intent — this keeps the intent vocabulary range-agnostic.
    Rate,
    Increase,

    // ── Counter-derivative / range-vector functions (issue #44) ──────────
    // All per-series, label-preserving reductions of a single series' range
    // window to one value; the window rides on the enclosing `TimeRange`.
    // Each has distinct semantics and is deliberately NOT aliased to
    // `Rate`/`Increase`/`Count`.
    /// PromQL `changes(v[w])` — number of times the value changed in the window.
    Changes,
    /// PromQL `delta(v[w])` — difference between the first and last sample
    /// (gauge semantics; not counter-reset-adjusted).
    Delta,
    /// PromQL `idelta(v[w])` — difference between the last two samples.
    IDelta,
    /// PromQL `deriv(v[w])` — per-second derivative via simple linear
    /// regression over the window (gauges).
    Deriv,
    /// PromQL `resets(v[w])` — number of counter resets in the window.
    Resets,
    /// PromQL `predict_linear(v[w], t)` — linear-regression extrapolation of
    /// the value `t` seconds into the future.
    PredictLinear {
        /// The prediction horizon in seconds (the 2nd, scalar argument).
        seconds: f64,
    },
    /// PromQL `double_exponential_smoothing(v[w], sf, tf)` (a.k.a. the legacy
    /// `holt_winters`) — Holt-Winters double-exponential smoothing.
    DoubleExpSmoothing {
        /// Data (level) smoothing factor `sf` ∈ (0, 1).
        smoothing: f64,
        /// Trend smoothing factor `tf` ∈ (0, 1).
        trend: f64,
    },

    // ── Native-histogram accessors (issue #43) ───────────────────────────
    // Per-series extractions from a native-histogram instant vector — one
    // float per series, label-preserving. (Classic `le`-bucket
    // `histogram_quantile` stays a `Quantile` over the bucketed vector.)
    /// PromQL `histogram_count(v)` — observation count of each native histogram.
    HistogramCount,
    /// PromQL `histogram_sum(v)` — sum of observations.
    HistogramSum,
    /// PromQL `histogram_avg(v)` — mean (`sum/count`).
    HistogramAvg,
    /// PromQL `histogram_stddev(v)` — standard deviation of observations.
    HistogramStdDev,
    /// PromQL `histogram_stdvar(v)` — variance of observations.
    HistogramStdVar,
    /// PromQL `histogram_fraction(lower, upper, v)` — fraction of observations
    /// in `[lower, upper]`.
    HistogramFraction {
        lower: f64,
        upper: f64,
    },
    /// PromQL `histogram_quantile(φ, <le-bucketed vector>)` — the φ-quantile
    /// interpolated from classic cumulative `le` buckets. Distinct from the
    /// generic [`Quantile`](Self::Quantile) the *native*-histogram form lowers
    /// to: this is exact bucket interpolation, not a sketch-able quantile, so it
    /// carries no accuracy target and is a cross-series reduction over `le`
    /// (issue #43).
    HistogramQuantile {
        q: f64,
    },

    /// A per-sample element-wise math / trig transform (issue #45) — `abs`,
    /// `ceil`, `sqrt`, `ln`, `clamp_max`, the trig family, … Label-preserving:
    /// one value out per input sample. (`pi()` is a constant, lowered to a
    /// scalar leaf, not this.)
    Math(MathFunc),

    // ── Presence functions (issue #47) ───────────────────────────────────
    // `absent`/`present_over_time` — the value/emptiness of the argument
    // determines the output. The empty-result → synthesized-1-sample logic is
    // an L4/runtime concern; L3 only marks the operation. Modelled as
    // label-preserving so the argument's (matcher-derived) labels — which
    // `absent` synthesizes onto its output — stay in the schema.
    /// PromQL `absent(v)` — a 1-sample vector when the instant vector `v` has no
    /// matching series, else empty.
    Absent,
    /// PromQL `absent_over_time(v[w])` — `absent` over a range vector.
    AbsentOverTime,
    /// PromQL `present_over_time(v[w])` — value 1 per series that has any sample
    /// in the range (per-series).
    PresentOverTime,

    /// A time / calendar accessor (issue #46) — `timestamp`, `minute`, `hour`,
    /// `day_of_week`, … over each sample's timestamp (or, for the no-arg forms,
    /// over the evaluation time). Label-preserving per-series value transform.
    /// (`time()` is the evaluation time itself — a `QueryExpr::EvalTime` leaf,
    /// not this.)
    TimeFn(TimeFunc),

    // ── Extended aggregation operators (issue #49) ───────────────────────
    /// PromQL `group(v)` — a constant `1` per group ("group presence"). The
    /// grouping keys ride on the enclosing `Aggregate.by`. Deliberately NOT
    /// aliased to `Sum`/`Count`: the output value is always 1, independent of
    /// the input values (folding it onto `Sum` would change the result).
    Group,
    /// PromQL `count_values("l", v)` — group the input series by their sample
    /// *value* and count each distinct value, emitting that value as a new
    /// label `l`. Output = one series per distinct value, with labels
    /// `by-keys ∪ {l}` and value = the count. Unlike every other reducer this
    /// adds a synthesized `Utf8` label column, so `Aggregate` schema derivation
    /// special-cases it (two output columns, not one).
    CountValues {
        /// The name of the synthesized label carrying the stringified value.
        label: String,
    },

    // ── Additional range-vector reducers (issue #51) ─────────────────────
    // All per-series, label-preserving reductions of a single series' range
    // window to one value; the window rides on the enclosing `TimeRange`.
    /// PromQL `last_over_time(v[w])` — the most recent sample in the window.
    LastOverTime,
    /// PromQL `first_over_time(v[w])` — the oldest sample in the window.
    FirstOverTime,
    /// PromQL `mad_over_time(v[w])` — median absolute deviation over the window.
    MadOverTime,
    /// PromQL `ts_of_min_over_time(v[w])` — timestamp of the minimum sample.
    TsOfMinOverTime,
    /// PromQL `ts_of_max_over_time(v[w])` — timestamp of the maximum sample.
    TsOfMaxOverTime,
    /// PromQL `ts_of_first_over_time(v[w])` — timestamp of the first sample.
    TsOfFirstOverTime,
    /// PromQL `ts_of_last_over_time(v[w])` — timestamp of the last sample.
    TsOfLastOverTime,
}

/// Time / calendar accessor functions (issue #46), evaluated over a timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fn", rename_all = "snake_case")]
pub enum TimeFunc {
    /// `timestamp(v)` — the sample's own timestamp as a value.
    Timestamp,
    Minute,
    Hour,
    DayOfWeek,
    DayOfMonth,
    DayOfYear,
    Month,
    Year,
    DaysInMonth,
}

/// The element-wise math / trig functions (issue #45). Unary over the sample
/// value unless a variant carries scalar params (`clamp*`, `round`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "fn", rename_all = "snake_case")]
pub enum MathFunc {
    Abs,
    Ceil,
    Floor,
    Exp,
    Ln,
    Log2,
    Log10,
    Sqrt,
    Sgn,
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    Atan,
    Sinh,
    Cosh,
    Tanh,
    Asinh,
    Acosh,
    Atanh,
    /// `deg(v)` — radians → degrees.
    Deg,
    /// `rad(v)` — degrees → radians.
    Rad,
    /// `round(v, to_nearest)` — nearest multiple of `to_nearest` (default 1).
    Round { to_nearest: f64 },
    /// `clamp(v, min, max)`.
    Clamp { min: f64, max: f64 },
    /// `clamp_min(v, min)`.
    ClampMin { min: f64 },
    /// `clamp_max(v, max)`.
    ClampMax { max: f64 },
}

impl AggIntent {
    /// Which data model this intent semantically requires. L4 rules consult
    /// this to skip non-applicable intents (e.g. `Rate` over a tabular source).
    pub fn requires(&self) -> DataModel {
        match self {
            Self::Rate
            | Self::Increase
            | Self::Changes
            | Self::Delta
            | Self::IDelta
            | Self::Deriv
            | Self::Resets
            | Self::PredictLinear { .. }
            | Self::DoubleExpSmoothing { .. }
            | Self::HistogramCount
            | Self::HistogramSum
            | Self::HistogramAvg
            | Self::HistogramStdDev
            | Self::HistogramStdVar
            | Self::HistogramFraction { .. }
            | Self::Absent
            | Self::AbsentOverTime
            | Self::PresentOverTime
            | Self::TimeFn(_)
            | Self::LastOverTime
            | Self::FirstOverTime
            | Self::MadOverTime
            | Self::TsOfMinOverTime
            | Self::TsOfMaxOverTime
            | Self::TsOfFirstOverTime
            | Self::TsOfLastOverTime => DataModel::TimeSeries,
            _ => DataModel::Any,
        }
    }

    /// Whether this is a *per-series* reduction — it reduces a single series'
    /// samples over its range window (one value out per series), so it does
    /// **not** collapse across series and every label column is preserved.
    /// `rate`/`increase` carry their window in the intent. (Cross-series
    /// reductions like `sum`/`avg` over a series set return `false`.)
    pub fn is_per_series(&self) -> bool {
        matches!(
            self,
            Self::Rate
                | Self::Increase
                | Self::Changes
                | Self::Delta
                | Self::IDelta
                | Self::Deriv
                | Self::Resets
                | Self::PredictLinear { .. }
                | Self::DoubleExpSmoothing { .. }
                | Self::HistogramCount
                | Self::HistogramSum
                | Self::HistogramAvg
                | Self::HistogramStdDev
                | Self::HistogramStdVar
                | Self::HistogramFraction { .. }
                | Self::Math(_)
                | Self::Absent
                | Self::AbsentOverTime
                | Self::PresentOverTime
                | Self::TimeFn(_)
                | Self::LastOverTime
                | Self::FirstOverTime
                | Self::MadOverTime
                | Self::TsOfMinOverTime
                | Self::TsOfMaxOverTime
                | Self::TsOfFirstOverTime
                | Self::TsOfLastOverTime
        )
    }

    /// The positional input column this intent reduces, if it carries one.
    /// `None` = the synthetic time-series sample value (PromQL) or an
    /// argument-less aggregate (`Count` / `TopK`). Used by schema derivation to
    /// resolve each reducer's input column, and by `plan::bind` to pick the
    /// column a summary is built over.
    pub fn input_col(&self) -> Option<ColumnId> {
        match self {
            AggIntent::Sum { col }
            | AggIntent::Min { col }
            | AggIntent::Max { col }
            | AggIntent::Avg { col }
            | AggIntent::Quantile { col, .. }
            | AggIntent::Cardinality { col, .. }
            | AggIntent::StdDev { col, .. }
            | AggIntent::Variance { col, .. } => *col,
            _ => None,
        }
    }

    /// Output column name + type produced by this intent over `input`.
    /// Used by `QueryExpr::Aggregate`'s schema-derivation rule. The PromQL
    /// convention names the column after the intent kind so consumers can
    /// locate it without an alias lookup.
    pub fn output_column(&self, input: &Column) -> Column {
        match self {
            AggIntent::Count { .. } => col("count", DataType::Int64, false),
            AggIntent::Sum { .. } => col("sum", input.dtype.clone(), false),
            AggIntent::Min { .. } => col("min", input.dtype.clone(), input.nullable),
            AggIntent::Max { .. } => col("max", input.dtype.clone(), input.nullable),
            AggIntent::Avg { .. } => col("avg", DataType::Float64, false),
            AggIntent::StdDev { .. } => col("stddev", DataType::Float64, false),
            AggIntent::Variance { .. } => col("variance", DataType::Float64, false),
            AggIntent::Quantile { q, .. } => col(
                &format!("quantile_{}", quantile_suffix(*q)),
                DataType::Float64,
                false,
            ),
            // TopK output is a per-row struct/list; modeled as Utf8 at L3
            // (the L4 sketch-bound IR upgrades the dtype).
            AggIntent::TopK { k, .. } => col(&format!("topk_{k}"), DataType::Utf8, false),
            AggIntent::Cardinality { .. } => col("cardinality", DataType::Int64, false),
            AggIntent::Rate => col("rate", DataType::Float64, false),
            AggIntent::Increase => col("increase", DataType::Float64, false),
            // Counter-derivative range functions (issue #44) — all yield one
            // float per series (PromQL values are float64), named after the
            // function so consumers can locate the column without an alias.
            AggIntent::Changes => col("changes", DataType::Float64, false),
            AggIntent::Delta => col("delta", DataType::Float64, false),
            AggIntent::IDelta => col("idelta", DataType::Float64, false),
            AggIntent::Deriv => col("deriv", DataType::Float64, false),
            AggIntent::Resets => col("resets", DataType::Float64, false),
            AggIntent::PredictLinear { .. } => {
                col("predict_linear", DataType::Float64, false)
            }
            AggIntent::DoubleExpSmoothing { .. } => {
                col("double_exponential_smoothing", DataType::Float64, false)
            }
            AggIntent::HistogramCount => col("histogram_count", DataType::Float64, false),
            AggIntent::HistogramSum => col("histogram_sum", DataType::Float64, false),
            AggIntent::HistogramAvg => col("histogram_avg", DataType::Float64, false),
            AggIntent::HistogramStdDev => col("histogram_stddev", DataType::Float64, false),
            AggIntent::HistogramStdVar => col("histogram_stdvar", DataType::Float64, false),
            AggIntent::HistogramFraction { .. } => {
                col("histogram_fraction", DataType::Float64, false)
            }
            AggIntent::HistogramQuantile { .. } => {
                col("histogram_quantile", DataType::Float64, false)
            }
            AggIntent::Math(_) => col("value", DataType::Float64, false),
            AggIntent::Absent => col("absent", DataType::Float64, false),
            AggIntent::AbsentOverTime => col("absent_over_time", DataType::Float64, false),
            AggIntent::PresentOverTime => col("present_over_time", DataType::Float64, false),
            AggIntent::TimeFn(_) => col("value", DataType::Float64, false),
            // `group` — constant 1 per group.
            AggIntent::Group => col("group", DataType::Float64, false),
            // `count_values` — the *value* column (the per-value count). The
            // synthesized `label` column is added alongside it by `Aggregate`
            // schema derivation, which special-cases this intent.
            AggIntent::CountValues { .. } => col("count", DataType::Int64, false),
            // Additional range reducers (issue #51) — one float per series,
            // named after the function. The `ts_of_*` variants carry a
            // timestamp-as-float (PromQL values are float64).
            AggIntent::LastOverTime => col("last_over_time", DataType::Float64, false),
            AggIntent::FirstOverTime => col("first_over_time", DataType::Float64, false),
            AggIntent::MadOverTime => col("mad_over_time", DataType::Float64, false),
            AggIntent::TsOfMinOverTime => col("ts_of_min_over_time", DataType::Float64, false),
            AggIntent::TsOfMaxOverTime => col("ts_of_max_over_time", DataType::Float64, false),
            AggIntent::TsOfFirstOverTime => {
                col("ts_of_first_over_time", DataType::Float64, false)
            }
            AggIntent::TsOfLastOverTime => col("ts_of_last_over_time", DataType::Float64, false),
        }
    }
}

fn col(name: &str, dtype: DataType, nullable: bool) -> Column {
    Column::new(name, dtype, nullable)
}

/// `0.99` → `"0_99"`, `0.5` → `"0_5"`. Used by `Quantile` output naming so
/// `quantile_0_99` is a valid identifier downstream.
fn quantile_suffix(q: f64) -> String {
    let mut s = format!("{q}");
    if let Some(stripped) = s.strip_prefix('-') {
        s = format!("neg_{stripped}");
    }
    s.replace('.', "_")
}

// ── AggIntent helpers ────────────────────────────────────────────────────────

/// What a top-k ranks its groups by — the axis that decides whether the ranking
/// is a sketchable **heavy-hitter** or a generic order-by-value `Sort + Limit`.
///
/// Sketchability follows the *additivity* of the ranking measure, not "count"
/// per se: an additive per-key aggregate admits a single-pass heavy-hitter
/// sketch (CMS-with-heap / SpaceSaving), a non-additive one does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankingMeasure {
    /// Unweighted frequency — `count` of rows/samples per key. Additive, and the
    /// one heavy-hitter measure **realised today** (→ `AggIntent::TopK`).
    Frequency,
    /// Weighted frequency — an additive `sum` of a per-row weight per key.
    /// Sketchable in principle (weighted SpaceSaving), but no weighted
    /// heavy-hitter sketch is realised yet, so a `sum`-ranked top-k currently
    /// stays a generic `Sort + Limit`. Reserved so the axis is explicit; the
    /// realisation is L4 sketch selection (issues #6/#33).
    WeightedSum,
    /// A non-additive measure (`avg` / `quantile` / `min` / `max`) or a raw,
    /// un-aggregated value. Never a heavy-hitter — always generic.
    NonAdditive,
}

impl RankingMeasure {
    /// Whether a top-k ranked by this measure can be a heavy-hitter **with a
    /// sketch that exists today**. Only [`Frequency`](Self::Frequency) is
    /// realised; [`WeightedSum`](Self::WeightedSum) is additive (sketchable in
    /// principle) but has no implemented sketch yet, so it stays generic until
    /// one lands. This gate is the single knob to flip when that happens.
    pub fn is_realised_heavy_hitter(self) -> bool {
        matches!(self, RankingMeasure::Frequency)
    }
}

/// Classify the aggregate a top-k ranks by into its [`RankingMeasure`] — the
/// additivity axis heavy-hitter sketchability turns on.
pub fn ranking_measure(agg: &AggIntent) -> RankingMeasure {
    match agg {
        AggIntent::Count { .. } => RankingMeasure::Frequency,
        AggIntent::Sum { .. } => RankingMeasure::WeightedSum,
        _ => RankingMeasure::NonAdditive,
    }
}

/// The single rule that decides whether a top-k ranking is the frequency
/// **heavy-hitter** that [`AggIntent::TopK`] represents, as opposed to a generic
/// order-by-value `Sort + Limit`.
///
/// A ranking qualifies iff it takes the **top** k — `descending` (bottom-k, and
/// any ascending `ORDER BY … LIMIT`, never do) — **and** ranks by a measure with
/// a realised heavy-hitter sketch ([`RankingMeasure::is_realised_heavy_hitter`],
/// i.e. unweighted [`Frequency`](RankingMeasure::Frequency) today). Both places
/// that make this decision consult this one predicate so they cannot drift
/// (issue #38):
///
/// - the PromQL front-end gate, on `topk(k, count_over_time(…))` — `descending`
///   is the `topk`-vs-`bottomk` choice, `measure` is the inner range function
///   (`count_over_time` → `Frequency`, else `NonAdditive`);
/// - the shared L3 canonicalize promotion, on a
///   `Limit { Sort { … Aggregate([agg]) } }` — `descending` is the sort key's
///   direction, `measure` is [`ranking_measure`] of the ranked aggregate.
///
/// The two detectors recognise different *shapes* (a per-series
/// `count_over_time` vs. a cross-series `GROUP BY … COUNT`), which is why the
/// shape-matching stays language-specific; only this heavy-hitter *decision* is
/// shared.
pub fn is_frequency_heavy_hitter(descending: bool, measure: RankingMeasure) -> bool {
    descending && measure.is_realised_heavy_hitter()
}

/// Two instances of this aggregation can be merged
/// (`agg(A ∪ B) = combine(agg(A), agg(B))`). `Avg` / `StdDev` / `Variance`
/// need richer partial state than a single value, so they are not mergeable.
pub fn agg_is_mergeable(op: &AggIntent) -> bool {
    !matches!(
        op,
        AggIntent::Avg { .. } | AggIntent::StdDev { .. } | AggIntent::Variance { .. }
    )
}

/// Whether this op implies `exact_required` — no sketch benefit. The exact
/// intents are `Sum / Count / Avg / Min / Max`.
pub fn agg_is_exact(op: &AggIntent) -> bool {
    matches!(
        op,
        AggIntent::Sum { .. }
            | AggIntent::Count { .. }
            | AggIntent::Avg { .. }
            | AggIntent::Min { .. }
            | AggIntent::Max { .. }
            | AggIntent::Group
            | AggIntent::CountValues { .. }
    )
}

/// Accuracy parameter as a fractional ε (`0.0` for exact ops), unpacked from
/// the typed `AccuracyTarget` on Quantile / Cardinality / Count / TopK.
pub fn agg_accuracy(op: &AggIntent) -> f64 {
    match op {
        AggIntent::Quantile { accuracy, .. }
        | AggIntent::Cardinality { accuracy, .. }
        | AggIntent::Count { accuracy }
        | AggIntent::TopK { accuracy, .. } => accuracy_target_to_f64(accuracy),
        _ => 0.0,
    }
}

fn accuracy_target_to_f64(t: &AccuracyTarget) -> f64 {
    match t {
        AccuracyTarget::Exact => 0.0,
        AccuracyTarget::Epsilon(eps) => *eps,
        AccuracyTarget::EpsilonDelta { epsilon, .. } => *epsilon,
    }
}

/// Default `Cardinality` intent over the sample value — HLL standard error at
/// precision p=14.
pub fn default_cardinality() -> AggIntent {
    AggIntent::Cardinality {
        col: None,
        accuracy: AccuracyTarget::Epsilon(1.04 / ((1u64 << 14) as f64).sqrt()),
    }
}

/// Default `Quantile` intent over the sample value at φ = `q`, `accuracy = ε 0.01`.
pub fn default_quantile(q: f64) -> AggIntent {
    AggIntent::Quantile {
        col: None,
        q,
        accuracy: AccuracyTarget::Epsilon(0.01),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent_algebra::schema::{Column, DataType};

    fn c(name: &str, dtype: DataType) -> Column {
        Column::new(name, dtype, false)
    }

    #[test]
    fn output_column_names_are_intent_keyed() {
        let v = c("value", DataType::Float64);
        assert_eq!(
            AggIntent::Count {
                accuracy: AccuracyTarget::Exact
            }
            .output_column(&v)
            .name,
            "count"
        );
        assert_eq!(AggIntent::Sum { col: None }.output_column(&v).name, "sum");
        assert_eq!(
            AggIntent::Quantile {
                col: None,
                q: 0.99,
                accuracy: AccuracyTarget::Epsilon(0.01)
            }
            .output_column(&v)
            .name,
            "quantile_0_99"
        );
    }

    #[test]
    fn sum_preserves_input_dtype() {
        assert!(matches!(
            AggIntent::Sum { col: None }
                .output_column(&c("c", DataType::Int64))
                .dtype,
            DataType::Int64
        ));
    }

    #[test]
    fn mergeability_and_exactness() {
        assert!(agg_is_mergeable(&AggIntent::Sum { col: None }));
        assert!(!agg_is_mergeable(&AggIntent::Avg { col: None }));
        assert!(!agg_is_mergeable(&AggIntent::StdDev {
            col: None,
            population: false
        }));
        assert!(agg_is_exact(&AggIntent::Min { col: None }));
        assert!(!agg_is_exact(&default_cardinality()));
    }

    #[test]
    fn frequency_heavy_hitter_rule() {
        use RankingMeasure::*;
        // Heavy-hitter iff descending AND a realised (Frequency) measure (#38).
        // Frequency=count, NonAdditive=value measure, WeightedSum=sum (additive
        // but not yet sketch-realised, so it stays generic like the rest).
        assert!(is_frequency_heavy_hitter(true, Frequency));
        assert!(!is_frequency_heavy_hitter(false, Frequency));
        assert!(!is_frequency_heavy_hitter(true, NonAdditive));
        assert!(!is_frequency_heavy_hitter(true, WeightedSum));
    }

    #[test]
    fn ranking_measure_classifies_by_additivity() {
        use RankingMeasure::*;
        assert_eq!(
            ranking_measure(&AggIntent::Count {
                accuracy: AccuracyTarget::Exact
            }),
            Frequency
        );
        assert_eq!(ranking_measure(&AggIntent::Sum { col: None }), WeightedSum);
        assert_eq!(ranking_measure(&AggIntent::Avg { col: None }), NonAdditive);
        assert_eq!(ranking_measure(&AggIntent::Max { col: None }), NonAdditive);
        // Only Frequency is a realised heavy-hitter today; the additive
        // WeightedSum is reserved (sketchable, not yet implemented).
        assert!(Frequency.is_realised_heavy_hitter());
        assert!(!WeightedSum.is_realised_heavy_hitter());
        assert!(!NonAdditive.is_realised_heavy_hitter());
    }

    #[test]
    fn input_col_tracks_only_reducers() {
        assert_eq!(AggIntent::Sum { col: Some(3) }.input_col(), Some(3));
        assert_eq!(
            AggIntent::Avg { col: None }.input_col(),
            None,
            "None = PromQL sample value"
        );
        assert_eq!(
            AggIntent::Count {
                accuracy: AccuracyTarget::Exact
            }
            .input_col(),
            None
        );
    }

    #[test]
    fn agg_intent_serde_roundtrip() {
        for v in [
            AggIntent::Quantile {
                col: None,
                q: 0.99,
                accuracy: AccuracyTarget::Epsilon(0.01),
            },
            AggIntent::Quantile {
                col: Some(3),
                q: 0.99,
                accuracy: AccuracyTarget::Epsilon(0.01),
            },
            AggIntent::Cardinality {
                col: Some(2),
                accuracy: AccuracyTarget::Exact,
            },
        ] {
            let json = serde_json::to_string(&v).unwrap();
            let back: AggIntent = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back);
        }
    }

    /// `col` is `#[serde(default)]`, so L3 serialized before issue #115 — with
    /// no `col` key — still deserializes, as the sample-value convention `None`.
    #[test]
    fn agg_intent_serde_reads_pre_115_payloads() {
        let legacy = r#"{"kind":"quantile","q":0.99,"accuracy":"Exact"}"#;
        assert_eq!(
            serde_json::from_str::<AggIntent>(legacy).unwrap(),
            AggIntent::Quantile {
                col: None,
                q: 0.99,
                accuracy: AccuracyTarget::Exact
            }
        );

        let legacy = r#"{"kind":"cardinality","accuracy":"Exact"}"#;
        assert_eq!(
            serde_json::from_str::<AggIntent>(legacy).unwrap(),
            AggIntent::Cardinality {
                col: None,
                accuracy: AccuracyTarget::Exact
            }
        );
    }
}
