//! Structural ClickHouse syntax normalization before DataFusion type inference.
use datafusion::sql::sqlparser::ast::{
    visit_expressions, visit_expressions_mut, BinaryOperator, Expr, Function, FunctionArg,
    FunctionArgExpr, FunctionArgumentList, FunctionArguments, Ident, MapAccessSyntax, ObjectName,
    Query, SelectItem, SetExpr, Statement, VisitMut, VisitorMut,
};
use std::ops::ControlFlow;

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
                                        changed |= function.name.0.len() == 1
                                            && function.name.0[0].quote_style.is_none()
                                            && matches!(
                                                function.name.0[0]
                                                    .value
                                                    .to_ascii_lowercase()
                                                    .as_str(),
                                                "modulo"
                                                    | "map"
                                                    | "mapconcat"
                                                    | "arrayelement"
                                                    | "tupleelement"
                                            );
                                    }
                                    ControlFlow::Continue(())
                                });
                                if changed {
                                    let alias = Ident::with_quote('"', expr.to_string());
                                    let value = std::mem::replace(
                                        expr,
                                        Expr::Value(datafusion::sql::sqlparser::ast::Value::Null),
                                    );
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
        if function.name.0.len() != 1
            || function.name.0[0].quote_style.is_some()
            || !function.name.0[0].value.eq_ignore_ascii_case("modulo")
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

fn normalize_map_access(expression: &mut Expr) -> bool {
    let Expr::MapAccess { keys, .. } = expression else {
        return false;
    };
    if keys.is_empty()
        || keys
            .iter()
            .any(|key| key.syntax != MapAccessSyntax::Bracket)
    {
        return false;
    }
    let Expr::MapAccess { column, keys } = std::mem::replace(
        expression,
        Expr::Value(datafusion::sql::sqlparser::ast::Value::Null),
    ) else {
        unreachable!()
    };
    let mut input = *column;
    for key in keys {
        input = Expr::Function(Function {
            name: ObjectName(vec![Ident::new("arrayElement")]),
            parameters: FunctionArguments::None,
            args: FunctionArguments::List(FunctionArgumentList {
                duplicate_treatment: None,
                clauses: vec![],
                args: vec![
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(input)),
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(key.key)),
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
