use std::rc::Rc;

use asap_aware_mapping::replacement::{
    search_workload, Replacement, ReplacementStrategy, TargetSubDAG,
};
use asap_aware_mapping::rewrite::AvgToSumOverCountStrategy;
use asap_frontend_promql::lower_promql;
use asap_types::pre_asap::{AggIntent, QueryExpr, Reduction};
use asap_types::types::AccuracyTarget;

/// The actual frontend range query retains its schema and exposes independently bindable work.
#[test]
fn range_average_exposes_sum_and_count() {
    let root = Rc::new(
        lower_promql(
            "avg_over_time(latency{job=\"api\"}[5m])",
            AccuracyTarget::Exact,
        )
        .unwrap(),
    );
    let replacements = AvgToSumOverCountStrategy.replacements(&TargetSubDAG::new(&root));
    assert_eq!(replacements.len(), 1);
    let Replacement::Rewrite(rewritten) = &replacements[0].replacement else {
        panic!("expected rewrite")
    };
    assert_eq!(
        root.output_schema().unwrap(),
        rewritten.output_schema().unwrap()
    );
    let QueryExpr::BinaryOp {
        lhs,
        rhs,
        vector_match: None,
        ..
    } = rewritten.as_ref()
    else {
        panic!("expected series division")
    };
    for branch in [lhs, rhs] {
        let QueryExpr::Aggregate {
            reduction: Reduction::PerEntity,
            child,
            ..
        } = branch.as_ref()
        else {
            panic!("expected per-series accumulator")
        };
        let QueryExpr::Aggregate {
            child: original_child,
            ..
        } = root.as_ref()
        else {
            panic!("expected range average")
        };
        assert!(Rc::ptr_eq(child, original_child));
    }
    let space = search_workload(vec![("avg", root)]);
    for want_sum in [true, false] {
        assert!(space.groups().any(|group| {
            let QueryExpr::Aggregate {
                reduction: Reduction::PerEntity,
                measures,
                ..
            } = group.target.as_ref()
            else {
                return false;
            };
            let matches = if want_sum {
                matches!(measures.as_slice(), [AggIntent::Sum { .. }])
            } else {
                matches!(
                    measures.as_slice(),
                    [AggIntent::Count {
                        accuracy: AccuracyTarget::Exact
                    }]
                )
            };
            matches
                && group
                    .candidates
                    .iter()
                    .any(|c| matches!(c.replacement, Replacement::Summary(_)))
        }));
    }
}

// Small range-algebra interpreter for the operators this rewrite emits. Empty
// ranges produce no series, including at the division, as PromQL requires.
fn evaluate(query: &QueryExpr, samples: &[f64]) -> Option<f64> {
    match query {
        QueryExpr::Aggregate { measures, .. } => {
            if samples.is_empty() {
                return None;
            }
            match measures.as_slice() {
                [AggIntent::Avg { .. }] => Some(samples.iter().sum::<f64>() / samples.len() as f64),
                [AggIntent::Sum { .. }] => Some(samples.iter().sum()),
                [AggIntent::Count { .. }] => Some(samples.len() as f64),
                _ => panic!("unexpected measure"),
            }
        }
        QueryExpr::BinaryOp {
            op:
                asap_types::pre_asap::query_expr::BinaryOpKind::Arithmetic(
                    asap_types::pre_asap::expr_ir::ArithmeticOpKind::Div,
                ),
            lhs,
            rhs,
            ..
        } => Some(evaluate(lhs, samples)? / evaluate(rhs, samples)?),
        _ => panic!("unexpected range expression"),
    }
}

/// Representative non-null ranges agree, including absent ranges and IEEE special values.
#[test]
fn range_average_and_rewrite_agree_on_samples() {
    let root = Rc::new(lower_promql("avg_over_time(latency[5m])", AccuracyTarget::Exact).unwrap());
    let replacements = AvgToSumOverCountStrategy.replacements(&TargetSubDAG::new(&root));
    let Replacement::Rewrite(rewritten) = &replacements[0].replacement else {
        panic!("expected rewrite")
    };
    for samples in [
        vec![],
        vec![3.0],
        vec![1.0, 2.0, 9.0],
        vec![-4.0, 0.0, 7.0],
        vec![f64::NAN],
        vec![f64::INFINITY],
    ] {
        match (evaluate(&root, &samples), evaluate(rewritten, &samples)) {
            (None, None) => (),
            (Some(a), Some(b)) if a.is_nan() && b.is_nan() => (),
            (a, b) => assert_eq!(a, b),
        }
    }
}
