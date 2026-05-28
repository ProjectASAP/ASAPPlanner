//! Layer 3 aggregation-intent vocabulary — "what to compute, not how".
//!
//! L3 carries intent ("compute a quantile to ε=0.01 accuracy"); the choice
//! between `HashAgg` / `SortAgg` / `SketchAgg(KLL{k=200})` is an L4 cost-aware
//! decision, not encoded here.
//!
//! `AggIntent::TopK` is a first-class *intent* — a dedicated heavy-hitter
//! sketch (SpaceSaving, CMS-with-heap) computes it in one pass. Generic
//! `ORDER BY value LIMIT k` stays as the `QueryExpr::Sort + Limit` operator
//! pair. L1→L2→L3 lowering picks one or the other deterministically.

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
/// `Variance`) carry `col: Option<ColumnId>` — the positional input column
/// they reduce. `None` is the PromQL convention "the time-series sample
/// value"; SQL `SUM(bytes), AVG(latency)` sets distinct `Some(id)`s so a
/// multi-aggregate node binds each reducer to the right column.
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
    Quantile {
        q: f64,
        accuracy: AccuracyTarget,
    },
    /// Heavy-hitter top-k — served by a dedicated sketch in one pass. The
    /// group-by keys live on the enclosing `Aggregate.by`.
    TopK {
        k: usize,
        accuracy: AccuracyTarget,
    },
    Cardinality {
        accuracy: AccuracyTarget,
    },

    // ── Time-series streaming derivatives ────────────────────────────────
    // Counter-reset adjustment; not equivalent to Sum/Count over a window.
    // The temporal range lives on the enclosing `QueryExpr::TimeRange` node,
    // not in the intent — this keeps the intent vocabulary range-agnostic.
    Rate,
    Increase,
}

impl AggIntent {
    /// Which data model this intent semantically requires. L4 rules consult
    /// this to skip non-applicable intents (e.g. `Rate` over a tabular source).
    pub fn requires(&self) -> DataModel {
        match self {
            Self::Rate | Self::Increase => DataModel::TimeSeries,
            _ => DataModel::Any,
        }
    }

    /// Whether this is a *per-series* reduction — it reduces a single series'
    /// samples over its range window (one value out per series), so it does
    /// **not** collapse across series and every label column is preserved.
    /// `rate`/`increase` carry their window in the intent. (Cross-series
    /// reductions like `sum`/`avg` over a series set return `false`.)
    pub fn is_per_series(&self) -> bool {
        matches!(self, Self::Rate | Self::Increase)
    }

    /// The positional input column this intent reduces, if it carries one.
    /// `None` = the synthetic time-series sample value (PromQL) or an
    /// argument-less aggregate (`Count` / `Cardinality` / `TopK`). Used by
    /// schema derivation to resolve each reducer's input column.
    pub fn input_col(&self) -> Option<ColumnId> {
        match self {
            AggIntent::Sum { col }
            | AggIntent::Min { col }
            | AggIntent::Max { col }
            | AggIntent::Avg { col }
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
    )
}

/// Accuracy parameter as a fractional ε (`0.0` for exact ops), unpacked from
/// the typed `AccuracyTarget` on Quantile / Cardinality / Count / TopK.
pub fn agg_accuracy(op: &AggIntent) -> f64 {
    match op {
        AggIntent::Quantile { accuracy, .. }
        | AggIntent::Cardinality { accuracy }
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

/// Default `Cardinality` intent — HLL standard error at precision p=14.
pub fn default_cardinality() -> AggIntent {
    AggIntent::Cardinality {
        accuracy: AccuracyTarget::Epsilon(1.04 / ((1u64 << 14) as f64).sqrt()),
    }
}

/// Default `Quantile` intent at φ = `q`, `accuracy = ε 0.01`.
pub fn default_quantile(q: f64) -> AggIntent {
    AggIntent::Quantile {
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
        let v = AggIntent::Quantile {
            q: 0.99,
            accuracy: AccuracyTarget::Epsilon(0.01),
        };
        let json = serde_json::to_string(&v).unwrap();
        let back: AggIntent = serde_json::from_str(&json).unwrap();
        assert_eq!(v, back);
    }
}
