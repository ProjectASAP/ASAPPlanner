//! Structural ClickHouse syntax normalization before DataFusion type inference.
use datafusion::sql::sqlparser::ast::{
    visit_expressions, visit_expressions_mut, AccessExpr, BinaryOperator, Expr, Function,
    FunctionArg, FunctionArgExpr, FunctionArgumentList, FunctionArguments, Ident, ObjectName,
    ObjectNamePart, Query, SelectItem, SetExpr, Statement, Subscript, Value, VisitMut, VisitorMut,
};
use std::ops::ControlFlow;

use super::collection_planning::MAP_PLANNING_NAME;

pub(super) fn normalize(statement: &mut Statement) {
    struct PreserveNames;
    impl VisitorMut for PreserveNames {
        type Break = ();
        fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<()> {
            fn preserve(body: &mut SetExpr) {
                match body {
                    SetExpr::Select(select) => {
                        for item in &mut select.projection {
                            if let SelectItem::UnnamedExpr(expr) = item {
                                let mut changed = false;
                                let _: ControlFlow<()> = visit_expressions_mut(expr, |node| {
                                    changed |= normalize_map_access(node);
                                    ControlFlow::Continue(())
                                });
                                let _: ControlFlow<()> = visit_expressions(expr, |candidate| {
                                    if let Expr::Function(function) = candidate {
                                        changed |=
                                            unquoted_name(&function.name).is_some_and(|name| {
                                                matches!(
                                                    name.to_ascii_lowercase().as_str(),
                                                    "modulo"
                                                        | "map"
                                                        | "mapconcat"
                                                        | "arrayelement"
                                                        | "tupleelement"
                                                )
                                            });
                                    }
                                    ControlFlow::Continue(())
                                });
                                if changed {
                                    let alias = Ident::with_quote('"', expr.to_string());
                                    let value =
                                        std::mem::replace(expr, Expr::Value(Value::Null.into()));
                                    *item = SelectItem::ExprWithAlias { expr: value, alias };
                                }
                            }
                        }
                    }
                    SetExpr::SetOperation { left, right, .. } => {
                        preserve(left);
                        preserve(right);
                    }
                    _ => {}
                }
            }
            preserve(&mut query.body);
            ControlFlow::Continue(())
        }
    }
    let _: ControlFlow<()> = statement.visit(&mut PreserveNames);
    let _: ControlFlow<()> = visit_expressions_mut(statement, |expr| {
        normalize_map_access(expr);
        let Expr::Function(function) = expr else {
            return ControlFlow::Continue(());
        };
        if unquoted_name(&function.name).is_some_and(|name| name.eq_ignore_ascii_case("map")) {
            function.name = ObjectName::from(vec![Ident::new(MAP_PLANNING_NAME)]);
            return ControlFlow::Continue(());
        }
        if !unquoted_name(&function.name).is_some_and(|name| name.eq_ignore_ascii_case("modulo"))
            || !matches!(function.parameters, FunctionArguments::None)
            || function.filter.is_some()
            || function.over.is_some()
            || function.null_treatment.is_some()
            || !function.within_group.is_empty()
        {
            return ControlFlow::Continue(());
        }
        let FunctionArguments::List(arguments) = &function.args else {
            return ControlFlow::Continue(());
        };
        if arguments.duplicate_treatment.is_some() || !arguments.clauses.is_empty() {
            return ControlFlow::Continue(());
        }
        let [FunctionArg::Unnamed(FunctionArgExpr::Expr(left)), FunctionArg::Unnamed(FunctionArgExpr::Expr(right))] =
            arguments.args.as_slice()
        else {
            return ControlFlow::Continue(());
        };
        *expr = Expr::BinaryOp {
            left: Box::new(left.clone()),
            op: BinaryOperator::Modulo,
            right: Box::new(right.clone()),
        };
        ControlFlow::Continue(())
    });
}

/// The name of a single-part, unquoted function name such as `modulo`.
fn unquoted_name(name: &ObjectName) -> Option<&str> {
    match name.0.as_slice() {
        [ObjectNamePart::Identifier(ident)] if ident.quote_style.is_none() => Some(&ident.value),
        _ => None,
    }
}

/// Rewrites a bracket-only access chain such as `m['k'][1]` into nested
/// `arrayElement` calls. Chains with a dot access or a slice stay unchanged.
fn normalize_map_access(expression: &mut Expr) -> bool {
    let Expr::CompoundFieldAccess { access_chain, .. } = expression else {
        return false;
    };
    if access_chain.is_empty()
        || access_chain
            .iter()
            .any(|access| !matches!(access, AccessExpr::Subscript(Subscript::Index { .. })))
    {
        return false;
    }
    let Expr::CompoundFieldAccess { root, access_chain } =
        std::mem::replace(expression, Expr::Value(Value::Null.into()))
    else {
        unreachable!()
    };
    let mut input = *root;
    for access in access_chain {
        let AccessExpr::Subscript(Subscript::Index { index }) = access else {
            unreachable!()
        };
        input = Expr::Function(Function {
            name: ObjectName::from(vec![Ident::new("arrayElement")]),
            uses_odbc_syntax: false,
            parameters: FunctionArguments::None,
            args: FunctionArguments::List(FunctionArgumentList {
                duplicate_treatment: None,
                clauses: vec![],
                args: vec![
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(input)),
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(index)),
                ],
            }),
            filter: None,
            null_treatment: None,
            over: None,
            within_group: vec![],
        });
    }
    *expression = input;
    true
}
