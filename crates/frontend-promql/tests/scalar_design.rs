//! Scalar expressions never become constant-wrapper operators.
mod support;
use asap_types::ir::scalar::{ArithmeticOpKind, ScalarValue};
use asap_types::ir::{NonASAPOp, QueryRoot, ScalarExpr};
use asap_types::types::AccuracyTarget;

fn root(query: &str) -> QueryRoot {
    asap_frontend_promql::lower_promql_query_workload(
        &support::workload(query, AccuracyTarget::Exact),
        0,
    )
    .unwrap()
    .remove(0)
}

#[test]
fn standalone_scalars_are_expressions() {
    for query in [
        "2",
        "time()",
        "scalar(sum(up)) + 1",
        "1 < bool 2",
        "-time()",
    ] {
        let QueryRoot::Scalar(expr) = root(query) else {
            panic!("{query} became an operator")
        };
        expr.scalar_type(&Default::default()).unwrap();
    }
}

#[test]
fn arithmetic_projects_the_sample_and_preserves_full_identity_and_time() {
    let QueryRoot::Operator(node) = root("up * 2") else {
        panic!()
    };
    let NonASAPOp::Project { child, cols, .. } = node.expect_non_asap() else {
        panic!()
    };
    assert!(child.schema.has_promql_series_identity());
    assert!(node.schema.has_promql_series_identity());
    assert_eq!(node.schema.time_index, child.schema.time_index);
    let value = node.schema.column_id("value").unwrap();
    assert!(
        matches!(&cols[value].expr, ScalarExpr::Arithmetic { op: ArithmeticOpKind::Mul, right, .. } if **right == ScalarExpr::Literal(ScalarValue::Float64(2.0)))
    );
    assert!(cols.iter().any(|c| matches!(&c.expr, ScalarExpr::FunctionCall { name, .. } if name == "promql_drop_metric_name")));
}

#[test]
fn non_bool_comparisons_keep_vector_samples_even_with_scalar_on_left() {
    for query in ["up > 0", "0 < up"] {
        let QueryRoot::Operator(node) = root(query) else {
            panic!()
        };
        let NonASAPOp::Filter { child, .. } = node.expect_non_asap() else {
            panic!()
        };
        assert_eq!(node.schema, child.schema);
    }
}

#[test]
fn bool_comparison_projects_zero_or_one() {
    let QueryRoot::Operator(node) = root("up > bool 0") else {
        panic!()
    };
    let NonASAPOp::Project { cols, .. } = node.expect_non_asap() else {
        panic!()
    };
    assert!(matches!(
        cols[node.schema.column_id("value").unwrap()].expr,
        ScalarExpr::Case { .. }
    ));
}

#[test]
fn scalar_plan_dependencies_remain_visible() {
    let QueryRoot::Operator(node) = root("up * scalar(sum(up))") else {
        panic!()
    };
    assert_eq!(node.children().len(), 2);
}

/// Pointwise functions own scalar parameters, including vector-to-scalar reads.
#[test]
fn pointwise_functions_are_typed_scalar_projections() {
    for query in [
        "abs(up)",
        "round(up, scalar(sum(other)))",
        "clamp(up, time() - 1, time())",
        "year(up)",
        "hour()",
    ] {
        let QueryRoot::Operator(node) = root(query) else {
            panic!()
        };
        let NonASAPOp::Project { cols, .. } = node.expect_non_asap() else {
            panic!("{query}: expected projection")
        };
        assert!(matches!(
            &cols[node.schema.column_id("value").unwrap()].expr,
            ScalarExpr::FunctionCall { .. }
        ));
        node.validate_structure().unwrap();
    }
}

/// Negation preserves the metric name and complete identity unlike multiplication.
#[test]
fn unary_minus_preserves_identity() {
    let QueryRoot::Operator(node) = root("-up") else {
        panic!()
    };
    let NonASAPOp::Project { child, cols, .. } = node.expect_non_asap() else {
        panic!()
    };
    assert_eq!(node.schema, child.schema);
    assert!(matches!(
        &cols[node.schema.column_id("value").unwrap()].expr,
        ScalarExpr::Negative { .. }
    ));
    for (index, col) in cols.iter().enumerate() {
        if index != node.schema.column_id("value").unwrap() {
            assert_eq!(col.expr, ScalarExpr::Column(index));
        }
    }
}
