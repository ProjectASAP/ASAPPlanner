//! The metamorphic relations: how each derives a related workload from a
//! workload and one of its queries, and how its outcome is judged.
//!
//! Marginal-cost relations (the reuse patterns of the planner-layering,
//! primitive-schema and accuracy-model design docs; README's coverage
//! table cites each) add a query `q'` derivable from query `q` and compare
//! the planner's marginal cost of `q'` with a witness plan's
//! ([`super`] module doc). Monotonicity and invariance relations compare
//! two plannings directly.

use std::collections::HashMap;
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use asap_types::ir::operator::agg_intent::AggIntent;
use asap_types::ir::operator::operator_properties::Reduction;
use asap_types::ir::scalar::{ColumnRef, CompareOpKind, ScalarValue};
use asap_types::ir::schema::family_capability::FamilyCapability;
use asap_types::ir::schema::sketch::{SketchAlgorithm, SketchStatistic};
use asap_types::ir::schema::{DataType, FieldDataType};
use asap_types::ir::{
    ASAPOp, ExprSemantics, NonASAPOp, Operator, OperatorNode, Predicate, ProjectItem, QueryRoot,
    ScalarExpr,
};

use super::generate::{
    label_value, looser, tighter, OuterAggregation, OuterOp, PromqlSpec, QuerySpec, RangeFunction,
    SqlAggregate,
};
use super::{FuzzQuery, FuzzWorkload, Language, Planned, Planner, Priced};

/// A metamorphic relation the tester checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    /// (a) `q'` = `q` with a looser accuracy target: derived by reading `q`.
    LooserAccuracy,
    /// (b) `q'` = grouped `q` plus an equality filter on a group key:
    /// derived by a selection (`Filter`) over `q`'s result.
    FilterOnGroupKey,
    /// (c) `q'` = `q` grouped by fewer keys: derived by an exact rollup
    /// (`Aggregate` over `q`'s result) or a sketch rollup (`SummaryMerge`
    /// with `group_by` over `q`'s state).
    CoarserGrouping,
    /// (d) `q'` = `q` over a shorter window on the same pane grid: derived
    /// by merging the newest panes of `q`'s pane chain.
    NestedShorterWindow,
    /// (e) `q'` = `q` at another quantile rank: derived by one more
    /// estimate from `q`'s sketch.
    OtherQuantileRank,
    /// (f) `q'` = `q`: derived by reading `q`.
    ExactDuplicate,
    /// (g) `q'` = an outer operator over `q` (`topk`, `bottomk`, `max`,
    /// `quantile` or a threshold, by `q`'s own outer operator): derived by
    /// that operator over `q`'s result.
    OuterOverResult,
    /// (h) `q'` = another frequency statistic of `q`'s SQL column (row
    /// count, distinct count, L2): derived by another estimate from `q`'s
    /// state when its family answers it, or from one UnivMon the planner
    /// sizes for both.
    OtherStatistic,
    /// (i) `q'` = `q` evaluated at a longer repeat interval: derived by
    /// reading `q`'s result or state at its own cadence.
    OtherRepeatInterval,
    /// (j) `q'` = `q`'s top-k with a smaller k: derived by a top-k over
    /// `q`'s result.
    SmallerTopK,
    /// (k) `q'` = `q` over the union of two disjoint selections, both of
    /// which the workload computes: derived by merging the two states.
    UnionOfSelections,
    /// (l) `q'` = a ratio of `q` and a sibling query (a filtered share of
    /// `q`, or `q`'s `sum_over_time` over its `count_over_time`): derived by
    /// the division with `q` read from `q`'s result.
    BinaryOverResult,
    /// (m) `q'` = `q` over a longer window that is a multiple of it:
    /// derived by lengthening `q`'s pane chain, which `q` reads the newest
    /// panes of.
    LongerWindow,
    /// (n) `q'` = `q` over a longer window and grouped by one key fewer:
    /// derived by a rollup over (m)'s derivation.
    LongerWindowRollup,
    /// (o) `q'` = `q` with `rate` and `increase` swapped: derived by scaling
    /// `q`'s result by the window length.
    EquivalentRangeFunction,
    /// Tightening a query's accuracy target never lowers cost.
    TighterAccuracy,
    /// Adding a query never lowers cost.
    AddedQuery,
    /// Dropping raw-data retention never lowers cost.
    NoRawRetention,
    /// Keeping query-time results across evaluations never raises cost.
    ResultsRetained,
    /// Relaxing a latency bound never raises cost.
    RelaxedLatency,
    /// Renaming the metric (table) and labels (columns) keeps the cost.
    Renamed,
    /// Reordering the queries keeps the cost.
    Reordered,
    /// Planning the workload with a disjoint copy of itself doubles the
    /// cost.
    DisjointCopy,
}

impl Relation {
    pub const ALL: [Relation; 23] = [
        Relation::LooserAccuracy,
        Relation::FilterOnGroupKey,
        Relation::CoarserGrouping,
        Relation::NestedShorterWindow,
        Relation::OtherQuantileRank,
        Relation::ExactDuplicate,
        Relation::OuterOverResult,
        Relation::OtherStatistic,
        Relation::OtherRepeatInterval,
        Relation::SmallerTopK,
        Relation::UnionOfSelections,
        Relation::BinaryOverResult,
        Relation::LongerWindow,
        Relation::LongerWindowRollup,
        Relation::EquivalentRangeFunction,
        Relation::TighterAccuracy,
        Relation::AddedQuery,
        Relation::NoRawRetention,
        Relation::ResultsRetained,
        Relation::RelaxedLatency,
        Relation::Renamed,
        Relation::Reordered,
        Relation::DisjointCopy,
    ];

    /// The CLI name, e.g. `a-looser-accuracy`.
    pub fn name(self) -> &'static str {
        match self {
            Relation::LooserAccuracy => "a-looser-accuracy",
            Relation::FilterOnGroupKey => "b-filter-on-group-key",
            Relation::CoarserGrouping => "c-coarser-grouping",
            Relation::NestedShorterWindow => "d-nested-shorter-window",
            Relation::OtherQuantileRank => "e-other-quantile-rank",
            Relation::ExactDuplicate => "f-exact-duplicate",
            Relation::OuterOverResult => "g-outer-over-result",
            Relation::OtherStatistic => "h-other-statistic",
            Relation::OtherRepeatInterval => "i-other-repeat-interval",
            Relation::SmallerTopK => "j-smaller-topk",
            Relation::UnionOfSelections => "k-union-of-selections",
            Relation::BinaryOverResult => "l-binary-over-result",
            Relation::LongerWindow => "m-longer-window",
            Relation::LongerWindowRollup => "n-longer-window-rollup",
            Relation::EquivalentRangeFunction => "o-equivalent-range-function",
            Relation::TighterAccuracy => "tighter-accuracy",
            Relation::AddedQuery => "added-query",
            Relation::NoRawRetention => "no-raw-retention",
            Relation::ResultsRetained => "results-retained",
            Relation::RelaxedLatency => "relaxed-latency",
            Relation::Renamed => "renamed",
            Relation::Reordered => "reordered",
            Relation::DisjointCopy => "disjoint-copy",
        }
    }

    /// Parses a CLI name or, for (a) to (o), its letter.
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| {
            r.name() == text
                || r.name()
                    .split_once('-')
                    .is_some_and(|(l, _)| l == text && l.len() == 1)
        })
    }

    /// Whether the relation compares a marginal cost with a witness's.
    pub fn is_marginal(self) -> bool {
        matches!(
            self,
            Relation::LooserAccuracy
                | Relation::FilterOnGroupKey
                | Relation::CoarserGrouping
                | Relation::NestedShorterWindow
                | Relation::OtherQuantileRank
                | Relation::ExactDuplicate
                | Relation::OuterOverResult
                | Relation::OtherStatistic
                | Relation::OtherRepeatInterval
                | Relation::SmallerTopK
                | Relation::UnionOfSelections
                | Relation::BinaryOverResult
                | Relation::LongerWindow
                | Relation::LongerWindowRollup
                | Relation::EquivalentRangeFunction
        )
    }

    /// Whether the relation is checked once per query of the workload
    /// (otherwise once per workload).
    pub fn per_query(self) -> bool {
        self.is_marginal() || matches!(self, Relation::TighterAccuracy | Relation::RelaxedLatency)
    }

    /// The statement checked, for reports.
    pub fn statement(self) -> &'static str {
        match self {
            r if r.is_marginal() => "cost(W ∪ {q'}) − cost(W) ≤ derive_cost + tolerance",
            Relation::TighterAccuracy | Relation::AddedQuery | Relation::NoRawRetention => {
                "cost(W') ≥ cost(W) − tolerance"
            }
            Relation::ResultsRetained | Relation::RelaxedLatency => {
                "cost(W') ≤ cost(W) + tolerance"
            }
            Relation::DisjointCopy => "|cost(W ∪ copy(W)) − 2·cost(W)| ≤ tolerance",
            _ => "|cost(W') − cost(W)| ≤ tolerance",
        }
    }
}

/// One check: a relation applied to a workload (at one of its queries for
/// a per-query relation).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Check {
    pub relation: Relation,
    pub workload: FuzzWorkload,
    /// The query `q` the relation derives from (0 for a whole-workload
    /// relation).
    pub target: usize,
    /// The query [`Relation::AddedQuery`] adds.
    pub added: Option<FuzzQuery>,
}

/// How a derived query `q'` is answered from `q`'s node in the witness.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Witness {
    /// `q'` reads `q`'s root node itself.
    Same,
    /// `Filter(column = value)` over `q`'s root.
    Filter { column: String, value: String },
    /// The frontends' lowering of `text`, a query over `q`'s text, with the
    /// lowering of `q` inside it replaced by `q`'s root.
    Graft { text: String },
    /// An exact `Aggregate` (`op` of the value column) grouped by the
    /// `labels` columns over `q`'s result.
    ExactRollup { labels: Vec<String>, op: RollupOp },
    /// `q`'s topmost top-k (`Limit` or `TopK` aggregate) at `to` rows per
    /// group instead of `from`, over the same input.
    SmallerTopK { from: usize, to: usize },
    /// `q`'s root estimate reading a `SummaryMerge` with `group_by` the
    /// `labels` columns over its state, when the family merges groups.
    SketchRollup { labels: Vec<String> },
    /// `q`'s `SummaryMerge` over a pane chain merging only the newest panes
    /// covering `window_ms`.
    NewestPanes { window_ms: u64 },
    /// `q`'s topmost quantile estimate (or exact quantile aggregate) at rank
    /// `to` instead of `from`.
    QuantileRank { from: f64, to: f64 },
    /// `q`'s root estimate reading `statistic` instead, when the state's
    /// family answers it.
    OtherStatistic { statistic: SketchStatistic },
    /// `q`'s root estimate over a `SummaryMerge` of its state and the state
    /// of `W`'s last query (the other selection, added to `W`).
    UnionWithLast,
    /// `q`'s pane chain lengthened to cover `window_ms`: every reader of
    /// the chain in `W`'s plan merges its old newest panes of the longer
    /// chain, and `q'` merges all of them, then applies `rollup` when set.
    LongerWindow {
        window_ms: u64,
        rollup: Option<Box<Witness>>,
    },
    /// Like [`Witness::Graft`], for a `text` that also reads the sibling
    /// query `operand`: `W` is planned with `operand` added, and both its
    /// and `q`'s lowerings inside `text` read their planned roots.
    GraftWithOperand { text: String, operand: String },
    /// `W ∪ {q'}` planned with UnivMon as the only sketch family, so the
    /// planner sizes one UnivMon for both statistics (its summary-capability
    /// rule), priced with every family.
    UnivMonOnly,
}

/// The exact rollup of a sum, count (a sum of counts), max or min.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollupOp {
    Sum,
    Max,
    Min,
}

/// A derived query, how the witness answers it, and for
/// [`Relation::UnionOfSelections`] the query added to `W` first.
#[derive(Debug, Clone, PartialEq)]
pub struct Derived {
    pub query: FuzzQuery,
    pub witnesses: Vec<Witness>,
    pub base_addition: Option<FuzzQuery>,
}

/// What a check measured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measurement {
    /// `W`'s cost per second and selected plan.
    pub base_cost_per_second: f64,
    pub base_plan: String,
    /// The related workload's (`W ∪ {q'}`, or `W'`).
    pub related_cost_per_second: f64,
    pub related_plan: String,
    /// The derived or added query's text, for a relation that adds one.
    pub related_query: Option<String>,
    /// The witness (marginal relations) or the confirming plan (a violated
    /// monotonicity relation: the other side's logical plan priced for this
    /// side).
    pub witness: Option<WitnessReport>,
    /// The bound `related_cost_per_second` was checked against, tolerance
    /// included.
    pub bound_per_second: f64,
    pub tolerance_per_second: f64,
    /// One line: the relation's numbers.
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WitnessReport {
    /// How `q'` was derived (marginal relations).
    pub derivation: Option<Witness>,
    pub cost_per_second: f64,
    pub plan: String,
    /// Marginal relations: `W`'s selected logical plan priced the same way.
    pub base_reference_cost_per_second: Option<f64>,
    /// `cost_per_second − base_reference_cost_per_second`.
    pub derive_cost_per_second: Option<f64>,
    /// The witness's per-node cost of the nodes only `q'` reads (the derived
    /// operators alone) and their operators.
    pub derived_node_cost_per_second: Option<f64>,
    pub derived_operators: Vec<String>,
    /// Whether the witness's `Filter` on a group key kept its input's
    /// guarantee, which Stage 1's guarantee composition does not give it
    /// (so the witness passes Stage 3 only under that rule).
    pub key_filter_keeps_input_guarantee: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Holds(Measurement),
    Violated(Measurement),
    /// Not applicable to this workload, a side failed to plan, or no
    /// witness could be built or priced.
    Inconclusive {
        reason: String,
    },
}

/// An inconclusive check's reason, as the `Err` of the evaluation helpers.
fn inconclusive<T>(reason: impl Into<String>) -> Result<T, String> {
    Err(reason.into())
}

/// The ranks [`Relation::OtherQuantileRank`] moves between.
fn other_rank(q: f64) -> f64 {
    if (q - 0.5).abs() < 1e-9 {
        0.9
    } else {
        0.5
    }
}

/// The largest grid-aligned window shorter than `window_ms` that divides it.
fn shorter_window(window_ms: u64) -> Option<u64> {
    [300_000, 120_000, 60_000, 30_000]
        .into_iter()
        .find(|&w| w < window_ms && window_ms.is_multiple_of(w))
}

fn repeat_interval_ms(query: &FuzzQuery) -> Option<u64> {
    serde_json::to_value(&query.entry.demand).ok()?["fixed_interval"].as_u64()
}

fn with_repeat_interval(query: &FuzzQuery, interval_ms: u64) -> FuzzQuery {
    let mut query = query.clone();
    query.entry.demand =
        serde_json::from_value(json!({"fixed_interval": interval_ms})).expect("a fixed interval");
    query
}

/// The derived query of a marginal relation at `target`, or `None` when
/// the relation does not apply to it.
pub fn derive(relation: Relation, workload: &FuzzWorkload, target: usize) -> Option<Derived> {
    let q = &workload.queries[target];
    let derived = |query: FuzzQuery, witnesses: Vec<Witness>| {
        Some(Derived {
            query,
            witnesses,
            base_addition: None,
        })
    };
    match relation {
        Relation::LooserAccuracy => {
            derived(q.with_accuracy(looser(q.accuracy())?), vec![Witness::Same])
        }
        Relation::ExactDuplicate => derived(q.clone(), vec![Witness::Same]),
        Relation::OtherRepeatInterval => {
            let interval = repeat_interval_ms(q)?;
            let longer = [30_000, 60_000, 120_000, 300_000]
                .into_iter()
                .find(|&i| i > interval && i % interval == 0)?;
            derived(with_repeat_interval(q, longer), vec![Witness::Same])
        }
        Relation::FilterOnGroupKey => match q.spec.as_ref()? {
            QuerySpec::Promql(spec) => {
                let outer = spec.outer.as_ref()?;
                if matches!(outer.op, OuterOp::Topk(_)) && !outer.without && outer.labels.is_empty()
                {
                    return None;
                }
                let label = spec
                    .group_labels()
                    .into_iter()
                    .find(|l| spec.matchers.iter().all(|(m, _)| m != l))?;
                let value = label_value(&label, 0);
                let mut filtered = spec.clone();
                filtered.matchers.push((label.clone(), value.clone()));
                derived(
                    q.with_spec(QuerySpec::Promql(filtered)),
                    vec![Witness::Filter {
                        column: label,
                        value,
                    }],
                )
            }
            QuerySpec::Sql(spec) => {
                let column = spec
                    .group_by
                    .iter()
                    .find(|c| spec.filters.iter().all(|(f, _)| f != *c))?
                    .clone();
                let value = label_value(&column, 0);
                let mut filtered = spec.clone();
                filtered.filters.push((column.clone(), value.clone()));
                derived(
                    q.with_spec(QuerySpec::Sql(filtered)),
                    vec![Witness::Filter { column, value }],
                )
            }
        },
        Relation::CoarserGrouping => {
            let (coarse, witnesses) = coarser(q.spec.as_ref()?)?;
            derived(q.with_spec(coarse), witnesses)
        }
        Relation::NestedShorterWindow => {
            let spec = match q.spec.as_ref()? {
                QuerySpec::Promql(spec) => {
                    // A counter's rate and increase read the window length
                    // and its boundary samples, not only merged pane state.
                    if matches!(spec.range?, RangeFunction::Rate | RangeFunction::Increase) {
                        return None;
                    }
                    let mut shorter = spec.clone();
                    shorter.window_ms = shorter_window(spec.window_ms)?;
                    QuerySpec::Promql(shorter)
                }
                QuerySpec::Sql(spec) => {
                    let mut shorter = spec.clone();
                    shorter.window_ms = shorter_window(spec.window_ms)?;
                    QuerySpec::Sql(shorter)
                }
            };
            let window_ms = match &spec {
                QuerySpec::Promql(s) => s.window_ms,
                QuerySpec::Sql(s) => s.window_ms,
            };
            derived(q.with_spec(spec), vec![Witness::NewestPanes { window_ms }])
        }
        Relation::OtherQuantileRank => {
            let (spec, from) = match q.spec.as_ref()? {
                QuerySpec::Promql(spec) => {
                    let mut other = spec.clone();
                    let from = match (&mut other.outer, &mut other.range) {
                        (
                            Some(OuterAggregation {
                                op: OuterOp::Quantile(q),
                                ..
                            }),
                            _,
                        )
                        | (_, Some(RangeFunction::QuantileOverTime(q))) => {
                            let from = *q;
                            *q = other_rank(from);
                            from
                        }
                        _ => return None,
                    };
                    (QuerySpec::Promql(other), from)
                }
                QuerySpec::Sql(spec) => {
                    let SqlAggregate::Quantile(from) = spec.aggregate else {
                        return None;
                    };
                    let mut other = spec.clone();
                    other.aggregate = SqlAggregate::Quantile(other_rank(from));
                    (QuerySpec::Sql(other), from)
                }
            };
            derived(
                q.with_spec(spec),
                vec![Witness::QuantileRank {
                    from,
                    to: other_rank(from),
                }],
            )
        }
        Relation::OtherStatistic => {
            // One UnivMon answers a column's distinct count, frequency L2
            // and entropy and the row count (planner-layering Example 2);
            // q' asks for another of them than q.
            let QuerySpec::Sql(spec) = q.spec.as_ref()? else {
                return None;
            };
            if !spec.group_by.is_empty() {
                return None;
            }
            let (aggregate, statistic) = match &spec.aggregate {
                SqlAggregate::CountDistinct(_) => (
                    SqlAggregate::Count,
                    SketchStatistic::PointCount {
                        key: ColumnRef::SampleValue,
                        value: None,
                    },
                ),
                SqlAggregate::FrequencyL2(c) => (
                    SqlAggregate::CountDistinct(c.clone()),
                    SketchStatistic::Cardinality,
                ),
                SqlAggregate::FrequencyEntropy(c) => (
                    SqlAggregate::FrequencyL2(c.clone()),
                    SketchStatistic::FrequencyL2,
                ),
                _ => return None,
            };
            let mut other = spec.clone();
            other.aggregate = aggregate;
            derived(
                q.with_spec(QuerySpec::Sql(other)),
                vec![Witness::OtherStatistic { statistic }, Witness::UnivMonOnly],
            )
        }
        Relation::OuterOverResult => match q.spec.as_ref()? {
            QuerySpec::Promql(PromqlSpec {
                outer: Some(outer), ..
            }) if !matches!(outer.op, OuterOp::Topk(_)) && !outer.labels.is_empty() => {
                // The operator follows q's own, so a shrunk q keeps it.
                let text = match outer.op {
                    OuterOp::Count => format!("bottomk(3, {})", q.text()),
                    OuterOp::Max => format!("max({})", q.text()),
                    OuterOp::Avg => format!("quantile(0.5, {})", q.text()),
                    OuterOp::Min | OuterOp::Quantile(_) => format!("{} > 0", q.text()),
                    _ => format!("topk(3, {})", q.text()),
                };
                let mut query = q.clone();
                query.entry.query.0 = text.clone();
                query.spec = None;
                derived(query, vec![Witness::Graft { text }])
            }
            _ => None,
        },
        Relation::SmallerTopK => match q.spec.as_ref()? {
            QuerySpec::Promql(spec) => {
                let outer = spec.outer.as_ref()?;
                let OuterOp::Topk(k) = outer.op else {
                    return None;
                };
                if k <= 3 {
                    return None;
                }
                let mut smaller = spec.clone();
                smaller.outer.as_mut().expect("matched").op = OuterOp::Topk(3);
                derived(
                    q.with_spec(QuerySpec::Promql(smaller)),
                    vec![Witness::SmallerTopK {
                        from: k as usize,
                        to: 3,
                    }],
                )
            }
            QuerySpec::Sql(_) => None,
        },
        Relation::UnionOfSelections => {
            // q selects label = v1 by a filter: add the same query on v2
            // to W, and derive the query over v1 or v2.
            let (other_spec, union_text) = match q.spec.as_ref()? {
                QuerySpec::Promql(spec) => {
                    let (label, value) = spec.matchers.first()?.clone();
                    let other_value = alternate_value(&value);
                    let mut other = spec.clone();
                    other.matchers[0].1 = other_value.clone();
                    let union_text = spec.render().replacen(
                        &format!("{label}=\"{value}\""),
                        &format!("{label}=~\"{value}|{other_value}\""),
                        1,
                    );
                    (QuerySpec::Promql(other), union_text)
                }
                QuerySpec::Sql(spec) => {
                    let (column, value) = spec.filters.first()?.clone();
                    let other_value = alternate_value(&value);
                    let mut other = spec.clone();
                    other.filters[0].1 = other_value.clone();
                    let union_text = spec.render().replacen(
                        &format!("{column} = '{value}'"),
                        &format!("{column} IN ('{value}', '{other_value}')"),
                        1,
                    );
                    (QuerySpec::Sql(other), union_text)
                }
            };
            let mut union = q.clone();
            union.entry.query.0 = union_text;
            union.spec = None;
            Some(Derived {
                query: union,
                witnesses: vec![Witness::UnionWithLast],
                base_addition: Some(q.with_spec(other_spec)),
            })
        }
        Relation::BinaryOverResult => {
            let QuerySpec::Promql(spec) = q.spec.as_ref()? else {
                return None;
            };
            // Vector matching pairs q's series one to one with the
            // numerator's: no top-k or quantile that picks series.
            if spec.range.is_none()
                || spec.outer.as_ref().is_some_and(|o| {
                    o.without || matches!(o.op, OuterOp::Topk(_) | OuterOp::Quantile(_))
                })
            {
                return None;
            }
            let mut numerator = spec.clone();
            if spec.range == Some(RangeFunction::SumOverTime)
                && spec
                    .outer
                    .as_ref()
                    .is_none_or(|o| matches!(o.op, OuterOp::Sum))
            {
                // avg as sum / count, with q the sum.
                let mut count = spec.clone();
                count.range = Some(RangeFunction::CountOverTime);
                let text = format!("{} / {}", spec.render(), count.render());
                let mut query = q.clone();
                query.entry.query.0 = text.clone();
                query.spec = None;
                return derived(
                    query,
                    vec![
                        Witness::GraftWithOperand {
                            text: text.clone(),
                            operand: count.render(),
                        },
                        Witness::Graft { text },
                    ],
                );
            }
            // A filtered share of q, on a label q does not group by.
            let grouped = spec.group_labels();
            let (label, _) = super::generate::PROMQL_LABELS.iter().find(|(l, _)| {
                (spec.outer.is_none() || !grouped.iter().any(|g| g == l))
                    && spec.matchers.iter().all(|(m, _)| m != l)
            })?;
            numerator
                .matchers
                .push((label.to_string(), label_value(label, 0)));
            let text = format!("{} / {}", numerator.render(), spec.render());
            let mut query = q.clone();
            query.entry.query.0 = text.clone();
            query.spec = None;
            derived(
                query,
                vec![
                    Witness::GraftWithOperand {
                        text: text.clone(),
                        operand: numerator.render(),
                    },
                    Witness::Graft { text },
                ],
            )
        }
        Relation::LongerWindow | Relation::LongerWindowRollup => {
            let (longer, window_ms) = match q.spec.as_ref()? {
                QuerySpec::Promql(spec) => {
                    // As for (d): rate and increase read more than merged
                    // pane state.
                    if matches!(spec.range?, RangeFunction::Rate | RangeFunction::Increase) {
                        return None;
                    }
                    let mut longer = spec.clone();
                    longer.window_ms = longer_window(spec.window_ms)?;
                    (QuerySpec::Promql(longer.clone()), longer.window_ms)
                }
                QuerySpec::Sql(spec) => {
                    let mut longer = spec.clone();
                    longer.window_ms = longer_window(spec.window_ms)?;
                    (QuerySpec::Sql(longer.clone()), longer.window_ms)
                }
            };
            if relation == Relation::LongerWindow {
                return derived(
                    q.with_spec(longer),
                    vec![Witness::LongerWindow {
                        window_ms,
                        rollup: None,
                    }],
                );
            }
            let (coarse, rollups) = coarser(&longer)?;
            derived(
                q.with_spec(coarse),
                rollups
                    .into_iter()
                    .map(|rollup| Witness::LongerWindow {
                        window_ms,
                        rollup: Some(Box::new(rollup)),
                    })
                    .collect(),
            )
        }
        Relation::EquivalentRangeFunction => {
            let QuerySpec::Promql(spec) = q.spec.as_ref()? else {
                return None;
            };
            // A count of series does not scale with its values.
            if spec
                .outer
                .as_ref()
                .is_some_and(|o| matches!(o.op, OuterOp::Count))
            {
                return None;
            }
            let seconds = spec.window_ms / 1_000;
            let mut swapped = spec.clone();
            let text = match spec.range? {
                RangeFunction::Rate => {
                    swapped.range = Some(RangeFunction::Increase);
                    format!("({}) * {seconds}", q.text())
                }
                RangeFunction::Increase => {
                    swapped.range = Some(RangeFunction::Rate);
                    format!("({}) / {seconds}", q.text())
                }
                _ => return None,
            };
            derived(
                q.with_spec(QuerySpec::Promql(swapped)),
                vec![Witness::Graft { text }],
            )
        }
        _ => None,
    }
}

/// The shortest longer window, from the generator's windows up to an hour,
/// that is a multiple of `window_ms`.
fn longer_window(window_ms: u64) -> Option<u64> {
    [120_000, 300_000, 600_000, 1_800_000, 3_600_000]
        .into_iter()
        .find(|&w| w > window_ms && w.is_multiple_of(window_ms))
}

/// `spec` grouped by one key fewer, with the rollups that derive it from
/// `spec`'s result or state, or `None` with fewer than two keys or a top-k.
fn coarser(spec: &QuerySpec) -> Option<(QuerySpec, Vec<Witness>)> {
    match spec {
        QuerySpec::Promql(spec) => {
            let outer = spec.outer.as_ref()?;
            let fine = spec.group_labels();
            if fine.len() < 2 || matches!(outer.op, OuterOp::Topk(_)) {
                return None;
            }
            let coarse_labels = fine[..fine.len() - 1].to_vec();
            let mut coarse = spec.clone();
            coarse.outer = Some(OuterAggregation {
                op: outer.op,
                labels: if outer.without {
                    let mut without = outer.labels.clone();
                    without.push(fine[fine.len() - 1].clone());
                    without
                } else {
                    coarse_labels.clone()
                },
                without: outer.without,
            });
            let rollup_op = match outer.op {
                OuterOp::Sum | OuterOp::Count => Some(RollupOp::Sum),
                OuterOp::Max => Some(RollupOp::Max),
                OuterOp::Min => Some(RollupOp::Min),
                _ => None,
            };
            let mut witnesses = vec![Witness::SketchRollup {
                labels: coarse_labels.clone(),
            }];
            if let Some(op) = rollup_op {
                witnesses.insert(
                    0,
                    Witness::ExactRollup {
                        labels: coarse_labels.clone(),
                        op,
                    },
                );
            }
            Some((QuerySpec::Promql(coarse), witnesses))
        }
        QuerySpec::Sql(spec) => {
            if spec.group_by.len() < 2 {
                return None;
            }
            let coarse_columns = spec.group_by[..spec.group_by.len() - 1].to_vec();
            let mut coarse = spec.clone();
            coarse.group_by = coarse_columns.clone();
            let rollup_op = match spec.aggregate {
                SqlAggregate::Sum | SqlAggregate::Count => Some(RollupOp::Sum),
                SqlAggregate::Max => Some(RollupOp::Max),
                SqlAggregate::Min => Some(RollupOp::Min),
                _ => None,
            };
            let mut witnesses = vec![Witness::SketchRollup {
                labels: coarse_columns.clone(),
            }];
            if let Some(op) = rollup_op {
                witnesses.insert(
                    0,
                    Witness::ExactRollup {
                        labels: coarse_columns.clone(),
                        op,
                    },
                );
            }
            Some((QuerySpec::Sql(coarse), witnesses))
        }
    }
}

/// `<label><n>` with another `n`.
fn alternate_value(value: &str) -> String {
    let digits = value.trim_start_matches(|c: char| !c.is_ascii_digit());
    let prefix = &value[..value.len() - digits.len()];
    match digits.parse::<u64>() {
        Ok(1) => format!("{prefix}2"),
        _ => format!("{prefix}1"),
    }
}

/// Rewrites `node`, visiting it depth first from the root (so the first
/// match is the first on the leftmost path, not necessarily the shallowest):
/// a node `replace` maps is replaced (its inputs are not visited), and
/// every node above a replaced one is rebuilt over its new inputs with its
/// schema derived again. Unchanged sub-DAGs keep their `Rc`, so sharing is
/// kept.
pub fn rewrite(
    node: &Rc<OperatorNode>,
    replace: &mut dyn FnMut(&Rc<OperatorNode>) -> Option<Rc<OperatorNode>>,
    cache: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
) -> Result<Rc<OperatorNode>, String> {
    if let Some(done) = cache.get(&Rc::as_ptr(node)) {
        return Ok(done.clone());
    }
    let rebuilt = match replace(node) {
        Some(replacement) => replacement,
        None => {
            let mut changed = false;
            for child in node.operator.children() {
                let new = rewrite(child, replace, cache)?;
                changed |= !Rc::ptr_eq(&new, child);
            }
            if changed {
                Rc::new(
                    node.with_new_children(|child| cache[&Rc::as_ptr(child)].clone())
                        .map_err(|e| format!("rebuilding {}: {e}", node.operator.kind_name()))?,
                )
            } else {
                node.clone()
            }
        }
    };
    cache.insert(Rc::as_ptr(node), rebuilt.clone());
    Ok(rebuilt)
}

/// `root` with the nodes `replace` maps replaced, or an error when it maps
/// none.
fn rewrite_root(
    root: &Rc<OperatorNode>,
    what: &str,
    mut replace: impl FnMut(&Rc<OperatorNode>) -> Option<Rc<OperatorNode>>,
) -> Result<Rc<OperatorNode>, String> {
    let mut replaced = false;
    let new = rewrite(
        root,
        &mut |node| {
            let out = replace(node);
            replaced |= out.is_some();
            out
        },
        &mut HashMap::new(),
    )?;
    if replaced {
        Ok(new)
    } else {
        Err(format!("q's plan has no {what}"))
    }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

fn with_operator(node: &OperatorNode, operator: Operator) -> Rc<OperatorNode> {
    Rc::new(OperatorNode {
        operator,
        ..node.clone()
    })
}

/// `q`'s estimate when it is `q`'s root or the only input of a root
/// `Project` of plain column references: the shapes in which another
/// estimate can stand in for it without changing what the nodes above
/// compute. Returns its state and statistic.
fn root_estimate(root: &Rc<OperatorNode>) -> Result<(Rc<OperatorNode>, SketchStatistic), String> {
    let estimate = match root.non_asap() {
        Some(NonASAPOp::Project { cols, child, .. })
            if cols
                .iter()
                .all(|item| matches!(item.expr, ScalarExpr::Column(_))) =>
        {
            child
        }
        _ => root,
    };
    match estimate.asap() {
        Some(ASAPOp::SummaryEstimate {
            summary_input,
            query,
        }) => Ok((summary_input.clone(), query.clone())),
        _ => Err(
            "q's result is not a summary estimate (at its root or under a column projection)"
                .into(),
        ),
    }
}

/// `root` with its root estimate ([`root_estimate`]) replaced by
/// `estimate`; a root `Project` keeps its columns by name, except the
/// `dropped` group labels (a rollup's), which it loses. Any other column
/// `estimate` lacks is an error.
fn with_root_estimate(
    root: &Rc<OperatorNode>,
    estimate: Rc<OperatorNode>,
    dropped: &[String],
) -> Result<Rc<OperatorNode>, String> {
    let Some(NonASAPOp::Project {
        cols,
        qualifier,
        child,
    }) = root.non_asap()
    else {
        return Ok(estimate);
    };
    let mut kept = Vec::new();
    for item in cols {
        let ScalarExpr::Column(index) = item.expr else {
            return Err("q's root projection is not of plain columns".into());
        };
        let name = &child.schema.fields[index].name;
        match estimate.schema.fields.iter().position(|f| f.name == *name) {
            Some(new) => kept.push(ProjectItem {
                alias: item.alias.clone(),
                expr: ScalarExpr::Column(new),
            }),
            None if dropped.contains(name) => {}
            None => return Err(format!("the new estimate has no column {name}")),
        }
    }
    let cols = kept;
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
        cols,
        qualifier: qualifier.clone(),
        child: estimate,
    }))
    .map_err(|e| format!("projection over the new estimate: {e}"))
}

/// The family of the build whose state `state` carries.
fn state_family(state: &OperatorNode) -> Result<FieldDataType, String> {
    match state.summary_build() {
        Some(ASAPOp::SummaryAgg { family, .. } | ASAPOp::PaneChain { family, .. }) => {
            Ok(family.clone())
        }
        _ => Err("q's estimate reads no single summary build".into()),
    }
}

/// The witness root answering `q'` from `q`'s root `q_root` (query
/// `target` of `base`, whose selected logical roots are `base_roots`).
pub fn witness_root(
    planner: &Planner,
    witness: &Witness,
    base: &FuzzWorkload,
    planned: &Planned,
    base_roots: &[QueryRoot],
    target: usize,
) -> Result<Rc<OperatorNode>, String> {
    let q_root = base_roots[target]
        .as_operator()
        .ok_or("q is a scalar query")?;
    let semantics = match base.language {
        Language::Promql => ExprSemantics::Promql,
        Language::Sql => ExprSemantics::Sql,
    };
    match witness {
        Witness::Same => Ok(q_root.clone()),
        Witness::Filter { column, value } => {
            let index = q_root
                .schema
                .fields
                .iter()
                .position(|f| f.name == *column)
                .ok_or_else(|| format!("q's output has no column {column}"))?;
            let pred = Predicate(ScalarExpr::Compare {
                left: Box::new(ScalarExpr::Column(index)),
                op: CompareOpKind::Eq,
                right: Box::new(ScalarExpr::Literal(ScalarValue::Utf8(value.clone()))),
                semantics,
            });
            OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Filter {
                pred,
                child: q_root.clone(),
            }))
            .map_err(|e| format!("filter: {e}"))
        }
        Witness::Graft { text } => {
            let lowered_q = planned.stage0_roots[target]
                .as_operator()
                .ok_or("q is a scalar query")?;
            graft(planner, base, target, text, &[(lowered_q, q_root)])
        }
        Witness::ExactRollup { labels, op } => {
            let fields = &q_root.schema.fields;
            let keys = labels
                .iter()
                .map(|label| {
                    fields
                        .iter()
                        .position(|f| f.name == *label)
                        .ok_or_else(|| format!("q's output has no column {label}"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            // The value column: q's one numeric column (group keys are
            // strings).
            let numeric: Vec<usize> = fields
                .iter()
                .enumerate()
                .filter(|(_, f)| {
                    matches!(
                        f.dtype,
                        FieldDataType::Plain(DataType::Float64 | DataType::Int64)
                    )
                })
                .map(|(i, _)| i)
                .collect();
            let [value] = numeric[..] else {
                return Err(format!(
                    "q's output has {} float columns, not one",
                    numeric.len()
                ));
            };
            let measure = match op {
                RollupOp::Sum => AggIntent::Sum { col: Some(value) },
                RollupOp::Max => AggIntent::Max { col: Some(value) },
                RollupOp::Min => AggIntent::Min { col: Some(value) },
            };
            OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Aggregate {
                reduction: Reduction::by(keys),
                measures: vec![measure],
                output_names: vec![fields[value].name.clone()],
                filters: vec![],
                having: None,
                child: q_root.clone(),
            }))
            .map_err(|e| format!("exact rollup: {e}"))
        }
        Witness::SmallerTopK { from, to } => {
            let mut done = false;
            rewrite_root(q_root, "top-k", |node| {
                if done {
                    return None;
                }
                let mut operator = node.operator.clone();
                let changed = match &mut operator {
                    Operator::NonASAP(NonASAPOp::Limit { n: Some(n), .. }) if *n == *from => {
                        *n = *to;
                        true
                    }
                    Operator::NonASAP(NonASAPOp::Aggregate { measures, .. }) => {
                        let mut any = false;
                        for measure in measures.iter_mut() {
                            if let AggIntent::TopK { k, .. } = measure {
                                if *k == *from {
                                    *k = *to;
                                    any = true;
                                }
                            }
                        }
                        any
                    }
                    _ => false,
                };
                done |= changed;
                changed.then(|| with_operator(node, operator))
            })
        }
        Witness::SketchRollup { labels } => {
            let (state, query) = root_estimate(q_root)?;
            let family = state_family(&state)?;
            if !FamilyCapability::merges_groups(&family) {
                return Err(format!("q's family {family:?} does not merge groups"));
            }
            let group_by = labels
                .iter()
                .map(|label| {
                    state
                        .schema
                        .fields
                        .iter()
                        .position(|f| f.name == *label)
                        .ok_or_else(|| format!("q's state has no column {label}"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            // The finer group labels the rollup drops.
            let dropped: Vec<String> = state
                .schema
                .fields
                .iter()
                .map(|f| f.name.clone())
                .filter(|name| !labels.contains(name))
                .collect();
            let merge = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
                children: vec![state],
                newest_panes: None,
                group_by: Some(group_by),
            }))
            .map_err(|e| format!("rollup merge: {e}"))?;
            let estimate = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input: merge,
                query,
            }))
            .map_err(|e| format!("rollup estimate: {e}"))?;
            with_root_estimate(q_root, estimate, &dropped)
        }
        Witness::NewestPanes { window_ms } => {
            rewrite_root(q_root, "summary merge over a pane chain", |node| {
                let Some(ASAPOp::SummaryMerge {
                    children,
                    newest_panes: None,
                    group_by,
                }) = node.asap()
                else {
                    return None;
                };
                let [chain] = children.as_slice() else {
                    return None;
                };
                let Some(ASAPOp::PaneChain {
                    pane_width_ms,
                    pane_count,
                    ..
                }) = chain.asap()
                else {
                    return None;
                };
                let panes = window_ms / pane_width_ms;
                (window_ms % pane_width_ms == 0 && panes >= 1 && (panes as u32) < *pane_count).then(
                    || {
                        with_operator(
                            node,
                            Operator::ASAP(ASAPOp::SummaryMerge {
                                children: children.clone(),
                                newest_panes: Some(panes as u32),
                                group_by: group_by.clone(),
                            }),
                        )
                    },
                )
            })
        }
        Witness::QuantileRank { from, to } => {
            let mut done = false;
            rewrite_root(q_root, "quantile estimate", |node| {
                if done {
                    return None;
                }
                let mut operator = node.operator.clone();
                let changed = match &mut operator {
                    Operator::ASAP(ASAPOp::SummaryEstimate {
                        query: SketchStatistic::Quantile { q },
                        ..
                    }) if close(*q, *from) => {
                        *q = *to;
                        true
                    }
                    Operator::NonASAP(NonASAPOp::Aggregate { measures, .. }) => {
                        let mut any = false;
                        for measure in measures.iter_mut() {
                            if let AggIntent::Quantile { q, .. } = measure {
                                if close(*q, *from) {
                                    *q = *to;
                                    any = true;
                                }
                            }
                        }
                        any
                    }
                    _ => false,
                };
                done |= changed;
                changed.then(|| with_operator(node, operator))
            })
        }
        Witness::OtherStatistic { statistic } => {
            let answers = |state: &OperatorNode| -> Result<(), String> {
                let family = state_family(state)?;
                if FamilyCapability::of(&family).answers(statistic) {
                    Ok(())
                } else {
                    Err(format!(
                        "q's family {family:?} does not answer {statistic:?}"
                    ))
                }
            };
            if let Ok((state, _)) = root_estimate(q_root) {
                answers(&state)?;
                let estimate = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryEstimate {
                    summary_input: state,
                    query: statistic.clone(),
                }))
                .map_err(|e| format!("estimate: {e}"))?;
                return with_root_estimate(q_root, estimate, &[]);
            }
            // SQL's frequency idioms join the estimate with an exact row
            // count, for their NULL on empty input: swap the statistic in
            // place under q's root projection. That projection keeps q's
            // value expression (the entropy idiom's sign and log base, a
            // Float64 result), so the witness answers q' up to that scalar
            // step, which costs the same.
            let mut done = false;
            rewrite_root(q_root, "summary estimate", |node| {
                let Some(ASAPOp::SummaryEstimate { summary_input, .. }) = node.asap() else {
                    return None;
                };
                if done || answers(summary_input).is_err() {
                    return None;
                }
                done = true;
                OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryEstimate {
                    summary_input: summary_input.clone(),
                    query: statistic.clone(),
                }))
                .ok()
            })
        }
        Witness::UnionWithLast => {
            let other_root = base_roots
                .last()
                .and_then(QueryRoot::as_operator)
                .ok_or("no other query")?;
            let (state, query) = root_estimate(q_root)?;
            let (other_state, _) = root_estimate(other_root)?;
            if Rc::ptr_eq(&state, &other_state) {
                return Err("both selections read one state".into());
            }
            let family = state_family(&state)?;
            if FamilyCapability::of(&family).merge_inputs.is_none() {
                return Err(format!("q's family {family:?} has no merge"));
            }
            // The merge's coverage check refuses states of different
            // definitions (family, parameters, input) or overlapping
            // selections.
            let merge = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
                children: vec![state, other_state],
                newest_panes: None,
                group_by: None,
            }))
            .map_err(|e| format!("union merge: {e}"))?;
            let estimate = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input: merge,
                query,
            }))
            .map_err(|e| format!("union estimate: {e}"))?;
            with_root_estimate(q_root, estimate, &[])
        }
        Witness::LongerWindow { .. } | Witness::UnivMonOnly | Witness::GraftWithOperand { .. } => {
            Err("the derivation changes W's roots: see witness_roots".into())
        }
    }
}

/// The frontends' lowering of `text` (in `base`'s language, with query
/// `target`'s requirements) with each lowered subtree of `planned_for`
/// replaced by its planned root.
fn graft(
    planner: &Planner,
    base: &FuzzWorkload,
    target: usize,
    text: &str,
    planned_for: &[(&Rc<OperatorNode>, &Rc<OperatorNode>)],
) -> Result<Rc<OperatorNode>, String> {
    let mut derivation = base.clone();
    derivation.queries = vec![FuzzQuery {
        spec: None,
        entry: {
            let mut entry = base.queries[target].entry.clone();
            entry.query.0 = text.to_string();
            entry
        },
    }];
    let lowered = planner.lower(&derivation)?;
    let lowered = lowered[0]
        .as_operator()
        .ok_or("the derivation is a scalar query")?;
    let names = |node: &OperatorNode| -> Vec<String> {
        node.schema.fields.iter().map(|f| f.name.clone()).collect()
    };
    if planned_for
        .iter()
        .any(|(lowering, planned)| names(lowering) != names(planned))
    {
        return Err("a plan and its lowering have different output columns".into());
    }
    rewrite_root(lowered, "lowering of q inside the derivation", |node| {
        planned_for
            .iter()
            .find(|(lowering, _)| ***lowering == **node)
            .map(|(_, planned)| (*planned).clone())
    })
}

/// The witness's roots: `W`'s selected logical roots plus `q'`'s root,
/// with `W`'s roots rewritten when the derivation changes a node they
/// share ([`Witness::LongerWindow`]).
pub fn witness_roots(
    planner: &mut Planner,
    witness: &Witness,
    base: &FuzzWorkload,
    related: &FuzzWorkload,
    planned: &Planned,
    base_roots: &[QueryRoot],
    target: usize,
) -> Result<Vec<QueryRoot>, String> {
    if let Witness::GraftWithOperand { text, operand } = witness {
        let mut sibling = base.queries[target].clone();
        sibling.entry.query.0 = operand.clone();
        sibling.spec = None;
        let with_operand = base.with_query(sibling);
        let planned = planner
            .plan(&with_operand)
            .map_err(|e| format!("W with q's sibling operand does not plan: {e}"))?;
        let roots = planned
            .logical_roots
            .clone()
            .ok_or("the search keeps no logical plan for W with q's sibling operand")?;
        let n = base.queries.len();
        let operator = |roots: &[QueryRoot], i: usize| -> Result<Rc<OperatorNode>, String> {
            roots[i]
                .as_operator()
                .cloned()
                .ok_or_else(|| "a scalar operand".to_string())
        };
        let (lowered_q, lowered_operand) = (
            operator(&planned.stage0_roots, target)?,
            operator(&planned.stage0_roots, n)?,
        );
        let (q_root, operand_root) = (operator(&roots, target)?, operator(&roots, n)?);
        let root = graft(
            planner,
            base,
            target,
            text,
            &[(&lowered_q, &q_root), (&lowered_operand, &operand_root)],
        )?;
        let mut roots = roots[..n].to_vec();
        roots.push(QueryRoot::Operator(root));
        return Ok(roots);
    }
    if let Witness::UnivMonOnly = witness {
        let univmon_only = FuzzWorkload {
            sketch_families: Some(vec![SketchAlgorithm::UnivMon]),
            ..related.clone()
        };
        return planner
            .plan(&univmon_only)
            .map_err(|e| format!("W ∪ {{q'}} with UnivMon only does not plan: {e}"))?
            .logical_roots
            .clone()
            .ok_or_else(|| "the search keeps no logical plan with UnivMon only".into());
    }
    let Witness::LongerWindow { window_ms, rollup } = witness else {
        let root = witness_root(planner, witness, base, planned, base_roots, target)?;
        let mut roots = base_roots.to_vec();
        roots.push(QueryRoot::Operator(root));
        return Ok(roots);
    };
    let (mut roots, long_root) = longer_chain_roots(base_roots, target, *window_ms)?;
    let root = match rollup {
        None => long_root,
        Some(rollup) => {
            let mut with_long = roots.clone();
            with_long[target] = QueryRoot::Operator(long_root);
            witness_root(planner, rollup, base, planned, &with_long, target)?
        }
    };
    roots.push(QueryRoot::Operator(root));
    Ok(roots)
}

/// The first node under `node` (depth first, `node` included) that `pred`
/// accepts.
fn find_first(
    node: &Rc<OperatorNode>,
    pred: &dyn Fn(&OperatorNode) -> bool,
) -> Option<Rc<OperatorNode>> {
    if pred(node) {
        return Some(node.clone());
    }
    node.operator
        .children()
        .into_iter()
        .find_map(|child| find_first(child, pred))
}

/// The distinct nodes under `node` (`node` included) that `pred` accepts.
fn collect(
    node: &Rc<OperatorNode>,
    pred: &dyn Fn(&OperatorNode) -> bool,
    found: &mut Vec<Rc<OperatorNode>>,
) {
    if found.iter().any(|f| Rc::ptr_eq(f, node)) {
        return;
    }
    if pred(node) {
        found.push(node.clone());
    }
    for child in node.operator.children() {
        collect(child, pred, found);
    }
}

/// `W`'s roots with the pane chain `q` merges whole lengthened to cover
/// `window_ms`, every merge of it reading its old newest panes of the
/// longer chain; and `q`'s root merging the whole longer chain.
fn longer_chain_roots(
    base_roots: &[QueryRoot],
    target: usize,
    window_ms: u64,
) -> Result<(Vec<QueryRoot>, Rc<OperatorNode>), String> {
    let q_root = base_roots[target]
        .as_operator()
        .ok_or("q is a scalar query")?;
    let whole_chain_merge = |node: &OperatorNode| {
        matches!(
            node.asap(),
            Some(ASAPOp::SummaryMerge { children, newest_panes: None, .. })
                if matches!(children.as_slice(), [chain] if matches!(chain.asap(), Some(ASAPOp::PaneChain { .. })))
        )
    };
    let merge = find_first(q_root, &whole_chain_merge)
        .ok_or("q's plan has no summary merge over a pane chain")?;
    // Lengthening one of several chains (say, an SQL idiom's exact row
    // count beside its statistic) would leave the others at q's window.
    let mut merges = Vec::new();
    collect(q_root, &whole_chain_merge, &mut merges);
    if merges.len() > 1 {
        return Err(format!(
            "q's plan merges {} pane chains whole",
            merges.len()
        ));
    }
    let Some(ASAPOp::SummaryMerge { children, .. }) = merge.asap() else {
        unreachable!("found a merge");
    };
    let chain = children[0].clone();
    let mut operator = chain.operator.clone();
    let Operator::ASAP(ASAPOp::PaneChain {
        pane_width_ms,
        pane_count,
        ..
    }) = &mut operator
    else {
        unreachable!("found a pane chain");
    };
    let old_count = *pane_count;
    if !window_ms.is_multiple_of(*pane_width_ms) || window_ms / *pane_width_ms <= old_count as u64 {
        return Err(format!(
            "q's chain of {old_count} panes of {pane_width_ms} ms does not extend to {window_ms} ms"
        ));
    }
    *pane_count = (window_ms / *pane_width_ms) as u32;
    let long_chain = with_operator(&chain, operator);
    let reread = |node: &Rc<OperatorNode>, whole: bool| {
        let Some(ASAPOp::SummaryMerge {
            children,
            newest_panes,
            group_by,
        }) = node.asap()
        else {
            return None;
        };
        let [child] = children.as_slice() else {
            return None;
        };
        Rc::ptr_eq(child, &chain).then(|| {
            with_operator(
                node,
                Operator::ASAP(ASAPOp::SummaryMerge {
                    children: vec![long_chain.clone()],
                    newest_panes: if whole {
                        None
                    } else {
                        Some(newest_panes.unwrap_or(old_count))
                    },
                    group_by: group_by.clone(),
                }),
            )
        })
    };
    let mut cache = HashMap::new();
    let roots = base_roots
        .iter()
        .map(|root| match root.as_operator() {
            Some(node) => {
                rewrite(node, &mut |n| reread(n, false), &mut cache).map(QueryRoot::Operator)
            }
            None => Ok(root.clone()),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let long_root = rewrite(q_root, &mut |n| reread(n, true), &mut HashMap::new())?;
    Ok((roots, long_root))
}

fn planned(
    planner: &mut Planner,
    workload: &FuzzWorkload,
    side: &str,
) -> Result<Rc<Planned>, String> {
    planner
        .plan(workload)
        .or_else(|e| inconclusive(format!("{side} does not plan: {e}")))
}

/// The two workloads `check` compares: `W` (for
/// [`Relation::UnionOfSelections`], with the other selection added) and the
/// related one (`W ∪ {q'}` or `W'`), with the derivation of a marginal
/// relation. `Err` is why the relation does not apply.
pub fn related_workloads(
    check: &Check,
) -> Result<(FuzzWorkload, FuzzWorkload, Option<Derived>), String> {
    let w = &check.workload;
    if check.relation.is_marginal() {
        let Some(derived) = derive(check.relation, w, check.target) else {
            return inconclusive("not applicable to q");
        };
        let base = match &derived.base_addition {
            Some(addition) => w.with_query(addition.clone()),
            None => w.clone(),
        };
        let related = base.with_query(derived.query.clone());
        return Ok((base, related, Some(derived)));
    }
    let q = w.queries.get(check.target).ok_or("no query")?;
    let related = match check.relation {
        Relation::TighterAccuracy => {
            let Some(tight) = tighter(q.accuracy()) else {
                return inconclusive("q is exact");
            };
            w.with_query_replaced(check.target, q.with_accuracy(tight))
        }
        Relation::RelaxedLatency => {
            if q.entry.requirements.max_latency_ms.is_none() {
                return inconclusive("q has no latency bound");
            }
            let mut relaxed = q.clone();
            relaxed.entry.requirements.max_latency_ms = None;
            w.with_query_replaced(check.target, relaxed)
        }
        Relation::AddedQuery => match &check.added {
            Some(added) => w.with_query(added.clone()),
            None => return inconclusive("no added query"),
        },
        Relation::NoRawRetention => {
            if !w.raw_data_retained {
                return inconclusive("raw data is not retained");
            }
            FuzzWorkload {
                raw_data_retained: false,
                ..w.clone()
            }
        }
        Relation::ResultsRetained => {
            if w.query_time_results_retained {
                return inconclusive("query-time results are already retained");
            }
            FuzzWorkload {
                query_time_results_retained: true,
                ..w.clone()
            }
        }
        Relation::Renamed => renamed(w, "_r", true).ok_or("a query has no structured form")?,
        Relation::Reordered => {
            if w.queries.len() < 2 {
                return inconclusive("one query");
            }
            let mut reordered = w.clone();
            reordered.queries.reverse();
            reordered
        }
        Relation::DisjointCopy => disjoint_copy(w).ok_or("a query has no structured form")?,
        _ => unreachable!("marginal relations are handled above"),
    };
    Ok((w.clone(), related, None))
}

/// The other side's logical plan priced for `workload` (its first
/// `keep` roots): a plan for `workload` the search could have selected.
fn confirming(
    planner: &Planner,
    workload: &FuzzWorkload,
    roots: Option<&Vec<QueryRoot>>,
    keep: usize,
) -> Option<WitnessReport> {
    let priced = planner.price(workload, &roots?[..keep], false).ok()?;
    Some(WitnessReport {
        derivation: None,
        cost_per_second: priced.cost_per_second,
        plan: priced.selected,
        base_reference_cost_per_second: None,
        derive_cost_per_second: None,
        derived_node_cost_per_second: None,
        derived_operators: Vec::new(),
        key_filter_keeps_input_guarantee: false,
    })
}

/// Evaluates `check`.
pub fn evaluate(planner: &mut Planner, check: &Check) -> Outcome {
    evaluate_inner(planner, check).unwrap_or_else(|reason| Outcome::Inconclusive { reason })
}

fn evaluate_inner(planner: &mut Planner, check: &Check) -> Result<Outcome, String> {
    let (w, related, derived) = related_workloads(check)?;
    let base = planned(planner, &w, "W")?;
    let other = planned(planner, &related, "the related workload")?;
    if let Some(derived) = derived {
        return evaluate_marginal(planner, check, &w, &related, &derived, &base, &other);
    }
    let (b, r) = (base.cost_per_second, other.cost_per_second);
    let n = w.queries.len();
    let (bound, tolerance, violated, witness) = match check.relation {
        Relation::TighterAccuracy | Relation::AddedQuery | Relation::NoRawRetention => {
            let tolerance = planner.settings.tolerance(b);
            let violated = r < b - tolerance;
            // The related plan, minus an added query, is a plan for W.
            let witness = violated
                .then(|| confirming(planner, &w, other.logical_roots.as_ref(), n))
                .flatten();
            (b - tolerance, tolerance, violated, witness)
        }
        Relation::ResultsRetained | Relation::RelaxedLatency => {
            let tolerance = planner.settings.tolerance(b);
            let violated = r > b + tolerance;
            let witness = violated
                .then(|| confirming(planner, &related, base.logical_roots.as_ref(), n))
                .flatten();
            (b + tolerance, tolerance, violated, witness)
        }
        Relation::DisjointCopy => {
            let tolerance = planner.settings.tolerance(2.0 * b);
            (2.0 * b, tolerance, (r - 2.0 * b).abs() > tolerance, None)
        }
        _ => {
            let tolerance = planner.settings.tolerance(b.max(r));
            (b, tolerance, (r - b).abs() > tolerance, None)
        }
    };
    let summary = format!(
        "{}: cost(W) = {b:.6}, cost(W') = {r:.6}, checked against {bound:.6} (tolerance {tolerance:.6})",
        check.relation.statement()
    );
    let measurement = Measurement {
        base_cost_per_second: b,
        base_plan: base.selected.clone(),
        related_cost_per_second: r,
        related_plan: other.selected.clone(),
        related_query: check
            .added
            .as_ref()
            .filter(|_| check.relation == Relation::AddedQuery)
            .map(|q| q.text().to_string()),
        witness,
        bound_per_second: bound,
        tolerance_per_second: tolerance,
        summary,
    };
    Ok(if violated {
        Outcome::Violated(measurement)
    } else {
        Outcome::Holds(measurement)
    })
}

/// The marginal relation: the cheapest witness that builds and prices,
/// against the planner's marginal cost. Violated only when both the
/// relation (`Δ > derive_cost + tolerance`) and its plain reading (the
/// witness is cheaper than the planner's `W ∪ {q'}` by more than the
/// tolerance) fail, so a search that prices `W` below Stage 3's price of
/// the same plan cannot make a false violation.
fn evaluate_marginal(
    planner: &mut Planner,
    check: &Check,
    base_workload: &FuzzWorkload,
    related: &FuzzWorkload,
    derived: &Derived,
    base: &Planned,
    with: &Planned,
) -> Result<Outcome, String> {
    let Some(base_roots) = base.logical_roots.clone() else {
        return inconclusive("the search keeps no logical plan for W");
    };
    let base_reference = planner
        .reference_price(base_workload, &base_roots)
        .or_else(|e| inconclusive(format!("W's plan does not price: {e}")))?;
    let mut best: Option<(Witness, Priced)> = None;
    let mut failures = Vec::new();
    for witness in &derived.witnesses {
        let priced = witness_roots(
            planner,
            witness,
            base_workload,
            related,
            base,
            &base_roots,
            check.target,
        )
        .and_then(|roots| {
            planner.price(related, &roots, matches!(witness, Witness::Filter { .. }))
        });
        match priced {
            Ok(priced)
                if best
                    .as_ref()
                    .is_none_or(|(_, b)| priced.cost_per_second < b.cost_per_second) =>
            {
                best = Some((witness.clone(), priced))
            }
            Ok(_) => {}
            Err(e) => failures.push(e),
        }
    }
    let Some((witness, priced)) = best else {
        return inconclusive(format!("no witness: {}", failures.join("; ")));
    };
    let derive_cost = priced.cost_per_second - base_reference.cost_per_second;
    let delta = with.cost_per_second - base.cost_per_second;
    let tolerance = planner.settings.tolerance(with.cost_per_second);
    let violated = delta > derive_cost + tolerance
        && with.cost_per_second > priced.cost_per_second + tolerance;
    let summary = format!(
        "cost(W ∪ {{q'}}) − cost(W) = {:.6} − {:.6} = {delta:.6}; derive_cost = {:.6} (witness {:.6} − W's plan {:.6}); tolerance {tolerance:.6}",
        with.cost_per_second,
        base.cost_per_second,
        derive_cost,
        priced.cost_per_second,
        base_reference.cost_per_second,
    );
    let key_filter = matches!(witness, Witness::Filter { .. });
    let measurement = Measurement {
        base_cost_per_second: base.cost_per_second,
        base_plan: base.selected.clone(),
        related_cost_per_second: with.cost_per_second,
        related_plan: with.selected.clone(),
        related_query: Some(derived.query.text().to_string()),
        witness: Some(WitnessReport {
            derivation: Some(witness),
            cost_per_second: priced.cost_per_second,
            plan: priced.selected.clone(),
            base_reference_cost_per_second: Some(base_reference.cost_per_second),
            derive_cost_per_second: Some(derive_cost),
            derived_node_cost_per_second: Some(priced.derived_root.cost_per_second),
            derived_operators: priced.derived_root.operators.clone(),
            key_filter_keeps_input_guarantee: key_filter,
        }),
        bound_per_second: (base.cost_per_second + derive_cost).max(priced.cost_per_second)
            + tolerance,
        tolerance_per_second: tolerance,
        summary,
    };
    Ok(if violated {
        Outcome::Violated(measurement)
    } else {
        Outcome::Holds(measurement)
    })
}

/// Renames a JSON object's keys with `rename`.
fn rename_keys(value: &mut Value, rename: &dyn Fn(&str) -> String) {
    if let Some(map) = value.as_object_mut() {
        *map = std::mem::take(map)
            .into_iter()
            .map(|(k, v)| (rename(&k), v))
            .collect();
    }
}

/// `workload` with every source renamed `<name><suffix>` and, when
/// `labels`, every label (column) too; `None` unless every query has a
/// structured form.
pub fn renamed(workload: &FuzzWorkload, suffix: &str, labels: bool) -> Option<FuzzWorkload> {
    if !workload.all_structured() {
        return None;
    }
    let source = |name: &str| format!("{name}{suffix}");
    let label = |name: &str| {
        if labels && name != "ts" && name != "value" {
            format!("{name}{suffix}")
        } else {
            name.to_string()
        }
    };
    let mut renamed = workload.clone();
    for query in &mut renamed.queries {
        let spec = match query.spec.clone()? {
            QuerySpec::Promql(mut spec) => {
                spec.metric = source(&spec.metric);
                for (l, _) in &mut spec.matchers {
                    *l = label(l);
                }
                if let Some(outer) = &mut spec.outer {
                    for l in &mut outer.labels {
                        *l = label(l);
                    }
                }
                QuerySpec::Promql(spec)
            }
            QuerySpec::Sql(mut spec) => {
                spec.table = source(&spec.table);
                for c in &mut spec.group_by {
                    *c = label(c);
                }
                for (c, _) in &mut spec.filters {
                    *c = label(c);
                }
                if let SqlAggregate::CountDistinct(c)
                | SqlAggregate::FrequencyL2(c)
                | SqlAggregate::FrequencyEntropy(c) = &mut spec.aggregate
                {
                    *c = label(c);
                }
                QuerySpec::Sql(spec)
            }
        };
        *query = query.with_spec(spec);
    }
    let mut data = serde_json::to_value(&workload.data).ok()?;
    rename_keys(&mut data["sources"], &source);
    if let Some(sources) = data["sources"].as_object_mut() {
        for facts in sources.values_mut() {
            if let Some(labels) = facts.get_mut("distinct_values_per_label") {
                rename_keys(labels, &label);
            }
            if let Some(sets) = facts
                .get_mut("distinct_values_per_label_set")
                .and_then(Value::as_array_mut)
            {
                for set in sets {
                    if let Some(names) = set.get_mut("labels").and_then(Value::as_array_mut) {
                        for name in names {
                            *name = json!(label(name.as_str().unwrap_or_default()));
                        }
                    }
                }
            }
        }
    }
    renamed.data = serde_json::from_value(data).ok()?;
    for table in &mut renamed.tables {
        table["name"] = json!(source(table["name"].as_str()?));
        for column in table["columns"].as_array_mut()? {
            column["name"] = json!(label(column["name"].as_str()?));
        }
    }
    Some(renamed)
}

/// `workload` planned together with a copy of itself over renamed sources.
pub fn disjoint_copy(workload: &FuzzWorkload) -> Option<FuzzWorkload> {
    let copy = renamed(workload, "_copy", false)?;
    let mut both = workload.clone();
    both.queries.extend(copy.queries);
    both.data.sources.extend(copy.data.sources);
    both.tables.extend(copy.tables);
    both.data.arrival = match workload.data.arrival {
        asap_types::workload::DataArrival::Ingesting { rows_per_second } => {
            asap_types::workload::DataArrival::Ingesting {
                rows_per_second: 2.0 * rows_per_second,
            }
        }
        asap_types::workload::DataArrival::Mixed { rows_per_second } => {
            asap_types::workload::DataArrival::Mixed {
                rows_per_second: 2.0 * rows_per_second,
            }
        }
        other => other,
    };
    Some(both)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner_fuzz::generate::{generated_workload, SqlSpec};
    use asap_types::ir::properties::AccuracyTarget;

    fn promql(spec: PromqlSpec, accuracy: AccuracyTarget) -> FuzzWorkload {
        generated_workload(
            Language::Promql,
            vec![FuzzQuery::generated(
                QuerySpec::Promql(spec),
                accuracy,
                10_000,
                None,
            )],
            1_000,
            15_000,
        )
    }

    fn sum_by(labels: &[&str]) -> PromqlSpec {
        PromqlSpec {
            metric: "m_counter".into(),
            matchers: vec![],
            range: Some(RangeFunction::Rate),
            window_ms: 300_000,
            outer: Some(OuterAggregation {
                op: OuterOp::Sum,
                labels: labels.iter().map(|l| l.to_string()).collect(),
                without: false,
            }),
        }
    }

    /// Each marginal relation derives the documented `q'` from `q`.
    #[test]
    fn marginal_relations_derive_the_documented_query() {
        let w = promql(sum_by(&["job", "instance"]), AccuracyTarget::exact());
        let text = |r| derive(r, &w, 0).map(|d| d.query.text().to_string());
        assert_eq!(
            text(Relation::FilterOnGroupKey).as_deref(),
            Some("sum by (job, instance) (rate(m_counter{job=\"job1\"}[5m]))")
        );
        let coarser = derive(Relation::CoarserGrouping, &w, 0).unwrap();
        assert_eq!(coarser.query.text(), "sum by (job) (rate(m_counter[5m]))");
        assert_eq!(
            coarser.witnesses[0],
            Witness::ExactRollup {
                labels: vec!["job".into()],
                op: RollupOp::Sum
            }
        );
        // A counter's rate reads more than merged panes: no nested window.
        assert_eq!(text(Relation::NestedShorterWindow), None);
        assert_eq!(text(Relation::OtherQuantileRank), None);
        assert_eq!(
            text(Relation::OuterOverResult).as_deref(),
            Some("topk(3, sum by (job, instance) (rate(m_counter[5m])))")
        );
        assert_eq!(
            text(Relation::ExactDuplicate).as_deref(),
            Some(w.queries[0].text())
        );
        let looser = derive(Relation::LooserAccuracy, &w, 0).unwrap();
        assert_eq!(looser.query.accuracy().max_error, 0.01);
        let interval = derive(Relation::OtherRepeatInterval, &w, 0).unwrap();
        assert_eq!(repeat_interval_ms(&interval.query), Some(30_000));
    }

    /// Quantile, top-k, count and union derivations, PromQL and SQL.
    #[test]
    fn statistic_and_selection_relations_derive() {
        let mut quantile = sum_by(&["job"]);
        quantile.metric = "m_gauge".into();
        quantile.range = Some(RangeFunction::QuantileOverTime(0.9));
        quantile.outer = None;
        quantile.matchers = vec![("job".into(), "job1".into())];
        let w = promql(quantile, AccuracyTarget::max_error(0.01));
        let rank = derive(Relation::OtherQuantileRank, &w, 0).unwrap();
        assert_eq!(
            rank.query.text(),
            "quantile_over_time(0.5, m_gauge{job=\"job1\"}[5m])"
        );
        assert_eq!(
            rank.witnesses,
            vec![Witness::QuantileRank { from: 0.9, to: 0.5 }]
        );
        // A quantile sketch answers only quantiles: (e), not (h).
        assert!(derive(Relation::OtherStatistic, &w, 0).is_none());
        let union = derive(Relation::UnionOfSelections, &w, 0).unwrap();
        assert_eq!(
            union.base_addition.unwrap().text(),
            "quantile_over_time(0.9, m_gauge{job=\"job2\"}[5m])"
        );
        assert_eq!(
            union.query.text(),
            "quantile_over_time(0.9, m_gauge{job=~\"job1|job2\"}[5m])"
        );

        let mut topk = sum_by(&["job"]);
        topk.outer.as_mut().unwrap().op = OuterOp::Topk(10);
        let w = promql(topk, AccuracyTarget::exact());
        let smaller = derive(Relation::SmallerTopK, &w, 0).unwrap();
        assert_eq!(
            smaller.query.text(),
            "topk by (job) (3, rate(m_counter[5m]))"
        );

        let sql = SqlSpec {
            table: "metrics".into(),
            aggregate: SqlAggregate::Sum,
            group_by: vec!["city".into(), "host".into()],
            filters: vec![],
            window_ms: 300_000,
        };
        let w = generated_workload(
            Language::Sql,
            vec![FuzzQuery::generated(
                QuerySpec::Sql(sql),
                AccuracyTarget::exact(),
                10_000,
                None,
            )],
            100,
            10_000,
        );
        let filtered = derive(Relation::FilterOnGroupKey, &w, 0).unwrap();
        assert!(filtered
            .query
            .text()
            .contains("AND city = 'city1' GROUP BY city, host"));
        let coarser = derive(Relation::CoarserGrouping, &w, 0).unwrap();
        assert!(coarser.query.text().ends_with("GROUP BY city"));
    }

    /// The relations taken from the ASAPWorkloads query catalog derive its
    /// shapes: a ratio over q, a longer window (rolled up), `rate` as
    /// `increase`, and an outer operator that follows q's own.
    #[test]
    fn workload_catalog_relations_derive() {
        let rate = promql(sum_by(&["job", "instance"]), AccuracyTarget::exact());
        let text = |w: &FuzzWorkload, r| derive(r, w, 0).map(|d| d.query.text().to_string());
        assert_eq!(
            text(&rate, Relation::BinaryOverResult).as_deref(),
            Some(
                "sum by (job, instance) (rate(m_counter{region=\"region1\"}[5m])) / \
                 sum by (job, instance) (rate(m_counter[5m]))"
            )
        );
        let increase = derive(Relation::EquivalentRangeFunction, &rate, 0).unwrap();
        assert_eq!(
            increase.query.text(),
            "sum by (job, instance) (increase(m_counter[5m]))"
        );
        assert_eq!(
            increase.witnesses,
            vec![Witness::Graft {
                text: "(sum by (job, instance) (rate(m_counter[5m]))) * 300".into()
            }]
        );
        // As for (d), a rate's window is not merged pane state.
        assert_eq!(text(&rate, Relation::LongerWindow), None);

        let mut sum = sum_by(&["job", "instance"]);
        sum.metric = "m_gauge".into();
        sum.range = Some(RangeFunction::SumOverTime);
        sum.outer.as_mut().unwrap().op = OuterOp::Max;
        let w = promql(sum, AccuracyTarget::exact());
        assert_eq!(
            text(&w, Relation::BinaryOverResult).as_deref(),
            // Not an average under max: the filtered share instead.
            Some(
                "max by (job, instance) (sum_over_time(m_gauge{region=\"region1\"}[5m])) / \
                 max by (job, instance) (sum_over_time(m_gauge[5m]))"
            )
        );
        assert_eq!(
            text(&w, Relation::OuterOverResult).as_deref(),
            Some("max(max by (job, instance) (sum_over_time(m_gauge[5m])))")
        );
        assert_eq!(
            text(&w, Relation::LongerWindow).as_deref(),
            Some("max by (job, instance) (sum_over_time(m_gauge[10m]))")
        );
        let rollup = derive(Relation::LongerWindowRollup, &w, 0).unwrap();
        assert_eq!(
            rollup.query.text(),
            "max by (job) (sum_over_time(m_gauge[10m]))"
        );
        assert_eq!(
            rollup.witnesses[0],
            Witness::LongerWindow {
                window_ms: 600_000,
                rollup: Some(Box::new(Witness::ExactRollup {
                    labels: vec!["job".into()],
                    op: RollupOp::Max
                }))
            }
        );
        // (h): one UnivMon answers the count, distinct count, L2 and
        // entropy; q' asks for another than q.
        let entropy = SqlSpec {
            table: "metrics".into(),
            aggregate: SqlAggregate::FrequencyEntropy("src_ip".into()),
            group_by: vec![],
            filters: vec![],
            window_ms: 60_000,
        };
        let w = generated_workload(
            Language::Sql,
            vec![FuzzQuery::generated(
                QuerySpec::Sql(entropy),
                AccuracyTarget::max_error(0.05),
                10_000,
                None,
            )],
            100_000,
            15_000,
        );
        let h = derive(Relation::OtherStatistic, &w, 0).unwrap();
        assert_eq!(
            h.query.text(),
            "SELECT SQRT(SUM(CAST(c AS DOUBLE) * CAST(c AS DOUBLE))) AS v FROM \
             (SELECT src_ip, COUNT(*) AS c FROM metrics WHERE ts > now() - INTERVAL '1' \
             MINUTE GROUP BY src_ip)"
        );
        assert_eq!(h.witnesses[1], Witness::UnivMonOnly);
        assert_eq!(longer_window(600_000), Some(1_800_000));
        assert_eq!(longer_window(3_600_000), None);
    }

    /// Renaming renames sources, labels, facts and tables; a disjoint copy
    /// doubles the queries, sources and arrival rate.
    #[test]
    fn rename_and_copy_rewrite_facts() {
        let w = promql(sum_by(&["job"]), AccuracyTarget::exact());
        let r = renamed(&w, "_r", true).unwrap();
        assert_eq!(
            r.queries[0].text(),
            "sum by (job_r) (rate(m_counter_r[5m]))"
        );
        assert!(r.data.sources["m_counter_r"]
            .distinct_values_per_label
            .contains_key("job_r"));
        let both = disjoint_copy(&w).unwrap();
        assert_eq!(both.queries.len(), 2);
        assert_eq!(both.data.sources.len(), 4);
        assert_eq!(
            both.data.arrival.rows_per_second(),
            w.data.arrival.rows_per_second().map(|r| 2.0 * r)
        );
    }

    /// Every relation has a distinct name that parses back, and letters
    /// parse for the marginal ones.
    #[test]
    fn relation_names_parse() {
        for relation in Relation::ALL {
            assert_eq!(Relation::parse(relation.name()), Some(relation));
        }
        assert_eq!(Relation::parse("a"), Some(Relation::LooserAccuracy));
        assert_eq!(Relation::parse("k"), Some(Relation::UnionOfSelections));
        assert_eq!(Relation::parse("z"), None);
    }
}
