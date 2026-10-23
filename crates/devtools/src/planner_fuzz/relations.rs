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
use asap_types::ir::schema::sketch::SketchStatistic;
use asap_types::ir::{
    ASAPOp, ExprSemantics, NonASAPOp, Operator, OperatorNode, Predicate, QueryRoot, ScalarExpr,
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
    /// (g) `q'` = `topk(3, q)`: derived by a top-k over `q`'s result.
    OuterOverResult,
    /// (h) `q'` = the count `q`'s sketch also answers (`Total`): derived by
    /// another estimate from `q`'s state when its family answers it.
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
    pub const ALL: [Relation; 19] = [
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

    /// Parses a CLI name or, for (a) to (k), its letter.
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
        (self as usize) <= Relation::UnionOfSelections as usize
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
    /// An exact `Aggregate` (`op` = sum, max or min of the value column)
    /// grouped by the `labels` columns over `q`'s result.
    ExactRollup { labels: Vec<String>, op: String },
    /// `q`'s topmost top-k (`Limit` or `TopK` aggregate) at `to` rows per
    /// group instead of `from`, over the same input.
    SmallerTopK { from: usize, to: usize },
    /// `q`'s topmost `SummaryEstimate` reading a `SummaryMerge` with
    /// `group_by` the `labels` columns over its state.
    SketchRollup { labels: Vec<String> },
    /// `q`'s `SummaryMerge` over a pane chain merging only the newest panes
    /// covering `window_ms`.
    NewestPanes { window_ms: u64 },
    /// `q`'s topmost quantile estimate (or exact quantile aggregate) at rank
    /// `to` instead of `from`.
    QuantileRank { from: f64, to: f64 },
    /// `q`'s topmost `SummaryEstimate` reading `statistic` instead, when the
    /// state's family answers it.
    OtherStatistic { statistic: SketchStatistic },
    /// `q`'s topmost `SummaryEstimate` over a `SummaryMerge` of its state
    /// and query `other`'s state.
    UnionWith { other: usize },
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
    pub base_cost: f64,
    pub base_plan: String,
    /// The related workload's (`W ∪ {q'}`, or `W'`).
    pub related_cost: f64,
    pub related_plan: String,
    /// The derived or added query's text, for a relation that adds one.
    pub related_query: Option<String>,
    /// The witness (marginal relations) or the confirming plan (a violated
    /// monotonicity relation: the other side's logical plan priced for this
    /// side).
    pub witness: Option<WitnessReport>,
    /// The bound `related_cost` was checked against, tolerance included.
    pub bound: f64,
    pub tolerance: f64,
    /// One line: the relation's numbers.
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WitnessReport {
    /// How `q'` was derived (marginal relations).
    pub derivation: Option<Witness>,
    pub cost: f64,
    pub plan: String,
    /// Marginal relations: `W`'s selected logical plan priced the same way.
    pub base_reference_cost: Option<f64>,
    /// `cost − base_reference_cost`.
    pub derive_cost: Option<f64>,
    /// The witness's per-node cost of the nodes only `q'` reads (the derived
    /// operators alone) and their operators.
    pub derived_node_cost: Option<f64>,
    pub derived_operators: Vec<String>,
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
        Relation::CoarserGrouping => match q.spec.as_ref()? {
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
                    OuterOp::Sum | OuterOp::Count => Some("sum"),
                    OuterOp::Max => Some("max"),
                    OuterOp::Min => Some("min"),
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
                            op: op.into(),
                        },
                    );
                }
                derived(q.with_spec(QuerySpec::Promql(coarse)), witnesses)
            }
            QuerySpec::Sql(spec) => {
                if spec.group_by.len() < 2 {
                    return None;
                }
                let coarse_columns = spec.group_by[..spec.group_by.len() - 1].to_vec();
                let mut coarse = spec.clone();
                coarse.group_by = coarse_columns.clone();
                let rollup_op = match spec.aggregate {
                    SqlAggregate::Sum | SqlAggregate::Count => Some("sum"),
                    SqlAggregate::Max => Some("max"),
                    SqlAggregate::Min => Some("min"),
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
                            op: op.into(),
                        },
                    );
                }
                derived(q.with_spec(QuerySpec::Sql(coarse)), witnesses)
            }
        },
        Relation::NestedShorterWindow => {
            let spec = match q.spec.as_ref()? {
                QuerySpec::Promql(spec) => {
                    spec.range?;
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
            let total = SketchStatistic::PointCount {
                key: ColumnRef::SampleValue,
                value: None,
            };
            let spec = match q.spec.as_ref()? {
                QuerySpec::Promql(spec) => {
                    let mut count = spec.clone();
                    match (&spec.outer, spec.range) {
                        (
                            Some(OuterAggregation {
                                op: OuterOp::Quantile(_),
                                ..
                            }),
                            None,
                        ) => {
                            count.outer.as_mut().expect("matched").op = OuterOp::Count;
                        }
                        (None, Some(RangeFunction::QuantileOverTime(_))) => {
                            count.range = Some(RangeFunction::CountOverTime);
                        }
                        _ => return None,
                    }
                    QuerySpec::Promql(count)
                }
                QuerySpec::Sql(spec) => {
                    if !matches!(
                        spec.aggregate,
                        SqlAggregate::Quantile(_) | SqlAggregate::CountDistinct(_)
                    ) {
                        return None;
                    }
                    let mut count = spec.clone();
                    count.aggregate = SqlAggregate::Count;
                    QuerySpec::Sql(count)
                }
            };
            derived(
                q.with_spec(spec),
                vec![Witness::OtherStatistic { statistic: total }],
            )
        }
        Relation::OuterOverResult => match q.spec.as_ref()? {
            QuerySpec::Promql(PromqlSpec {
                outer: Some(outer), ..
            }) if !matches!(outer.op, OuterOp::Topk(_)) && !outer.labels.is_empty() => {
                let text = format!("topk(3, {})", q.text());
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
            let (other_spec, union_spec, other_index) = match q.spec.as_ref()? {
                QuerySpec::Promql(spec) => {
                    let (label, value) = spec.matchers.first()?.clone();
                    let other_value = alternate_value(&value);
                    let mut other = spec.clone();
                    other.matchers[0].1 = other_value.clone();
                    let mut union = spec.clone();
                    union.matchers[0] = (label, format!("{value}|{other_value}"));
                    let union_text = union.render().replacen(
                        &format!("=\"{value}|{other_value}\""),
                        &format!("=~\"{value}|{other_value}\""),
                        1,
                    );
                    (
                        QuerySpec::Promql(other),
                        (union_text, QuerySpec::Promql(union)),
                        workload.queries.len(),
                    )
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
                    (
                        QuerySpec::Sql(other),
                        (union_text, QuerySpec::Sql(spec.clone())),
                        workload.queries.len(),
                    )
                }
            };
            let mut union = q.clone();
            union.entry.query.0 = union_spec.0;
            union.spec = None;
            Some(Derived {
                query: union,
                witnesses: vec![Witness::UnionWith { other: other_index }],
                base_addition: Some(q.with_spec(other_spec)),
            })
        }
        _ => None,
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

/// Rewrites `node` bottom-up: a node `replace` maps is replaced (its inputs
/// are not visited), and every node above a replaced one is rebuilt over
/// its new inputs with its schema derived again. Unchanged sub-DAGs keep
/// their `Rc`, so sharing is kept.
pub fn rewrite(
    node: &Rc<OperatorNode>,
    replace: &mut dyn FnMut(&Rc<OperatorNode>) -> Option<Rc<OperatorNode>>,
    memo: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
) -> Result<Rc<OperatorNode>, String> {
    if let Some(done) = memo.get(&Rc::as_ptr(node)) {
        return Ok(done.clone());
    }
    let rebuilt = match replace(node) {
        Some(replacement) => replacement,
        None => {
            let mut changed = false;
            for child in node.operator.children() {
                let new = rewrite(child, replace, memo)?;
                changed |= !Rc::ptr_eq(&new, child);
            }
            if changed {
                Rc::new(
                    node.with_new_children(|child| memo[&Rc::as_ptr(child)].clone())
                        .map_err(|e| format!("rebuilding {}: {e}", node.operator.kind_name()))?,
                )
            } else {
                node.clone()
            }
        }
    };
    memo.insert(Rc::as_ptr(node), rebuilt.clone());
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

/// `q`'s topmost `SummaryEstimate` replaced by `estimate(its input, its
/// statistic)`, which must keep its output schema's columns.
fn replace_topmost_estimate(
    root: &Rc<OperatorNode>,
    mut estimate: impl FnMut(&Rc<OperatorNode>, &SketchStatistic) -> Result<Rc<OperatorNode>, String>,
) -> Result<Rc<OperatorNode>, String> {
    let mut error = None;
    let mut done = false;
    let new = rewrite_root(root, "summary estimate", |node| {
        if done {
            return None;
        }
        let Some(ASAPOp::SummaryEstimate {
            summary_input,
            query,
        }) = node.asap()
        else {
            return None;
        };
        done = true;
        match estimate(summary_input, query) {
            Ok(new) => Some(new),
            Err(e) => {
                error = Some(e);
                Some(node.clone())
            }
        }
    })?;
    match error {
        Some(e) => Err(e),
        None => Ok(new),
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
            let mut derivation = base.clone();
            derivation.queries = vec![FuzzQuery {
                spec: None,
                entry: {
                    let mut entry = base.queries[target].entry.clone();
                    entry.query.0 = text.clone();
                    entry
                },
            }];
            let lowered = planner.lower(&derivation)?;
            let lowered = lowered[0]
                .as_operator()
                .ok_or("the derivation is a scalar query")?;
            let lowered_q = planned.stage0_roots[target]
                .as_operator()
                .ok_or("q is a scalar query")?;
            let names = |node: &OperatorNode| -> Vec<String> {
                node.schema.fields.iter().map(|f| f.name.clone()).collect()
            };
            if names(lowered_q) != names(q_root) {
                return Err("q's plan and q's lowering have different output columns".into());
            }
            rewrite_root(lowered, "lowering of q inside the derivation", |node| {
                (**node == **lowered_q).then(|| q_root.clone())
            })
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
            // The value column: the last column that is not a group key, a
            // timestamp or a series identity.
            let value = fields
                .iter()
                .rposition(|f| {
                    !labels.contains(&f.name) && f.name != "ts" && !f.name.starts_with('$')
                })
                .ok_or("q's output has no value column")?;
            let measure = match op.as_str() {
                "sum" => AggIntent::Sum { col: Some(value) },
                "max" => AggIntent::Max { col: Some(value) },
                _ => AggIntent::Min { col: Some(value) },
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
        Witness::SketchRollup { labels } => replace_topmost_estimate(q_root, |state, query| {
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
            let merge = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
                children: vec![state.clone()],
                newest_panes: None,
                group_by: Some(group_by),
            }))
            .map_err(|e| format!("rollup merge: {e}"))?;
            OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input: merge,
                query: query.clone(),
            }))
            .map_err(|e| format!("rollup estimate: {e}"))
        })
        .and_then(|root| {
            if Rc::ptr_eq(&root, q_root) {
                Err("no rollup".into())
            } else {
                Ok(root)
            }
        }),
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
        Witness::OtherStatistic { statistic } => replace_topmost_estimate(q_root, |state, _| {
            let family = match state.summary_build() {
                Some(ASAPOp::SummaryAgg { family, .. } | ASAPOp::PaneChain { family, .. }) => {
                    family.clone()
                }
                _ => return Err("q's estimate reads no single summary build".into()),
            };
            if !FamilyCapability::of(&family).answers(statistic) {
                return Err(format!(
                    "q's family {family:?} does not answer {statistic:?}"
                ));
            }
            OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input: state.clone(),
                query: statistic.clone(),
            }))
            .map_err(|e| format!("estimate: {e}"))
        }),
        Witness::UnionWith { other } => {
            let other_root = base_roots
                .get(*other)
                .and_then(QueryRoot::as_operator)
                .ok_or("no other query")?;
            let mut other_state = None;
            rewrite_root(other_root, "summary estimate", |node| {
                if let (None, Some(ASAPOp::SummaryEstimate { summary_input, .. })) =
                    (&other_state, node.asap())
                {
                    other_state = Some(summary_input.clone());
                }
                None
            })
            .ok();
            let other_state =
                other_state.ok_or("the other query's plan has no summary estimate")?;
            replace_topmost_estimate(q_root, |state, query| {
                let merge = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
                    children: vec![state.clone(), other_state.clone()],
                    newest_panes: None,
                    group_by: None,
                }))
                .map_err(|e| format!("union merge: {e}"))?;
                OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryEstimate {
                    summary_input: merge,
                    query: query.clone(),
                }))
                .map_err(|e| format!("union estimate: {e}"))
            })
        }
    }
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

/// The other side's logical plan priced for `workload`: a plan for it the
/// search could have selected.
fn confirming(
    planner: &Planner,
    workload: &FuzzWorkload,
    roots: Option<&Vec<QueryRoot>>,
    keep: usize,
) -> Option<WitnessReport> {
    let roots = roots?;
    let priced = planner.price(workload, &roots[..keep], None).ok()?;
    Some(WitnessReport {
        derivation: None,
        cost: priced.cost_per_second,
        plan: priced.selected,
        base_reference_cost: None,
        derive_cost: None,
        derived_node_cost: None,
        derived_operators: Vec::new(),
    })
}

/// Evaluates `check`.
pub fn evaluate(planner: &mut Planner, check: &Check) -> Outcome {
    evaluate_inner(planner, check).unwrap_or_else(|reason| Outcome::Inconclusive { reason })
}

fn evaluate_inner(planner: &mut Planner, check: &Check) -> Result<Outcome, String> {
    let w = &check.workload;
    if check.relation.is_marginal() {
        return evaluate_marginal(planner, check);
    }
    let target = check.target;
    let (related, related_query) = match check.relation {
        Relation::TighterAccuracy => {
            let q = &w.queries[target];
            let Some(tight) = tighter(q.accuracy()) else {
                return inconclusive("q is exact");
            };
            (w.with_query_replaced(target, q.with_accuracy(tight)), None)
        }
        Relation::RelaxedLatency => {
            let q = &w.queries[target];
            if q.entry.requirements.max_latency_ms.is_none() {
                return inconclusive("q has no latency bound");
            }
            let mut relaxed = q.clone();
            relaxed.entry.requirements.max_latency_ms = None;
            (w.with_query_replaced(target, relaxed), None)
        }
        Relation::AddedQuery => {
            let Some(added) = &check.added else {
                return inconclusive("no added query");
            };
            (w.with_query(added.clone()), Some(added.text().to_string()))
        }
        Relation::NoRawRetention => {
            if !w.raw_data_retained {
                return inconclusive("raw data is not retained");
            }
            let mut related = w.clone();
            related.raw_data_retained = false;
            (related, None)
        }
        Relation::ResultsRetained => {
            if w.query_time_results_retained {
                return inconclusive("query-time results are already retained");
            }
            let mut related = w.clone();
            related.query_time_results_retained = true;
            (related, None)
        }
        Relation::Renamed => match renamed(w, "_r", true) {
            Some(related) => (related, None),
            None => return inconclusive("a query has no structured form"),
        },
        Relation::Reordered => {
            if w.queries.len() < 2 {
                return inconclusive("one query");
            }
            let mut related = w.clone();
            related.queries.reverse();
            (related, None)
        }
        Relation::DisjointCopy => match disjoint_copy(w) {
            Some(related) => (related, None),
            None => return inconclusive("a query has no structured form"),
        },
        _ => unreachable!("marginal relations are evaluated above"),
    };
    let base = planned(planner, w, "W")?;
    let other = planned(planner, &related, "W'")?;
    let (b, r) = (base.cost_per_second, other.cost_per_second);
    let (bound, tolerance, violated, witness) = match check.relation {
        Relation::TighterAccuracy | Relation::AddedQuery | Relation::NoRawRetention => {
            let tolerance = planner.settings.tolerance(b);
            let violated = r < b - tolerance;
            // The related plan, minus an added query, is a plan for W.
            let witness = violated
                .then(|| confirming(planner, w, other.logical_roots.as_ref(), w.queries.len()))
                .flatten();
            (b - tolerance, tolerance, violated, witness)
        }
        Relation::ResultsRetained | Relation::RelaxedLatency => {
            let tolerance = planner.settings.tolerance(b);
            let violated = r > b + tolerance;
            let witness = violated
                .then(|| {
                    confirming(
                        planner,
                        &related,
                        base.logical_roots.as_ref(),
                        w.queries.len(),
                    )
                })
                .flatten();
            (b + tolerance, tolerance, violated, witness)
        }
        Relation::DisjointCopy => {
            let tolerance = planner.settings.tolerance(2.0 * b);
            ((2.0 * b), tolerance, (r - 2.0 * b).abs() > tolerance, None)
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
        base_cost: b,
        base_plan: base.selected.clone(),
        related_cost: r,
        related_plan: other.selected.clone(),
        related_query,
        witness,
        bound,
        tolerance,
        summary,
    };
    Ok(if violated {
        Outcome::Violated(measurement)
    } else {
        Outcome::Holds(measurement)
    })
}

fn evaluate_marginal(planner: &mut Planner, check: &Check) -> Result<Outcome, String> {
    let Some(derived) = derive(check.relation, &check.workload, check.target) else {
        return inconclusive("not applicable to q");
    };
    let base_workload = match &derived.base_addition {
        Some(addition) => check.workload.with_query(addition.clone()),
        None => check.workload.clone(),
    };
    let related = base_workload.with_query(derived.query.clone());
    let base = planned(planner, &base_workload, "W")?;
    let with = planned(planner, &related, "W ∪ {q'}")?;
    let Some(base_roots) = base.logical_roots.clone() else {
        return inconclusive("the search keeps no logical plan for W");
    };
    let base_reference = planner
        .price(&base_workload, &base_roots, None)
        .or_else(|e| inconclusive(format!("W's plan does not price: {e}")))?;
    let mut best: Option<(Witness, Priced)> = None;
    let mut failures = Vec::new();
    for witness in &derived.witnesses {
        let priced = witness_root(
            planner,
            witness,
            &base_workload,
            &base,
            &base_roots,
            check.target,
        )
        .and_then(|root| {
            let mut roots = base_roots.clone();
            roots.push(QueryRoot::Operator(root));
            let key_filter = matches!(witness, Witness::Filter { .. }).then_some(roots.len() - 1);
            planner.price(&related, &roots, key_filter)
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
    let violated = delta > derive_cost + tolerance;
    let last = priced.private_node_cost_per_second.len() - 1;
    let summary = format!(
        "cost(W ∪ {{q'}}) − cost(W) = {:.6} − {:.6} = {delta:.6}; derive_cost = {:.6} (witness {:.6} − W's plan {:.6}); tolerance {tolerance:.6}",
        with.cost_per_second,
        base.cost_per_second,
        derive_cost,
        priced.cost_per_second,
        base_reference.cost_per_second,
    );
    let measurement = Measurement {
        base_cost: base.cost_per_second,
        base_plan: base.selected.clone(),
        related_cost: with.cost_per_second,
        related_plan: with.selected.clone(),
        related_query: Some(derived.query.text().to_string()),
        witness: Some(WitnessReport {
            derivation: Some(witness),
            cost: priced.cost_per_second,
            plan: priced.selected.clone(),
            base_reference_cost: Some(base_reference.cost_per_second),
            derive_cost: Some(derive_cost),
            derived_node_cost: Some(priced.private_node_cost_per_second[last]),
            derived_operators: priced.private_operators[last].clone(),
        }),
        bound: base.cost_per_second + derive_cost + tolerance,
        tolerance,
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
                if let SqlAggregate::CountDistinct(c) = &mut spec.aggregate {
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
                op: "sum".into()
            }
        );
        assert_eq!(
            text(Relation::NestedShorterWindow).as_deref(),
            Some("sum by (job, instance) (rate(m_counter[1m]))")
        );
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
        let count = derive(Relation::OtherStatistic, &w, 0).unwrap();
        assert_eq!(
            count.query.text(),
            "count_over_time(m_gauge{job=\"job1\"}[5m])"
        );
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
