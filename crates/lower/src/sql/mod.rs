use std::sync::Arc;

use datafusion::common::ScalarValue;
use datafusion::datasource::MemTable;
use datafusion::logical_expr::{self, Distinct, Expr, LogicalPlan, WindowFunctionDefinition};
use datafusion::prelude::SessionContext;

use asap_control_core::intent_algebra::expr::{
    AggIntent, ColumnRef, GroupKey, L3Node, Predicate, ProjectItem, QueryExpr, SetOpKind, SortKey,
    Source, TableRef, WindowFuncKind,
};
use asap_control_core::intent_algebra::schema::{L3Schema, SchemaCatalog, TableSchema};
use asap_control_core::intent_algebra::{L3Expr, L3Scalar};
use asap_control_core::types::AccuracyTarget;

use crate::error::LoweringError;

mod expr;
mod time;
mod types;

use self::expr::{conjuncts_to_l3expr, df_expr_to_l3, split_conjuncts};
use self::time::extract_time_range_from_conjuncts;
use self::types::table_schema_to_arrow;

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
            LogicalPlan::Distinct(d) => match d {
                Distinct::On(_) => Err(LoweringError::UnsupportedFeature("DISTINCT ON".into())),
                Distinct::All(input) => {
                    let child = self.lower_plan(input)?;
                    Ok(QueryExpr::Distinct {
                        child: make_untyped_node(child),
                        cols: vec![],
                    })
                }
            },
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
                        left: make_untyped_node(left),
                        right: make_untyped_node(right),
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
        // Walk the full filter chain to find a TableScan at any depth, then
        // collect predicates from all stacked filters (including the outermost).
        let (inner_preds, maybe_scan) = collect_filter_chain(&filter.input);

        if let Some(scan) = maybe_scan {
            let table_name = scan.table_name.to_string();
            if let Some(schema) = self.catalog.tables.get(&table_name) {
                if let Some(time_col) = &schema.time_column {
                    if let Err(e) = schema.validate() {
                        return Err(LoweringError::InvalidExpression(format!(
                            "catalog table '{table_name}': {e}"
                        )));
                    }
                    // Merge outermost predicate + all inner filter predicates into one
                    // flat conjunct list, then classify for time-range extraction.
                    let all_conjuncts: Vec<&Expr> = std::iter::once(&filter.predicate)
                        .chain(inner_preds)
                        .flat_map(|p| split_conjuncts(p))
                        .collect();
                    let (time_range, non_time) =
                        extract_time_range_from_conjuncts(all_conjuncts, time_col);
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
                            child: make_untyped_node(scan_expr),
                            pred: Predicate(pred_expr),
                        })
                    };
                }
            }
        }

        let pred_expr = df_expr_to_l3(&filter.predicate)?;
        let child = self.lower_plan(&filter.input)?;
        Ok(QueryExpr::Filter {
            child: make_untyped_node(child),
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
            child: make_untyped_node(child),
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
        // Use DataFusion's own aggregate output schema for column names — the same
        // schema the enclosing Projection was built against when it wrote its column
        // references (e.g. "MIN(metrics.ts)"). The first n_groups fields are the
        // GROUP BY columns; the remaining fields are the aggregate outputs.
        // TODO: output_names couples core's Aggregate IR to DataFusion's internal
        // naming convention. Cleaner boundary: emit a Project on top of every
        // Aggregate that renames DataFusion's names to user-visible aliases, so
        // Aggregate.output_names can be removed and column resolution lives in Project.
        let n_groups = agg.group_expr.len();
        let output_names: Vec<String> = agg
            .schema
            .fields()
            .iter()
            .skip(n_groups)
            .take(agg.aggr_expr.len())
            .map(|f| f.name().to_string())
            .collect();
        Ok(QueryExpr::Aggregate {
            child: make_untyped_node(child),
            by,
            aggs,
            having: None,
            output_names,
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
            child: make_untyped_node(child),
            keys,
        })
    }

    fn lower_limit(&self, limit: &logical_expr::Limit) -> Result<QueryExpr, LoweringError> {
        // TopK: Limit on top of Sort on top of Aggregate, all sort keys DESC.
        if let Some(k) = eval_fetch(&limit.fetch) {
            if eval_fetch(&limit.skip).unwrap_or(0) > 0 {
                return Err(LoweringError::UnsupportedFeature(
                    "LIMIT ... OFFSET is not supported with ORDER BY ... DESC aggregates (TopK)"
                        .into(),
                ));
            }
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
            child: make_untyped_node(child),
            n: eval_fetch(&limit.fetch).map(|v| v as u64),
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
            child: make_untyped_node(child),
            by: vec![],
            aggs: vec![AggIntent::TopK {
                k,
                by,
                accuracy: self.accuracy.clone(),
            }],
            having: None,
            output_names: vec![],
        })
    }

    fn lower_window(&self, window: &logical_expr::Window) -> Result<QueryExpr, LoweringError> {
        if window.window_expr.len() > 1 {
            return Err(LoweringError::UnsupportedFeature(format!(
                "multiple window functions in one Window plan node (got {}); split into separate nodes",
                window.window_expr.len()
            )));
        }
        let child = self.lower_plan(&window.input)?;
        let first = window
            .window_expr
            .first()
            .ok_or_else(|| LoweringError::InvalidExpression("empty window expressions".into()))?;
        if let Expr::WindowFunction(wf) = first {
            let func = lower_window_func_kind(&wf.fun)?;
            let mut args = wf
                .args
                .iter()
                .map(df_expr_to_l3)
                .collect::<Result<Vec<_>, _>>()?;

            // For NthValue, extract N from args[1] and keep only the column (args[0]).
            let func = if matches!(func, WindowFuncKind::NthValue(None)) {
                let n = match args.get(1) {
                    Some(L3Expr::Literal(L3Scalar::Int64(n))) if *n > 0 => *n as u64,
                    other => {
                        return Err(LoweringError::InvalidExpression(format!(
                            "NthValue requires a positive integer literal as second arg, got: {other:?}"
                        )))
                    }
                };
                args.truncate(1);
                WindowFuncKind::NthValue(Some(n))
            } else {
                func
            };
            debug_assert!(
                !matches!(func, WindowFuncKind::NthValue(None)),
                "NthValue sentinel not resolved; lower_window has a bug"
            );

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
                child: make_untyped_node(child),
                func,
                args,
                partition_by,
                order_by,
                frame: None,
            });
        }
        Err(LoweringError::UnsupportedFeature(
            "unexpected non-WindowFunction expr in Window plan node".into(),
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
                    "sum" => Ok(AggIntent::Sum {
                        col: agg_col(&agg_fn.args),
                    }),
                    "min" => Ok(AggIntent::Min {
                        col: agg_col(&agg_fn.args),
                    }),
                    "max" => Ok(AggIntent::Max {
                        col: agg_col(&agg_fn.args),
                    }),
                    "avg" | "mean" => Ok(AggIntent::Avg {
                        col: agg_col(&agg_fn.args),
                    }),
                    "stddev" | "stddev_samp" => Ok(AggIntent::Stddev {
                        col: agg_col(&agg_fn.args),
                        population: false,
                    }),
                    "stddev_pop" => Ok(AggIntent::Stddev {
                        col: agg_col(&agg_fn.args),
                        population: true,
                    }),
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

/// If `child` (or a Filter wrapping it) contains a `Scan` with an empty
/// column list, populate it from the columns referenced in `cols`.
/// DataFusion's unoptimized plan never sets `TableScan.projection`, so this
/// compensates without requiring optimizer passes that could alter other
/// plan-node shapes our lowerer depends on.
///
/// Handled topologies: `Project → Scan` and `Project → Filter → Scan`.
///
/// **Known gap**: `Project → Aggregate → * → Scan` is NOT handled. Aggregate
/// lowering does not call this function, so `Scan.columns` stays empty in any
/// topology where an Aggregate sits between the Project and the Scan. Any
/// downstream stage that uses `Scan.columns` for pruning or cost estimation
/// will see an unconstrained (full) scan in those cases. TODO: propagate
/// column refs through the Aggregate child when implementing column-pruning.
fn push_columns_into_scan(child: QueryExpr, cols: &[ProjectItem]) -> QueryExpr {
    match child {
        // Recurse through Filter so that Project → Filter → Scan works.
        QueryExpr::Filter { child: inner, pred } => {
            let updated = push_columns_into_scan(inner.expr.clone(), cols);
            QueryExpr::Filter {
                child: Arc::new(L3Node {
                    expr: updated,
                    schema: inner.schema.clone(),
                }),
                pred,
            }
        }
        QueryExpr::Scan {
            source:
                Source::Table {
                    table_ref,
                    columns,
                    time_range,
                },
            predicates,
        } if columns.is_empty() => {
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
        other => other,
    }
}

fn make_untyped_node(expr: QueryExpr) -> Arc<L3Node> {
    Arc::new(L3Node {
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
        Expr::Literal(ScalarValue::Int64(Some(v))) if *v >= 0 => Some(*v as usize),
        Expr::Literal(ScalarValue::UInt64(Some(v))) => Some(*v as usize),
        Expr::Literal(ScalarValue::Int32(Some(v))) if *v >= 0 => Some(*v as usize),
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
        other => Err(LoweringError::UnsupportedFeature(format!(
            "non-column GROUP BY expression: {other}"
        ))),
    }
}

fn expr_to_col_ref(expr: &Expr) -> Result<ColumnRef, LoweringError> {
    match expr {
        Expr::Column(col) => Ok(ColumnRef(col.name.clone())),
        Expr::Alias(a) => expr_to_col_ref(&a.expr),
        other => Err(LoweringError::UnsupportedFeature(format!(
            "non-column reference in TopK by-list: {other}"
        ))),
    }
}

/// Extract the aggregated column name from aggregate function args.
/// Returns `None` for wildcards (`COUNT(*)`) and non-column expressions.
fn agg_col(args: &[Expr]) -> Option<ColumnRef> {
    match args.first() {
        Some(Expr::Column(col)) => Some(ColumnRef(col.name.clone())),
        Some(Expr::Alias(a)) => match a.expr.as_ref() {
            Expr::Column(col) => Some(ColumnRef(col.name.clone())),
            _ => None,
        },
        Some(Expr::Cast(c)) => match c.expr.as_ref() {
            Expr::Column(col) => Some(ColumnRef(col.name.clone())),
            _ => None,
        },
        Some(Expr::Wildcard { .. }) | None => None,
        _ => None,
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

/// Walk a `Filter(Filter(...(TableScan)))` chain.
/// Returns `(predicates_from_inner_filters, Some(scan))` when a TableScan is
/// found at any depth, or `(vec![], None)` if a non-Filter non-Scan node is
/// reached first. The outermost filter's predicate is NOT included — the
/// caller adds it.
fn collect_filter_chain(plan: &LogicalPlan) -> (Vec<&Expr>, Option<&logical_expr::TableScan>) {
    let plan = strip_aliases(plan);
    match plan {
        LogicalPlan::TableScan(scan) => (vec![], Some(scan)),
        LogicalPlan::Filter(f) => {
            let (mut inner_preds, maybe_scan) = collect_filter_chain(&f.input);
            if maybe_scan.is_some() {
                inner_preds.push(&f.predicate);
            }
            (inner_preds, maybe_scan)
        }
        _ => (vec![], None),
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
            // NthValue(None) is a sentinel; lower_window extracts the real N from args.
            "nth_value" => Ok(WindowFuncKind::NthValue(None)),
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
        // In DataFusion 43, BuiltInWindowFunction covers FirstValue, LastValue, NthValue.
        // NthValue(None) is a sentinel; the real N is extracted from args in lower_window.
        WindowFunctionDefinition::BuiltInWindowFunction(biwf) => {
            use datafusion::logical_expr::BuiltInWindowFunction;
            match biwf {
                BuiltInWindowFunction::FirstValue => Ok(WindowFuncKind::FirstValue),
                BuiltInWindowFunction::LastValue => Ok(WindowFuncKind::LastValue),
                BuiltInWindowFunction::NthValue => Ok(WindowFuncKind::NthValue(None)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── collect_filter_chain unit tests (Fix 3) ───────────────────────────────

    fn empty_scan() -> LogicalPlan {
        use datafusion::arrow::datatypes::{DataType, Field, Schema};
        use datafusion::logical_expr::builder::LogicalTableSource;
        use datafusion::logical_expr::LogicalPlanBuilder;

        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let source = Arc::new(LogicalTableSource::new(schema));
        LogicalPlanBuilder::scan("t", source, None)
            .unwrap()
            .build()
            .unwrap()
    }

    #[test]
    fn collect_filter_chain_finds_scan_at_depth_zero() {
        let scan = empty_scan();
        let (preds, maybe_scan) = collect_filter_chain(&scan);
        assert!(maybe_scan.is_some(), "should find the TableScan");
        assert!(preds.is_empty(), "no inner predicates at depth zero");
    }

    #[test]
    fn collect_filter_chain_returns_none_for_non_scan() {
        use datafusion::common::DFSchema;
        use datafusion::logical_expr::EmptyRelation;

        let empty = LogicalPlan::EmptyRelation(EmptyRelation {
            produce_one_row: false,
            schema: Arc::new(DFSchema::empty()),
        });
        let (preds, maybe_scan) = collect_filter_chain(&empty);
        assert!(maybe_scan.is_none());
        assert!(preds.is_empty());
    }
}
