use datafusion::common::ScalarValue;
use datafusion::logical_expr::{BinaryExpr, Expr, Operator};

use asap_control_core::intent_algebra::expr::TimeRange;

/// Core: classify a pre-split list of conjuncts into time bounds + residual.
pub(super) fn extract_time_range_from_conjuncts<'a>(
    conjuncts: Vec<&'a Expr>,
    time_col: &str,
) -> (Option<TimeRange>, Vec<&'a Expr>) {
    let mut start_ms: Option<i64> = None;
    let mut end_ms: Option<i64> = None;
    let mut non_time: Vec<&'a Expr> = vec![];

    for c in conjuncts {
        match classify_time_pred(c, time_col) {
            TimeClass::Start(ms) => {
                start_ms = Some(start_ms.map_or(ms, |s: i64| s.max(ms)));
            }
            TimeClass::End(ms) => {
                end_ms = Some(end_ms.map_or(ms, |e: i64| e.min(ms)));
            }
            TimeClass::Both(lo, hi) => {
                start_ms = Some(start_ms.map_or(lo, |s: i64| s.max(lo)));
                end_ms = Some(end_ms.map_or(hi, |e: i64| e.min(hi)));
            }
            TimeClass::NonTime => non_time.push(c),
        }
    }

    let range = if start_ms.is_some() || end_ms.is_some() {
        Some(TimeRange { start_ms, end_ms })
    } else {
        None
    };
    (range, non_time)
}

/// Convenience wrapper: split a single expression then classify conjuncts.
#[cfg(test)]
fn extract_time_range<'a>(expr: &'a Expr, time_col: &str) -> (Option<TimeRange>, Vec<&'a Expr>) {
    use super::expr::split_conjuncts;
    extract_time_range_from_conjuncts(split_conjuncts(expr), time_col)
}

enum TimeClass {
    Start(i64),
    End(i64),
    /// BETWEEN low AND high on the time column — contributes both bounds at once.
    Both(i64, i64),
    NonTime,
}

fn classify_time_pred(expr: &Expr, time_col: &str) -> TimeClass {
    match expr {
        // `ts BETWEEN low AND high` — contributes both a start and end bound.
        // `ts NOT BETWEEN …` cannot be expressed as a contiguous TimeRange; treat as non-time.
        Expr::Between(b) if !b.negated && is_time_col(&b.expr, time_col) => {
            match (expr_to_ms(&b.low), expr_to_ms(&b.high)) {
                (Some(lo), Some(hi)) => TimeClass::Both(lo, hi),
                _ => TimeClass::NonTime,
            }
        }

        Expr::BinaryExpr(BinaryExpr { left, op, right }) => {
            let (col_is_left, val_expr): (bool, &Expr) = if is_time_col(left, time_col) {
                (true, right)
            } else if is_time_col(right, time_col) {
                (false, left)
            } else {
                return TimeClass::NonTime;
            };
            let Some(ms) = expr_to_ms(val_expr) else {
                return TimeClass::NonTime;
            };
            match (op, col_is_left) {
                (Operator::Gt | Operator::GtEq, true) | (Operator::Lt | Operator::LtEq, false) => {
                    TimeClass::Start(ms)
                }
                (Operator::Lt | Operator::LtEq, true) | (Operator::Gt | Operator::GtEq, false) => {
                    TimeClass::End(ms)
                }
                // Eq (exact timestamp equality) and all other operators cannot be
                // expressed as a contiguous half-open range, so leave them as
                // regular Filter predicates rather than time-range bounds.
                _ => TimeClass::NonTime,
            }
        }

        _ => TimeClass::NonTime,
    }
}

fn is_time_col(expr: &Expr, time_col: &str) -> bool {
    match expr {
        Expr::Column(col) => col.name == time_col,
        Expr::Cast(c) => is_time_col(&c.expr, time_col),
        _ => false,
    }
}

fn expr_to_ms(expr: &Expr) -> Option<i64> {
    match expr {
        Expr::Literal(sv) => scalar_to_ms(sv),
        Expr::Cast(c) => expr_to_ms(&c.expr),
        Expr::TryCast(c) => expr_to_ms(&c.expr),
        _ => None,
    }
}

/// Round `v` to the nearest millisecond and return it as `i64`.
/// Returns `None` if `v` is non-finite or outside the `i64` range.
fn float_to_ms(v: f64) -> Option<i64> {
    let rounded = v.round();
    // i64::MAX as f64 rounds up to 2^63, which overflows i64 on cast.
    // Use strict less-than for the upper bound.
    if rounded.is_finite() && rounded >= i64::MIN as f64 && rounded < i64::MAX as f64 {
        Some(rounded as i64)
    } else {
        None
    }
}

fn scalar_to_ms(sv: &ScalarValue) -> Option<i64> {
    match sv {
        ScalarValue::Int64(Some(v)) => Some(*v),
        ScalarValue::Int32(Some(v)) => Some(*v as i64),
        ScalarValue::Float64(Some(v)) => float_to_ms(*v),
        ScalarValue::Float32(Some(v)) => float_to_ms(*v as f64),
        ScalarValue::TimestampMillisecond(Some(ms), _) => Some(*ms),
        ScalarValue::TimestampNanosecond(Some(ns), _) => Some(*ns / 1_000_000),
        ScalarValue::TimestampMicrosecond(Some(us), _) => Some(*us / 1_000),
        ScalarValue::TimestampSecond(Some(s), _) => Some(*s * 1_000),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::common::ScalarValue;
    use datafusion::logical_expr::{BinaryExpr, Expr, Operator};

    fn col(name: &str) -> Expr {
        Expr::Column(datafusion::common::Column::new_unqualified(name))
    }
    fn int(v: i64) -> Expr {
        Expr::Literal(ScalarValue::Int64(Some(v)))
    }
    fn float(v: f64) -> Expr {
        Expr::Literal(ScalarValue::Float64(Some(v)))
    }
    fn bin(left: Expr, op: Operator, right: Expr) -> Expr {
        Expr::BinaryExpr(BinaryExpr {
            left: Box::new(left),
            op,
            right: Box::new(right),
        })
    }
    fn and(l: Expr, r: Expr) -> Expr {
        bin(l, Operator::And, r)
    }

    #[test]
    fn col_left_gt_lower_bound() {
        let expr = bin(col("ts"), Operator::Gt, int(1000));
        let (range, non_time) = extract_time_range(&expr, "ts");
        assert_eq!(
            range,
            Some(TimeRange {
                start_ms: Some(1000),
                end_ms: None
            })
        );
        assert!(non_time.is_empty());
    }

    #[test]
    fn col_right_lt_is_start_bound() {
        // `1000 < ts` ≡ `ts > 1000`
        let expr = bin(int(1000), Operator::Lt, col("ts"));
        let (range, non_time) = extract_time_range(&expr, "ts");
        assert_eq!(
            range,
            Some(TimeRange {
                start_ms: Some(1000),
                end_ms: None
            })
        );
        assert!(non_time.is_empty());
    }

    #[test]
    fn col_right_gt_is_end_bound() {
        // `2000 > ts` ≡ `ts < 2000`
        let expr = bin(int(2000), Operator::Gt, col("ts"));
        let (range, non_time) = extract_time_range(&expr, "ts");
        assert_eq!(
            range,
            Some(TimeRange {
                start_ms: None,
                end_ms: Some(2000)
            })
        );
        assert!(non_time.is_empty());
    }

    #[test]
    fn overlapping_repeated_bounds_tighten() {
        // `ts > 500 AND ts > 1000` → start = 1000 (tighter)
        let expr = and(
            bin(col("ts"), Operator::Gt, int(500)),
            bin(col("ts"), Operator::Gt, int(1000)),
        );
        let (range, _) = extract_time_range(&expr, "ts");
        assert_eq!(range.unwrap().start_ms, Some(1000));
    }

    #[test]
    fn overlapping_end_bounds_tighten() {
        // `ts < 2000 AND ts < 1500` → end = 1500 (tighter)
        let expr = and(
            bin(col("ts"), Operator::Lt, int(2000)),
            bin(col("ts"), Operator::Lt, int(1500)),
        );
        let (range, _) = extract_time_range(&expr, "ts");
        assert_eq!(range.unwrap().end_ms, Some(1500));
    }

    #[test]
    fn between_contributes_both_bounds() {
        use datafusion::logical_expr::Between;
        let expr = Expr::Between(Between {
            expr: Box::new(col("ts")),
            negated: false,
            low: Box::new(int(1000)),
            high: Box::new(int(2000)),
        });
        let (range, non_time) = extract_time_range(&expr, "ts");
        assert_eq!(
            range,
            Some(TimeRange {
                start_ms: Some(1000),
                end_ms: Some(2000)
            })
        );
        assert!(non_time.is_empty());
    }

    #[test]
    fn not_between_is_non_time() {
        use datafusion::logical_expr::Between;
        let expr = Expr::Between(Between {
            expr: Box::new(col("ts")),
            negated: true,
            low: Box::new(int(1000)),
            high: Box::new(int(2000)),
        });
        let (range, non_time) = extract_time_range(&expr, "ts");
        assert!(range.is_none());
        assert_eq!(non_time.len(), 1);
    }

    #[test]
    fn float_literal_extracted_as_ms() {
        let expr = bin(col("ts"), Operator::Gt, float(1_000_000.0));
        let (range, non_time) = extract_time_range(&expr, "ts");
        assert_eq!(
            range,
            Some(TimeRange {
                start_ms: Some(1_000_000),
                end_ms: None
            })
        );
        assert!(non_time.is_empty());
    }

    #[test]
    fn non_time_conjunct_passes_through() {
        let expr = and(
            bin(col("ts"), Operator::Gt, int(1000)),
            bin(col("value"), Operator::Gt, int(0)),
        );
        let (range, non_time) = extract_time_range(&expr, "ts");
        assert!(range.is_some());
        assert_eq!(non_time.len(), 1);
    }
}
