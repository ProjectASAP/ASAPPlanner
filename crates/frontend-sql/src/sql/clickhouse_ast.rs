//! Structural ClickHouse syntax normalization before DataFusion type inference.
use datafusion::sql::sqlparser::ast::{
    visit_expressions_mut, BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArguments,
    Statement,
};
use std::ops::ControlFlow;

pub(super) fn normalize(statement: &mut Statement) {
    let _: ControlFlow<()> = visit_expressions_mut(statement, |expr| {
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
