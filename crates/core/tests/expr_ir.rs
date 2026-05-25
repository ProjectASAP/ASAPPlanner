use asap_control_core::intent_algebra::{ArithOp, ColumnRef, CompareOp, L3Expr, L3Scalar};

// ── L3Scalar ──────────────────────────────────────────────────────────────────

#[test]
fn l3scalar_int64_eq() {
    assert_eq!(L3Scalar::Int64(42), L3Scalar::Int64(42));
    assert_ne!(L3Scalar::Int64(1), L3Scalar::Int64(2));
}

#[test]
fn l3scalar_float64_eq() {
    assert_eq!(L3Scalar::Float64(1.5), L3Scalar::Float64(1.5));
    assert_ne!(L3Scalar::Float64(1.5), L3Scalar::Float64(2.5));
}

#[test]
fn l3scalar_utf8_eq() {
    assert_eq!(
        L3Scalar::Utf8("hello".into()),
        L3Scalar::Utf8("hello".into())
    );
    assert_ne!(L3Scalar::Utf8("a".into()), L3Scalar::Utf8("b".into()));
}

#[test]
fn l3scalar_null_eq() {
    assert_eq!(L3Scalar::Null, L3Scalar::Null);
}

// ── L3Expr construction ───────────────────────────────────────────────────────

#[test]
fn l3expr_column_eq() {
    let a = L3Expr::Column(ColumnRef("ts".into()));
    let b = L3Expr::Column(ColumnRef("ts".into()));
    assert_eq!(a, b);
    assert_ne!(
        L3Expr::Column(ColumnRef("ts".into())),
        L3Expr::Column(ColumnRef("x".into()))
    );
}

#[test]
fn l3expr_literal_eq() {
    assert_eq!(
        L3Expr::Literal(L3Scalar::Int64(5)),
        L3Expr::Literal(L3Scalar::Int64(5))
    );
}

#[test]
fn l3expr_compare_eq() {
    let make = || L3Expr::Compare {
        left: Box::new(L3Expr::Column(ColumnRef("v".into()))),
        op: CompareOp::Gt,
        right: Box::new(L3Expr::Literal(L3Scalar::Float64(0.0))),
    };
    assert_eq!(make(), make());
}

// ── conjuncts() ───────────────────────────────────────────────────────────────

#[test]
fn bool_and_conjuncts_returns_all_elements() {
    let a = L3Expr::Column(ColumnRef("a".into()));
    let b = L3Expr::Column(ColumnRef("b".into()));
    let expr = L3Expr::BoolAnd(vec![a.clone(), b.clone()]);
    let c = expr.conjuncts();
    assert_eq!(c.len(), 2);
    assert_eq!(c[0], a);
    assert_eq!(c[1], b);
}

#[test]
fn non_and_conjuncts_returns_self_as_slice() {
    let col = L3Expr::Column(ColumnRef("x".into()));
    let c = col.conjuncts();
    assert_eq!(c.len(), 1);
    assert_eq!(c[0], col);
}

#[test]
fn literal_conjuncts_returns_self() {
    let lit = L3Expr::Literal(L3Scalar::Boolean(true));
    assert_eq!(lit.conjuncts().len(), 1);
}

// ── disjuncts() ───────────────────────────────────────────────────────────────

#[test]
fn bool_or_disjuncts_returns_all_elements() {
    let a = L3Expr::Literal(L3Scalar::Int64(1));
    let b = L3Expr::Literal(L3Scalar::Int64(2));
    let expr = L3Expr::BoolOr(vec![a.clone(), b.clone()]);
    let d = expr.disjuncts();
    assert_eq!(d.len(), 2);
    assert_eq!(d[0], a);
    assert_eq!(d[1], b);
}

#[test]
fn non_or_disjuncts_returns_self() {
    let lit = L3Expr::Literal(L3Scalar::Null);
    assert_eq!(lit.disjuncts().len(), 1);
}

// ── columns_referenced() ──────────────────────────────────────────────────────

#[test]
fn columns_referenced_from_column_node() {
    let expr = L3Expr::Column(ColumnRef("ts".into()));
    let refs = expr.columns_referenced();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].0, "ts");
}

#[test]
fn columns_referenced_from_literal_is_empty() {
    let expr = L3Expr::Literal(L3Scalar::Int64(99));
    assert!(expr.columns_referenced().is_empty());
}

#[test]
fn columns_referenced_from_compare() {
    let expr = L3Expr::Compare {
        left: Box::new(L3Expr::Column(ColumnRef("a".into()))),
        op: CompareOp::Gt,
        right: Box::new(L3Expr::Column(ColumnRef("b".into()))),
    };
    let refs = expr.columns_referenced();
    assert_eq!(refs.len(), 2);
    assert!(refs.iter().any(|r| r.0 == "a"));
    assert!(refs.iter().any(|r| r.0 == "b"));
}

#[test]
fn columns_referenced_from_compare_with_literal() {
    let expr = L3Expr::Compare {
        left: Box::new(L3Expr::Column(ColumnRef("value".into()))),
        op: CompareOp::Ge,
        right: Box::new(L3Expr::Literal(L3Scalar::Float64(0.0))),
    };
    let refs = expr.columns_referenced();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].0, "value");
}

#[test]
fn columns_referenced_from_bool_and() {
    let expr = L3Expr::BoolAnd(vec![
        L3Expr::Compare {
            left: Box::new(L3Expr::Column(ColumnRef("region".into()))),
            op: CompareOp::Eq,
            right: Box::new(L3Expr::Literal(L3Scalar::Utf8("us".into()))),
        },
        L3Expr::Compare {
            left: Box::new(L3Expr::Column(ColumnRef("value".into()))),
            op: CompareOp::Gt,
            right: Box::new(L3Expr::Literal(L3Scalar::Float64(0.0))),
        },
    ]);
    let refs = expr.columns_referenced();
    assert_eq!(refs.len(), 2);
    assert!(refs.iter().any(|r| r.0 == "region"));
    assert!(refs.iter().any(|r| r.0 == "value"));
}

#[test]
fn columns_referenced_from_not() {
    let expr = L3Expr::Not(Box::new(L3Expr::Column(ColumnRef("flag".into()))));
    let refs = expr.columns_referenced();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].0, "flag");
}

#[test]
fn columns_referenced_from_is_null() {
    let expr = L3Expr::IsNull(Box::new(L3Expr::Column(ColumnRef("x".into()))));
    let refs = expr.columns_referenced();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].0, "x");
}

#[test]
fn columns_referenced_from_is_not_null() {
    let expr = L3Expr::IsNotNull(Box::new(L3Expr::Column(ColumnRef("y".into()))));
    let refs = expr.columns_referenced();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].0, "y");
}

#[test]
fn columns_referenced_from_in_list() {
    let expr = L3Expr::InList {
        expr: Box::new(L3Expr::Column(ColumnRef("region".into()))),
        list: vec![
            L3Expr::Literal(L3Scalar::Utf8("us".into())),
            L3Expr::Literal(L3Scalar::Utf8("eu".into())),
        ],
        negated: false,
    };
    let refs = expr.columns_referenced();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].0, "region");
}

#[test]
fn columns_referenced_from_function_call() {
    let expr = L3Expr::FunctionCall {
        name: "lower".into(),
        args: vec![L3Expr::Column(ColumnRef("host".into()))],
    };
    let refs = expr.columns_referenced();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].0, "host");
}

// ── ArithOp / L3Expr::Arith ───────────────────────────────────────────────────

#[test]
fn arith_eq() {
    let make = || L3Expr::Arith {
        op: ArithOp::Mul,
        left: Box::new(L3Expr::Column(ColumnRef("value".into()))),
        right: Box::new(L3Expr::Literal(L3Scalar::Float64(2.0))),
    };
    assert_eq!(make(), make());
}

#[test]
fn arith_columns_referenced_from_both_sides() {
    let expr = L3Expr::Arith {
        op: ArithOp::Add,
        left: Box::new(L3Expr::Column(ColumnRef("a".into()))),
        right: Box::new(L3Expr::Column(ColumnRef("b".into()))),
    };
    let refs = expr.columns_referenced();
    assert_eq!(refs.len(), 2);
    assert!(refs.iter().any(|r| r.0 == "a"));
    assert!(refs.iter().any(|r| r.0 == "b"));
}

#[test]
fn arith_columns_referenced_literal_side_is_empty() {
    let expr = L3Expr::Arith {
        op: ArithOp::Mul,
        left: Box::new(L3Expr::Column(ColumnRef("value".into()))),
        right: Box::new(L3Expr::Literal(L3Scalar::Float64(2.0))),
    };
    let refs = expr.columns_referenced();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].0, "value");
}

// ── L3Expr::Case ─────────────────────────────────────────────────────────────

#[test]
fn case_searched_columns_referenced_from_all_branches() {
    // CASE WHEN a > 0 THEN b ELSE c END
    let expr = L3Expr::Case {
        operand: None,
        branches: vec![(
            L3Expr::Compare {
                left: Box::new(L3Expr::Column(ColumnRef("a".into()))),
                op: CompareOp::Gt,
                right: Box::new(L3Expr::Literal(L3Scalar::Int64(0))),
            },
            L3Expr::Column(ColumnRef("b".into())),
        )],
        else_expr: Some(Box::new(L3Expr::Column(ColumnRef("c".into())))),
    };
    let refs = expr.columns_referenced();
    assert_eq!(refs.len(), 3);
    assert!(refs.iter().any(|r| r.0 == "a"));
    assert!(refs.iter().any(|r| r.0 == "b"));
    assert!(refs.iter().any(|r| r.0 == "c"));
}

#[test]
fn case_simple_operand_included_in_refs() {
    // CASE value WHEN 1 THEN x END
    let expr = L3Expr::Case {
        operand: Some(Box::new(L3Expr::Column(ColumnRef("value".into())))),
        branches: vec![(
            L3Expr::Literal(L3Scalar::Int64(1)),
            L3Expr::Column(ColumnRef("x".into())),
        )],
        else_expr: None,
    };
    let refs = expr.columns_referenced();
    assert!(refs.iter().any(|r| r.0 == "value"));
    assert!(refs.iter().any(|r| r.0 == "x"));
}

// ── CompareOp::ILike / NotILike ───────────────────────────────────────────────

#[test]
fn compare_op_ilike_eq() {
    assert_eq!(CompareOp::ILike, CompareOp::ILike);
    assert_ne!(CompareOp::ILike, CompareOp::Like);
}

#[test]
fn compare_op_not_ilike_eq() {
    assert_eq!(CompareOp::NotILike, CompareOp::NotILike);
    assert_ne!(CompareOp::NotILike, CompareOp::NotLike);
}
