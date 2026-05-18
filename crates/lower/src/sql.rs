use std::rc::Rc;
use std::sync::Arc;

use datafusion::arrow::datatypes::{DataType as ArrowDataType, Field, Fields, Schema, TimeUnit};
use datafusion::common::ScalarValue;
use datafusion::datasource::MemTable;
use datafusion::logical_expr::{
    self, BinaryExpr, Distinct, Expr, LogicalPlan, Operator, WindowFunctionDefinition,
};
use datafusion::prelude::SessionContext;

use asap_control_core::intent_algebra::expr::{
    AggIntent, ColumnRef, GroupKey, L3Node, Predicate, ProjectItem, QueryExpr, SetOpKind, SortKey,
    Source, TableRef, TimeRange, WindowFuncKind,
};
use asap_control_core::intent_algebra::schema::{L3DataType, L3Schema, SchemaCatalog, TableSchema};
use asap_control_core::intent_algebra::{ArithOp, CompareOp, L3Expr, L3Scalar};
use asap_control_core::types::AccuracyTarget;

use crate::error::LoweringError;

pub struct SqlLowerer<'a> {
    catalog: &'a SchemaCatalog,
    accuracy: AccuracyTarget,
}

impl<'a> SqlLowerer<'a> {
    pub fn new(catalog: &'a SchemaCatalog, accuracy: AccuracyTarget) -> Self {
        Self { catalog, accuracy }
    }

    pub async fn lower(&self, sql: &str) -> Result<QueryExpr, LoweringError> {
        let ctx = self.build_context()?;
        let df = ctx.sql(sql).await?;
        let plan = df.into_unoptimized_plan();
        self.lower_plan(&plan)
    }

    fn build_context(&self) -> Result<SessionContext, LoweringError> {
        let ctx = SessionContext::new();
        for (name, table_schema) in &self.catalog.tables {
            let arrow_schema = Arc::new(table_schema_to_arrow(table_schema));
            let mem_table = MemTable::try_new(arrow_schema, vec![])?;
            ctx.register_table(name.as_str(), Arc::new(mem_table))?;
        }
        Ok(ctx)
    }

    fn lower_plan(&self, plan: &LogicalPlan) -> Result<QueryExpr, LoweringError> {
        match plan {
            LogicalPlan::TableScan(scan) => self.lower_table_scan(scan),
            LogicalPlan::Filter(filter) => self.lower_filter(filter),
            LogicalPlan::Projection(proj) => self.lower_projection(proj),
            LogicalPlan::Aggregate(agg) => self.lower_aggregate(agg),
            LogicalPlan::Sort(sort) => self.lower_sort(sort),
            LogicalPlan::Limit(limit) => self.lower_limit(limit),
            LogicalPlan::Window(window) => self.lower_window(window),
            LogicalPlan::Distinct(d) => {
                let input = match d {
                    Distinct::All(input) => input.as_ref(),
                    Distinct::On(on) => on.input.as_ref(),
                };
                let child = self.lower_plan(input)?;
                Ok(QueryExpr::Distinct {
                    child: make_node(child),
                    cols: vec![],
                })
            }
            LogicalPlan::Union(u) => {
                // Fold n inputs left-associatively into SetOp { Union, all: true }.
                let mut iter = u.inputs.iter();
                let first = iter
                    .next()
                    .ok_or_else(|| LoweringError::InvalidExpression("empty union".into()))?;
                let first_expr = self.lower_plan(first)?;
                iter.try_fold(first_expr, |left, right_plan| {
                    let right = self.lower_plan(right_plan)?;
                    Ok(QueryExpr::SetOp {
                        kind: SetOpKind::Union,
                        all: true,
                        left: make_node(left),
                        right: make_node(right),
                    })
                })
            }
            LogicalPlan::Join(_) => Err(LoweringError::UnsupportedFeature("JOIN".into())),
            LogicalPlan::Subquery(_) => Err(LoweringError::UnsupportedFeature("subquery".into())),
            LogicalPlan::SubqueryAlias(alias) => {
                // Simple table alias (wraps only a TableScan or another alias) is
                // transparent. A derived table (wraps Projection, Aggregate, etc.)
                // is an inline-view subquery — unsupported in v1.
                match alias.input.as_ref() {
                    LogicalPlan::TableScan(_) | LogicalPlan::SubqueryAlias(_) => {
                        self.lower_plan(&alias.input)
                    }
                    _ => Err(LoweringError::UnsupportedFeature(
                        "subquery (inline view / derived table)".into(),
                    )),
                }
            }
            other => Err(LoweringError::UnsupportedFeature(format!(
                "plan node: {}",
                other.display()
            ))),
        }
    }

    fn lower_table_scan(&self, scan: &logical_expr::TableScan) -> Result<QueryExpr, LoweringError> {
        let table_name = scan.table_name.to_string();
        let table_schema = self
            .catalog
            .tables
            .get(&table_name)
            .ok_or_else(|| LoweringError::TableNotFound(table_name.clone()))?;
        let columns = projection_columns(scan, table_schema);
        Ok(QueryExpr::Scan {
            source: Source::Table {
                table_ref: TableRef(table_name),
                columns,
                time_range: None,
            },
            predicates: vec![],
        })
    }

    fn lower_filter(&self, filter: &logical_expr::Filter) -> Result<QueryExpr, LoweringError> {
        // When the direct child is a TableScan and the table has a time column,
        // split the predicate: time bounds go into Source::Table.time_range; the
        // remaining non-time conjuncts become the Filter predicate.
        let inner = strip_aliases(&filter.input);
        if let LogicalPlan::TableScan(scan) = inner {
            let table_name = scan.table_name.to_string();
            if let Some(schema) = self.catalog.tables.get(&table_name) {
                if let Some(time_col) = &schema.time_column {
                    let (time_range, non_time) = extract_time_range(&filter.predicate, time_col);
                    let columns = projection_columns(scan, schema);
                    let scan_expr = QueryExpr::Scan {
                        source: Source::Table {
                            table_ref: TableRef(table_name),
                            columns,
                            time_range,
                        },
                        predicates: vec![],
                    };
                    return if non_time.is_empty() {
                        Ok(scan_expr)
                    } else {
                        let pred_expr = conjuncts_to_l3expr(non_time)?;
                        Ok(QueryExpr::Filter {
                            child: make_node(scan_expr),
                            pred: Predicate(pred_expr),
                        })
                    };
                }
            }
        }

        let pred_expr = df_expr_to_l3(&filter.predicate)?;
        let child = self.lower_plan(&filter.input)?;
        Ok(QueryExpr::Filter {
            child: make_node(child),
            pred: Predicate(pred_expr),
        })
    }

    fn lower_projection(
        &self,
        proj: &logical_expr::Projection,
    ) -> Result<QueryExpr, LoweringError> {
        // SELECT * — all wildcards means "no column constraint". Pass through
        // without a Project wrapper; an empty Scan.columns list means "all columns".
        if proj.expr.iter().any(|e| matches!(e, Expr::Wildcard { .. })) {
            return self.lower_plan(&proj.input);
        }

        let child = self.lower_plan(&proj.input)?;
        let cols = proj
            .expr
            .iter()
            .map(|e| match e {
                Expr::Alias(a) => df_expr_to_l3(&a.expr).map(|expr| ProjectItem {
                    expr,
                    alias: Some(a.name.clone()),
                }),
                _ => df_expr_to_l3(e).map(|expr| ProjectItem { expr, alias: None }),
            })
            .collect::<Result<Vec<_>, _>>()?;

        // DataFusion's unoptimized plan never sets TableScan.projection, so we
        // derive the Scan's column list from the enclosing projection instead.
        let child = push_columns_into_scan(child, &cols);

        Ok(QueryExpr::Project {
            child: make_node(child),
            cols,
        })
    }

    fn lower_aggregate(&self, agg: &logical_expr::Aggregate) -> Result<QueryExpr, LoweringError> {
        let child = self.lower_plan(&agg.input)?;
        let by = agg
            .group_expr
            .iter()
            .map(expr_to_group_key)
            .collect::<Result<Vec<_>, _>>()?;
        let aggs = agg
            .aggr_expr
            .iter()
            .map(|e| self.lower_agg_expr(e))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(QueryExpr::Aggregate {
            child: make_node(child),
            by,
            aggs,
            having: None,
        })
    }

    fn lower_sort(&self, sort: &logical_expr::Sort) -> Result<QueryExpr, LoweringError> {
        // TopK: Sort with a constant LIMIT folded in, all keys descending, on an Aggregate.
        // Note: Sort.fetch is Option<usize>; Limit.fetch is Option<Box<Expr>>.
        if let Some(k) = sort.fetch {
            if sort.expr.iter().all(|s| !s.asc) {
                if let Some(agg) = find_aggregate(strip_projections_and_aliases(&sort.input)) {
                    return self.lower_as_topk(agg, k);
                }
            }
        }
        let child = self.lower_plan(&sort.input)?;
        let keys = sort
            .expr
            .iter()
            .map(|s| {
                df_expr_to_l3(&s.expr).map(|expr| SortKey {
                    expr,
                    ascending: s.asc,
                    nulls_first: s.nulls_first,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(QueryExpr::Sort {
            child: make_node(child),
            keys,
        })
    }

    fn lower_limit(&self, limit: &logical_expr::Limit) -> Result<QueryExpr, LoweringError> {
        // TopK: Limit on top of Sort on top of Aggregate, all sort keys DESC.
        if let Some(k) = eval_fetch(&limit.fetch) {
            let inner = strip_aliases(&limit.input);
            if let LogicalPlan::Sort(sort) = inner {
                if sort.expr.iter().all(|s| !s.asc) {
                    if let Some(agg) = find_aggregate(strip_projections_and_aliases(&sort.input)) {
                        return self.lower_as_topk(agg, k);
                    }
                }
            }
        }
        let child = self.lower_plan(&limit.input)?;
        Ok(QueryExpr::Limit {
            child: make_node(child),
            n: eval_fetch(&limit.fetch).unwrap_or(0) as u64,
            offset: eval_fetch(&limit.skip).unwrap_or(0) as u64,
        })
    }

    fn lower_as_topk(
        &self,
        agg: &logical_expr::Aggregate,
        k: usize,
    ) -> Result<QueryExpr, LoweringError> {
        let child = self.lower_plan(&agg.input)?;
        let by = agg
            .group_expr
            .iter()
            .map(expr_to_col_ref)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(QueryExpr::Aggregate {
            child: make_node(child),
            by: vec![],
            aggs: vec![AggIntent::TopK {
                k,
                by,
                accuracy: self.accuracy.clone(),
            }],
            having: None,
        })
    }

    fn lower_window(&self, window: &logical_expr::Window) -> Result<QueryExpr, LoweringError> {
        let child = self.lower_plan(&window.input)?;
        let first = window
            .window_expr
            .first()
            .ok_or_else(|| LoweringError::InvalidExpression("empty window expressions".into()))?;
        if let Expr::WindowFunction(wf) = first {
            let func = lower_window_func_kind(&wf.fun)?;
            let args = wf
                .args
                .iter()
                .map(df_expr_to_l3)
                .collect::<Result<Vec<_>, _>>()?;
            let partition_by = wf
                .partition_by
                .iter()
                .map(expr_to_group_key)
                .collect::<Result<Vec<_>, _>>()?;
            // In DataFusion 43, WindowFunction.order_by is Vec<SortExpr>.
            let order_by = wf
                .order_by
                .iter()
                .map(|s| {
                    df_expr_to_l3(&s.expr).map(|expr| SortKey {
                        expr,
                        ascending: s.asc,
                        nulls_first: s.nulls_first,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(QueryExpr::WindowFunc {
                child: make_node(child),
                func,
                args,
                partition_by,
                order_by,
                frame: None,
            });
        }
        Err(LoweringError::InvalidExpression(
            "expected WindowFunction expr".into(),
        ))
    }

    fn lower_agg_expr(&self, expr: &Expr) -> Result<AggIntent, LoweringError> {
        match expr {
            Expr::AggregateFunction(agg_fn) => {
                let name = agg_fn.func.name().to_lowercase();
                match name.as_str() {
                    "count" if agg_fn.distinct => Ok(AggIntent::Cardinality {
                        accuracy: self.accuracy.clone(),
                    }),
                    "count" => Ok(AggIntent::Count {
                        accuracy: self.accuracy.clone(),
                    }),
                    "sum" => Ok(AggIntent::Sum),
                    "min" => Ok(AggIntent::Min),
                    "max" => Ok(AggIntent::Max),
                    "avg" | "mean" => Ok(AggIntent::Avg),
                    "stddev" | "stddev_samp" => Ok(AggIntent::Stddev { population: false }),
                    "stddev_pop" => Ok(AggIntent::Stddev { population: true }),
                    "approx_percentile_cont" | "percentile_cont" => {
                        let q = extract_percentile_q(&agg_fn.args)?;
                        Ok(AggIntent::Quantile {
                            q,
                            accuracy: self.accuracy.clone(),
                        })
                    }
                    "approx_distinct" => Ok(AggIntent::Cardinality {
                        accuracy: self.accuracy.clone(),
                    }),
                    _ => Err(LoweringError::UnsupportedAggregate(name)),
                }
            }
            Expr::Alias(alias) => self.lower_agg_expr(&alias.expr),
            _ => Err(LoweringError::UnsupportedAggregate(format!("{expr:?}"))),
        }
    }
}

// ── Free helpers ──────────────────────────────────────────────────────────────

/// Map a DataFusion `TableScan.projection` (column index list) back to
/// `ColumnRef` names from the catalog schema.
/// Returns an empty `Vec` when the projection is absent (full scan / `SELECT *`).
fn projection_columns(scan: &logical_expr::TableScan, schema: &TableSchema) -> Vec<ColumnRef> {
    match &scan.projection {
        Some(indices) => indices
            .iter()
            .filter_map(|&i| schema.columns.get(i))
            .map(|c| ColumnRef(c.name.clone()))
            .collect(),
        None => vec![],
    }
}

/// If `child` is a `Scan` with an empty column list, populate it from the
/// columns referenced in `cols`. DataFusion's unoptimized plan never sets
/// `TableScan.projection`, so this compensates without requiring optimizer
/// passes that could alter other plan-node shapes our lowerer depends on.
fn push_columns_into_scan(child: QueryExpr, cols: &[ProjectItem]) -> QueryExpr {
    let QueryExpr::Scan {
        source:
            Source::Table {
                table_ref,
                columns,
                time_range,
            },
        predicates,
    } = child
    else {
        return child;
    };
    if !columns.is_empty() {
        return QueryExpr::Scan {
            source: Source::Table {
                table_ref,
                columns,
                time_range,
            },
            predicates,
        };
    }
    let mut seen = std::collections::HashSet::<String>::new();
    let col_refs: Vec<ColumnRef> = cols
        .iter()
        .flat_map(|item| item.expr.columns_referenced())
        .filter(|&c| seen.insert(c.0.clone()))
        .cloned()
        .collect();
    QueryExpr::Scan {
        source: Source::Table {
            table_ref,
            columns: col_refs,
            time_range,
        },
        predicates,
    }
}

fn make_node(expr: QueryExpr) -> Rc<L3Node> {
    Rc::new(L3Node {
        expr,
        schema: L3Schema {
            fields: vec![],
            time_index: None,
        },
    })
}

/// Evaluate a constant fetch/skip expression to a `usize`.
/// Returns `None` for parametric (non-literal) fetch expressions.
fn eval_fetch(expr_opt: &Option<Box<Expr>>) -> Option<usize> {
    expr_opt.as_ref().and_then(|e| match e.as_ref() {
        Expr::Literal(ScalarValue::Int64(Some(v))) => Some(*v as usize),
        Expr::Literal(ScalarValue::UInt64(Some(v))) => Some(*v as usize),
        Expr::Literal(ScalarValue::Int32(Some(v))) => Some(*v as usize),
        _ => None,
    })
}

fn strip_aliases(plan: &LogicalPlan) -> &LogicalPlan {
    match plan {
        LogicalPlan::SubqueryAlias(a) => strip_aliases(&a.input),
        _ => plan,
    }
}

/// Strip Projection and SubqueryAlias for TopK pattern-matching only.
/// Do NOT use when building the output tree.
fn strip_projections_and_aliases(plan: &LogicalPlan) -> &LogicalPlan {
    match plan {
        LogicalPlan::SubqueryAlias(a) => strip_projections_and_aliases(&a.input),
        LogicalPlan::Projection(p) => strip_projections_and_aliases(&p.input),
        _ => plan,
    }
}

fn find_aggregate(plan: &LogicalPlan) -> Option<&logical_expr::Aggregate> {
    match plan {
        LogicalPlan::Aggregate(agg) => Some(agg),
        LogicalPlan::Projection(p) => find_aggregate(&p.input),
        LogicalPlan::SubqueryAlias(a) => find_aggregate(&a.input),
        _ => None,
    }
}

fn expr_to_group_key(expr: &Expr) -> Result<GroupKey, LoweringError> {
    match expr {
        Expr::Column(col) => Ok(GroupKey(col.name.clone())),
        Expr::Alias(a) => expr_to_group_key(&a.expr),
        _ => Ok(GroupKey(format!("{expr}"))),
    }
}

fn expr_to_col_ref(expr: &Expr) -> Result<ColumnRef, LoweringError> {
    match expr {
        Expr::Column(col) => Ok(ColumnRef(col.name.clone())),
        Expr::Alias(a) => expr_to_col_ref(&a.expr),
        _ => Ok(ColumnRef(format!("{expr}"))),
    }
}

fn extract_percentile_q(args: &[Expr]) -> Result<f64, LoweringError> {
    let val = args.get(1).ok_or_else(|| {
        LoweringError::InvalidExpression("percentile requires 2 arguments".into())
    })?;
    match val {
        Expr::Literal(ScalarValue::Float64(Some(q))) => Ok(*q),
        Expr::Literal(ScalarValue::Float32(Some(q))) => Ok(*q as f64),
        _ => Err(LoweringError::InvalidExpression(
            "percentile value must be a float literal".into(),
        )),
    }
}

fn split_conjuncts(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::BinaryExpr(BinaryExpr {
            left,
            op: Operator::And,
            right,
        }) => {
            let mut v = split_conjuncts(left);
            v.extend(split_conjuncts(right));
            v
        }
        _ => vec![expr],
    }
}

/// Split `expr` into `(time_range, non_time_conjuncts)`.
/// Time-bound conjuncts are folded into the `TimeRange`; the rest are returned
/// as a `Vec<&Expr>` so the caller can translate them with `df_expr_to_l3`.
pub(crate) fn extract_time_range<'a>(
    expr: &'a Expr,
    time_col: &str,
) -> (Option<TimeRange>, Vec<&'a Expr>) {
    let conjuncts = split_conjuncts(expr);
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

/// Translate a slice of DataFusion `Expr`s (non-time conjuncts) into a single
/// `L3Expr`. A single element is returned as-is; multiple elements are wrapped
/// in `L3Expr::BoolAnd`.
fn conjuncts_to_l3expr(conjuncts: Vec<&Expr>) -> Result<L3Expr, LoweringError> {
    let parts: Result<Vec<_>, _> = conjuncts.iter().map(|e| df_expr_to_l3(e)).collect();
    let mut parts = parts?;
    if parts.len() == 1 {
        Ok(parts.remove(0))
    } else {
        Ok(L3Expr::BoolAnd(parts))
    }
}

/// Translate a DataFusion `Expr` to an `L3Expr`.
/// Returns `UnsupportedFeature` for anything not needed in v1.
fn df_expr_to_l3(expr: &Expr) -> Result<L3Expr, LoweringError> {
    match expr {
        Expr::Column(col) => Ok(L3Expr::Column(ColumnRef(col.name.clone()))),

        Expr::Literal(sv) => scalar_value_to_l3(sv).map(L3Expr::Literal),

        Expr::Alias(a) => df_expr_to_l3(&a.expr),

        Expr::BinaryExpr(BinaryExpr { left, op, right }) => match op {
            Operator::And => {
                let parts = split_conjuncts(expr);
                let l3_parts: Result<Vec<_>, _> = parts.iter().map(|e| df_expr_to_l3(e)).collect();
                Ok(L3Expr::BoolAnd(l3_parts?))
            }
            Operator::Or => {
                let parts = split_disjuncts(expr);
                let l3_parts: Result<Vec<_>, _> = parts.iter().map(|e| df_expr_to_l3(e)).collect();
                Ok(L3Expr::BoolOr(l3_parts?))
            }
            Operator::Eq => compare(left, CompareOp::Eq, right),
            Operator::NotEq => compare(left, CompareOp::Ne, right),
            Operator::Lt => compare(left, CompareOp::Lt, right),
            Operator::LtEq => compare(left, CompareOp::Le, right),
            Operator::Gt => compare(left, CompareOp::Gt, right),
            Operator::GtEq => compare(left, CompareOp::Ge, right),
            // BinaryExpr LIKE/ILIKE operators (from optimizer rewrites)
            Operator::LikeMatch => compare(left, CompareOp::Like, right),
            Operator::ILikeMatch => compare(left, CompareOp::ILike, right),
            Operator::NotLikeMatch => compare(left, CompareOp::NotLike, right),
            Operator::NotILikeMatch => compare(left, CompareOp::NotILike, right),
            // Arithmetic
            Operator::Plus => arith(left, ArithOp::Add, right),
            Operator::Minus => arith(left, ArithOp::Sub, right),
            Operator::Multiply => arith(left, ArithOp::Mul, right),
            Operator::Divide => arith(left, ArithOp::Div, right),
            Operator::Modulo => arith(left, ArithOp::Mod, right),
            other => Err(LoweringError::UnsupportedFeature(format!(
                "operator: {other:?}"
            ))),
        },

        // SQL LIKE / ILIKE (dedicated expr node from the SQL parser)
        Expr::Like(like) => {
            let op = match (like.negated, like.case_insensitive) {
                (false, false) => CompareOp::Like,
                (true, false) => CompareOp::NotLike,
                (false, true) => CompareOp::ILike,
                (true, true) => CompareOp::NotILike,
            };
            compare(&like.expr, op, &like.pattern)
        }

        // Unary minus: negate literals directly; wrap others in -1 * x.
        Expr::Negative(inner) => {
            let inner_l3 = df_expr_to_l3(inner)?;
            match inner_l3 {
                L3Expr::Literal(L3Scalar::Int64(v)) => Ok(L3Expr::Literal(L3Scalar::Int64(-v))),
                L3Expr::Literal(L3Scalar::Float64(v)) => Ok(L3Expr::Literal(L3Scalar::Float64(-v))),
                other => Ok(L3Expr::Arith {
                    op: ArithOp::Mul,
                    left: Box::new(L3Expr::Literal(L3Scalar::Int64(-1))),
                    right: Box::new(other),
                }),
            }
        }

        // SQL CASE expression
        Expr::Case(c) => {
            let operand = c
                .expr
                .as_ref()
                .map(|e| df_expr_to_l3(e).map(Box::new))
                .transpose()?;
            let branches = c
                .when_then_expr
                .iter()
                .map(|(when, then)| Ok((df_expr_to_l3(when)?, df_expr_to_l3(then)?)))
                .collect::<Result<Vec<_>, LoweringError>>()?;
            let else_expr = c
                .else_expr
                .as_ref()
                .map(|e| df_expr_to_l3(e).map(Box::new))
                .transpose()?;
            Ok(L3Expr::Case {
                operand,
                branches,
                else_expr,
            })
        }

        Expr::Not(inner) => Ok(L3Expr::Not(Box::new(df_expr_to_l3(inner)?))),

        Expr::IsNull(inner) => Ok(L3Expr::IsNull(Box::new(df_expr_to_l3(inner)?))),

        Expr::IsNotNull(inner) => Ok(L3Expr::IsNotNull(Box::new(df_expr_to_l3(inner)?))),

        Expr::Cast(c) => {
            let inner = df_expr_to_l3(&c.expr)?;
            let to = arrow_to_l3(&c.data_type)?;
            Ok(L3Expr::Cast {
                expr: Box::new(inner),
                to,
            })
        }

        Expr::TryCast(c) => {
            let inner = df_expr_to_l3(&c.expr)?;
            let to = arrow_to_l3(&c.data_type)?;
            Ok(L3Expr::Cast {
                expr: Box::new(inner),
                to,
            })
        }

        Expr::InList(il) => {
            let expr = df_expr_to_l3(&il.expr)?;
            let list: Result<Vec<_>, _> = il.list.iter().map(df_expr_to_l3).collect();
            Ok(L3Expr::InList {
                expr: Box::new(expr),
                list: list?,
                negated: il.negated,
            })
        }

        Expr::Between(b) => {
            // Normalize: `x BETWEEN low AND high` → `x >= low AND x <= high`.
            // `x NOT BETWEEN low AND high` → `x < low OR x > high`.
            let x_low = compare(&b.expr, CompareOp::Ge, &b.low)?;
            let x_high = compare(&b.expr, CompareOp::Le, &b.high)?;
            if b.negated {
                // NOT BETWEEN: invert each side
                let lt = compare(&b.expr, CompareOp::Lt, &b.low)?;
                let gt = compare(&b.expr, CompareOp::Gt, &b.high)?;
                Ok(L3Expr::BoolOr(vec![lt, gt]))
            } else {
                Ok(L3Expr::BoolAnd(vec![x_low, x_high]))
            }
        }

        Expr::ScalarFunction(sf) => {
            let args: Result<Vec<_>, _> = sf.args.iter().map(df_expr_to_l3).collect();
            Ok(L3Expr::FunctionCall {
                name: sf.func.name().to_string(),
                args: args?,
            })
        }

        other => Err(LoweringError::UnsupportedFeature(format!(
            "expression: {}",
            other
        ))),
    }
}

fn compare(left: &Expr, op: CompareOp, right: &Expr) -> Result<L3Expr, LoweringError> {
    Ok(L3Expr::Compare {
        left: Box::new(df_expr_to_l3(left)?),
        op,
        right: Box::new(df_expr_to_l3(right)?),
    })
}

fn arith(left: &Expr, op: ArithOp, right: &Expr) -> Result<L3Expr, LoweringError> {
    Ok(L3Expr::Arith {
        op,
        left: Box::new(df_expr_to_l3(left)?),
        right: Box::new(df_expr_to_l3(right)?),
    })
}

fn scalar_value_to_l3(sv: &ScalarValue) -> Result<L3Scalar, LoweringError> {
    match sv {
        ScalarValue::Int64(Some(v)) => Ok(L3Scalar::Int64(*v)),
        ScalarValue::Int32(Some(v)) => Ok(L3Scalar::Int64(*v as i64)),
        ScalarValue::Int16(Some(v)) => Ok(L3Scalar::Int64(*v as i64)),
        ScalarValue::Int8(Some(v)) => Ok(L3Scalar::Int64(*v as i64)),
        ScalarValue::UInt64(Some(v)) => Ok(L3Scalar::Int64(*v as i64)),
        ScalarValue::UInt32(Some(v)) => Ok(L3Scalar::Int64(*v as i64)),
        ScalarValue::Float64(Some(v)) => Ok(L3Scalar::Float64(*v)),
        ScalarValue::Float32(Some(v)) => Ok(L3Scalar::Float64(*v as f64)),
        ScalarValue::Utf8(Some(s)) | ScalarValue::LargeUtf8(Some(s)) => {
            Ok(L3Scalar::Utf8(s.clone()))
        }
        ScalarValue::Boolean(Some(b)) => Ok(L3Scalar::Boolean(*b)),
        // Typed nulls and untyped null both become L3Scalar::Null
        _ if sv.is_null() => Ok(L3Scalar::Null),
        _ => Err(LoweringError::InvalidExpression(format!(
            "unsupported scalar: {sv:?}"
        ))),
    }
}

fn arrow_to_l3(dt: &ArrowDataType) -> Result<L3DataType, LoweringError> {
    match dt {
        ArrowDataType::Int64
        | ArrowDataType::Int32
        | ArrowDataType::Int16
        | ArrowDataType::Int8 => Ok(L3DataType::Int64),
        ArrowDataType::Float64 | ArrowDataType::Float32 => Ok(L3DataType::Float64),
        ArrowDataType::Utf8 | ArrowDataType::LargeUtf8 => Ok(L3DataType::Utf8),
        ArrowDataType::Boolean => Ok(L3DataType::Boolean),
        ArrowDataType::Timestamp(_, _) => Ok(L3DataType::Timestamp),
        ArrowDataType::Duration(_) => Ok(L3DataType::Duration),
        other => Err(LoweringError::UnsupportedFeature(format!(
            "Arrow type in cast: {other:?}"
        ))),
    }
}

fn split_disjuncts(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::BinaryExpr(BinaryExpr {
            left,
            op: Operator::Or,
            right,
        }) => {
            let mut v = split_disjuncts(left);
            v.extend(split_disjuncts(right));
            v
        }
        _ => vec![expr],
    }
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

fn scalar_to_ms(sv: &ScalarValue) -> Option<i64> {
    match sv {
        ScalarValue::Int64(Some(v)) => Some(*v),
        ScalarValue::Int32(Some(v)) => Some(*v as i64),
        ScalarValue::TimestampMillisecond(Some(ms), _) => Some(*ms),
        ScalarValue::TimestampNanosecond(Some(ns), _) => Some(*ns / 1_000_000),
        ScalarValue::TimestampMicrosecond(Some(us), _) => Some(*us / 1_000),
        ScalarValue::TimestampSecond(Some(s), _) => Some(*s * 1_000),
        _ => None,
    }
}

fn table_schema_to_arrow(schema: &TableSchema) -> Schema {
    let fields: Fields = schema
        .columns
        .iter()
        .map(|c| Field::new(&c.name, l3_to_arrow(&c.data_type), c.nullable))
        .collect();
    Schema::new(fields)
}

fn l3_to_arrow(dt: &L3DataType) -> ArrowDataType {
    match dt {
        L3DataType::Int64 => ArrowDataType::Int64,
        L3DataType::Float64 => ArrowDataType::Float64,
        L3DataType::Utf8 => ArrowDataType::Utf8,
        L3DataType::Boolean => ArrowDataType::Boolean,
        L3DataType::Timestamp => ArrowDataType::Timestamp(TimeUnit::Millisecond, None),
        L3DataType::Duration => ArrowDataType::Duration(TimeUnit::Millisecond),
        L3DataType::Map(k, v) => ArrowDataType::Map(
            Arc::new(Field::new(
                "entries",
                ArrowDataType::Struct(Fields::from(vec![
                    Field::new("key", l3_to_arrow(k), false),
                    Field::new("value", l3_to_arrow(v), true),
                ])),
                false,
            )),
            false,
        ),
        L3DataType::List(item) => {
            ArrowDataType::List(Arc::new(Field::new("item", l3_to_arrow(item), true)))
        }
    }
}

fn lower_window_func_kind(fun: &WindowFunctionDefinition) -> Result<WindowFuncKind, LoweringError> {
    match fun {
        // In DataFusion 43 most ranking/nav window functions are WindowUDF.
        WindowFunctionDefinition::WindowUDF(udf) => match udf.name().to_lowercase().as_str() {
            "row_number" => Ok(WindowFuncKind::RowNumber),
            "rank" => Ok(WindowFuncKind::Rank),
            "dense_rank" => Ok(WindowFuncKind::DenseRank),
            "lag" => Ok(WindowFuncKind::Lag),
            "lead" => Ok(WindowFuncKind::Lead),
            "first_value" => Ok(WindowFuncKind::FirstValue),
            "last_value" => Ok(WindowFuncKind::LastValue),
            "nth_value" => Ok(WindowFuncKind::NthValue(0)),
            other => Err(LoweringError::UnsupportedFeature(format!(
                "window fn: {other}"
            ))),
        },
        WindowFunctionDefinition::AggregateUDF(udf) => match udf.name().to_lowercase().as_str() {
            "sum" => Ok(WindowFuncKind::Sum),
            "avg" | "mean" => Ok(WindowFuncKind::Avg),
            "count" => Ok(WindowFuncKind::Count),
            "min" => Ok(WindowFuncKind::Min),
            "max" => Ok(WindowFuncKind::Max),
            other => Err(LoweringError::UnsupportedFeature(format!(
                "window agg: {other}"
            ))),
        },
        WindowFunctionDefinition::BuiltInWindowFunction(biwf) => Err(
            LoweringError::UnsupportedFeature(format!("built-in window fn: {biwf:?}")),
        ),
    }
}
