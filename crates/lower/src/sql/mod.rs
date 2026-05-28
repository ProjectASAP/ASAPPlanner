//! SQL → Layer-2 relational lowering.
//!
//! Parses SQL via DataFusion (over the catalog's registered tables), then walks
//! the unoptimized `LogicalPlan` and emits the language-independent
//! [`relational::QueryExpr`](asap_control_core::intent_algebra::relational) that
//! [`convert_root`](asap_control_core::intent_algebra::convert_root) lowers to
//! canonical L3. Positional column identity, accuracy threading, and the
//! window-over-aggregate fold all happen in that converter — this front end
//! only interprets SQL semantics into the shared L2 algebra.

use std::sync::Arc;

use datafusion::common::ScalarValue;
use datafusion::datasource::MemTable;
use datafusion::logical_expr::{
    self, Distinct, Expr, JoinType, LogicalPlan, WindowFunctionDefinition,
};
use datafusion::prelude::SessionContext;

use asap_control_core::intent_algebra::relational::{
    AggFunc, AggItem, L2ProjectItem, L2SortKey, QueryExpr as L2, SourceSpec,
};
use asap_control_core::intent_algebra::schema::Schema;
use asap_control_core::intent_algebra::{
    ColumnRef, CompareOp, JoinKind, L2Expr, L3Scalar, SetOpKind, WindowFuncKind,
};

use crate::error::LoweringError;

mod expr;
mod types;

pub use types::SqlCatalog;

use self::expr::df_expr_to_l2;
use self::types::schema_to_arrow;

/// Lowers SQL strings to the Layer-2 [`relational::QueryExpr`] over a table
/// [`SqlCatalog`]. Call [`convert_root`](asap_control_core::intent_algebra::convert_root)
/// on the result for canonical L3.
pub struct SqlLowerer<'a> {
    catalog: &'a SqlCatalog,
}

impl<'a> SqlLowerer<'a> {
    pub fn new(catalog: &'a SqlCatalog) -> Self {
        Self { catalog }
    }

    /// Parse + lower a SQL query to Layer-2 relational form.
    pub async fn lower(&self, sql: &str) -> Result<L2, LoweringError> {
        let ctx = self.build_context()?;
        let df = ctx.sql(sql).await?;
        let plan = df.into_unoptimized_plan();
        self.lower_plan(&plan)
    }

    /// Register the catalog tables (empty Arrow `MemTable`s) so DataFusion can
    /// resolve table/column references during planning.
    fn build_context(&self) -> Result<SessionContext, LoweringError> {
        let ctx = SessionContext::new();
        for (name, schema) in &self.catalog.tables {
            let arrow_schema = Arc::new(schema_to_arrow(schema));
            let mem_table = MemTable::try_new(arrow_schema, vec![])?;
            ctx.register_table(name.as_str(), Arc::new(mem_table))?;
        }
        Ok(ctx)
    }

    fn lower_plan(&self, plan: &LogicalPlan) -> Result<L2, LoweringError> {
        match plan {
            LogicalPlan::TableScan(scan) => self.lower_table_scan(scan),
            LogicalPlan::Filter(filter) => Ok(L2::Filter {
                pred: df_expr_to_l2(&filter.predicate)?,
                input: Box::new(self.lower_plan(&filter.input)?),
            }),
            LogicalPlan::Projection(proj) => self.lower_projection(proj),
            LogicalPlan::Aggregate(agg) => self.lower_aggregate(agg),
            LogicalPlan::Sort(sort) => self.lower_sort(sort),
            LogicalPlan::Limit(limit) => self.lower_limit(limit),
            LogicalPlan::Distinct(d) => match d {
                Distinct::On(_) => Err(LoweringError::UnsupportedFeature("DISTINCT ON".into())),
                Distinct::All(input) => Ok(L2::Distinct {
                    cols: vec![],
                    input: Box::new(self.lower_plan(input)?),
                }),
            },
            LogicalPlan::Union(u) => {
                // Fold n inputs left-associatively into SetOp { Union, all: true }.
                let mut iter = u.inputs.iter();
                let first = iter
                    .next()
                    .ok_or_else(|| LoweringError::InvalidExpression("empty union".into()))?;
                let first_expr = self.lower_plan(first)?;
                iter.try_fold(first_expr, |left, right_plan| {
                    Ok(L2::SetOp {
                        kind: SetOpKind::Union,
                        all: true,
                        left: Box::new(left),
                        right: Box::new(self.lower_plan(right_plan)?),
                    })
                })
            }
            LogicalPlan::Window(window) => self.lower_window(window),
            LogicalPlan::Join(join) => self.lower_join(join),
            LogicalPlan::Subquery(_) => Err(LoweringError::UnsupportedFeature("subquery".into())),
            LogicalPlan::SubqueryAlias(alias) => {
                // An alias over a table re-qualifies the scan's columns with the
                // alias (so `a.col` / `b.col` in a self-join disambiguate). A
                // derived table (inline view) is unsupported in v1.
                match alias.input.as_ref() {
                    LogicalPlan::TableScan(scan) => {
                        self.scan_source(&scan.table_name.to_string(), &alias.alias.to_string())
                    }
                    LogicalPlan::SubqueryAlias(_) => self.lower_plan(&alias.input),
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

    /// Table leaf — carries the catalog's resolved schema so the L2→L3 Binder
    /// has positional column identity. Projection pushdown is left to the
    /// enclosing `Project` (DataFusion's unoptimized plan sets no projection).
    fn lower_table_scan(&self, scan: &logical_expr::TableScan) -> Result<L2, LoweringError> {
        let table = scan.table_name.to_string();
        self.scan_source(&table, &table)
    }

    /// A `Source` over catalog table `table`, with its columns qualified by
    /// `qualifier` (the table name, or an alias from a `SubqueryAlias`) so
    /// `Qualified` column refs resolve to the right side across a join.
    fn scan_source(&self, table: &str, qualifier: &str) -> Result<L2, LoweringError> {
        let schema = self
            .catalog
            .tables
            .get(table)
            .ok_or_else(|| LoweringError::TableNotFound(table.to_string()))?;
        let qualified = Schema {
            columns: schema
                .columns
                .iter()
                .cloned()
                .map(|c| c.with_table(qualifier))
                .collect(),
            time_index: schema.time_index,
            unique_keys: schema.unique_keys.clone(),
            // Catalog-backed: the table's columns are fully declared → closed.
            closed: true,
        };
        Ok(L2::Source(SourceSpec::with_schema(
            table.to_string(),
            qualified,
        )))
    }

    /// ⋈ — equijoin. The `on` key pairs become `left = right` comparisons,
    /// AND-ed with any non-equi `filter`, into the L2 join predicate. The L2→L3
    /// converter derives the concatenated output schema; the join predicate
    /// stays name-based (like a `WHERE`). Semi/anti/mark joins have no L3
    /// counterpart yet and are rejected.
    fn lower_join(&self, join: &logical_expr::Join) -> Result<L2, LoweringError> {
        let kind = match join.join_type {
            JoinType::Inner => JoinKind::Inner,
            JoinType::Left => JoinKind::Left,
            JoinType::Right => JoinKind::Right,
            JoinType::Full => JoinKind::Full,
            other => {
                return Err(LoweringError::UnsupportedFeature(format!(
                    "join type: {other:?}"
                )))
            }
        };
        let mut conjuncts = join
            .on
            .iter()
            .map(|(l, r)| {
                Ok(L2Expr::Compare {
                    left: Box::new(df_expr_to_l2(l)?),
                    op: CompareOp::Eq,
                    right: Box::new(df_expr_to_l2(r)?),
                })
            })
            .collect::<Result<Vec<_>, LoweringError>>()?;
        if let Some(filter) = &join.filter {
            conjuncts.push(df_expr_to_l2(filter)?);
        }
        let pred = match conjuncts.len() {
            0 => None,
            1 => Some(conjuncts.pop().unwrap()),
            _ => Some(L2Expr::BoolAnd(conjuncts)),
        };
        Ok(L2::Join {
            kind,
            pred,
            left: Box::new(self.lower_plan(&join.left)?),
            right: Box::new(self.lower_plan(&join.right)?),
        })
    }

    /// `func(args) OVER (PARTITION BY … ORDER BY …)`. One window function per
    /// plan node; window frames are not modelled yet (default frame assumed).
    fn lower_window(&self, window: &logical_expr::Window) -> Result<L2, LoweringError> {
        if window.window_expr.len() > 1 {
            return Err(LoweringError::UnsupportedFeature(format!(
                "multiple window functions in one plan node (got {}); split them",
                window.window_expr.len()
            )));
        }
        let input = Box::new(self.lower_plan(&window.input)?);
        let first = window
            .window_expr
            .first()
            .ok_or_else(|| LoweringError::InvalidExpression("empty window expression".into()))?;
        let Expr::WindowFunction(wf) = first else {
            return Err(LoweringError::InvalidExpression(
                "expected a window function in Window plan node".into(),
            ));
        };
        let func = lower_window_func_kind(&wf.fun)?;
        let mut args = wf
            .args
            .iter()
            .map(df_expr_to_l2)
            .collect::<Result<Vec<_>, _>>()?;
        // Nth_value: lift N from the (literal) 2nd arg, keep only the column.
        let func = if matches!(func, WindowFuncKind::NthValue(None)) {
            let n = match args.get(1) {
                Some(L2Expr::Literal(L3Scalar::Int64(n))) if *n > 0 => *n as u64,
                other => {
                    return Err(LoweringError::InvalidExpression(format!(
                        "NTH_VALUE requires a positive integer literal 2nd arg, got {other:?}"
                    )))
                }
            };
            args.truncate(1);
            WindowFuncKind::NthValue(Some(n))
        } else {
            func
        };
        let partition_by = wf
            .partition_by
            .iter()
            .map(expr_to_group_ref)
            .collect::<Result<Vec<_>, _>>()?;
        let order_by = wf
            .order_by
            .iter()
            .map(|s| {
                df_expr_to_l2(&s.expr).map(|expr| L2SortKey {
                    expr,
                    ascending: s.asc,
                    nulls_first: s.nulls_first,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        // The window plan's schema is `[input fields …, window output]`; the last
        // field is the window column's name (what an enclosing Project references).
        let output_name = window
            .schema
            .fields()
            .last()
            .map(|f| f.name().clone())
            .unwrap_or_else(|| "window".into());
        Ok(L2::WindowFunc {
            func,
            args,
            partition_by,
            order_by,
            output_name,
            input,
        })
    }

    fn lower_projection(&self, proj: &logical_expr::Projection) -> Result<L2, LoweringError> {
        // SELECT * — no column constraint; pass through without a Project.
        if proj.expr.iter().any(|e| matches!(e, Expr::Wildcard { .. })) {
            return self.lower_plan(&proj.input);
        }
        let input = Box::new(self.lower_plan(&proj.input)?);
        let cols = proj
            .expr
            .iter()
            .map(|e| match e {
                Expr::Alias(a) => df_expr_to_l2(&a.expr).map(|expr| L2ProjectItem {
                    expr,
                    alias: Some(a.name.clone()),
                }),
                _ => df_expr_to_l2(e).map(|expr| L2ProjectItem { expr, alias: None }),
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(L2::Project { cols, input })
    }

    fn lower_aggregate(&self, agg: &logical_expr::Aggregate) -> Result<L2, LoweringError> {
        let input = Box::new(self.lower_plan(&agg.input)?);
        let keys = agg
            .group_expr
            .iter()
            .map(expr_to_group_ref)
            .collect::<Result<Vec<_>, _>>()?;
        // DataFusion names the aggregate outputs in its own schema (e.g.
        // "sum(metrics.bytes)") — the same names the enclosing Projection
        // references. The schema is [group fields …, aggregate fields …], so
        // skip the group fields and thread the rest as L2 aliases → L3
        // `Aggregate.output_names`, letting that Projection resolve them.
        let out_names: Vec<String> = agg
            .schema
            .fields()
            .iter()
            .skip(agg.group_expr.len())
            .map(|f| f.name().to_string())
            .collect();
        let aggs = agg
            .aggr_expr
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let mut item = lower_agg_item(e)?;
                if let Some(name) = out_names.get(i) {
                    item.alias = Some(name.clone());
                }
                Ok(item)
            })
            .collect::<Result<Vec<_>, LoweringError>>()?;
        Ok(L2::Aggregate {
            keys,
            aggs,
            having: None,
            input,
        })
    }

    fn lower_sort(&self, sort: &logical_expr::Sort) -> Result<L2, LoweringError> {
        // Heavy-hitter TopK only when ranking DESC by a single COUNT aggregate
        // (the frequency sketch the `TopK` intent represents). Any other
        // ranking — by a SUM/AVG/… output or a group column — keeps the real
        // Aggregate under a generic Sort+Limit so its aggregate isn't discarded.
        if let Some(k) = sort.fetch {
            if let Some(agg) = find_aggregate(strip_projections_and_aliases(&sort.input)) {
                if heavy_hitter_topk(sort, agg) {
                    return self.lower_as_topk(agg, k as u64);
                }
            }
        }
        let keys = sort
            .expr
            .iter()
            .map(|s| {
                df_expr_to_l2(&s.expr).map(|expr| L2SortKey {
                    expr,
                    ascending: s.asc,
                    nulls_first: s.nulls_first,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(L2::Sort {
            keys,
            // SQL `ORDER BY` is a global sort; per-group ranking would come from a
            // window function (`WindowFunc`), not a bare Sort.
            partition_by: vec![],
            input: Box::new(self.lower_plan(&sort.input)?),
        })
    }

    fn lower_limit(&self, limit: &logical_expr::Limit) -> Result<L2, LoweringError> {
        // Heavy-hitter TopK only for a count-ranked Limit-over-Sort-over-Aggregate
        // with no OFFSET (see `lower_sort`). Otherwise fall through to Limit+Sort.
        if let Some(k) = eval_fetch(&limit.fetch) {
            if eval_fetch(&limit.skip).unwrap_or(0) == 0 {
                if let LogicalPlan::Sort(sort) = strip_aliases(&limit.input) {
                    if let Some(agg) = find_aggregate(strip_projections_and_aliases(&sort.input)) {
                        if heavy_hitter_topk(sort, agg) {
                            return self.lower_as_topk(agg, k as u64);
                        }
                    }
                }
            }
        }
        Ok(L2::Limit {
            n: eval_fetch(&limit.fetch).unwrap_or(usize::MAX) as u64,
            offset: eval_fetch(&limit.skip).unwrap_or(0) as u64,
            input: Box::new(self.lower_plan(&limit.input)?),
        })
    }

    fn lower_as_topk(&self, agg: &logical_expr::Aggregate, k: u64) -> Result<L2, LoweringError> {
        let by = agg
            .group_expr
            .iter()
            .map(expr_to_group_ref)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(L2::TopK {
            k,
            by,
            input: Box::new(self.lower_plan(&agg.input)?),
        })
    }
}

// ── Aggregate / group-key helpers ───────────────────────────────────────────────

/// Map a DataFusion aggregate expression to a relational [`AggItem`]. The
/// L2→L3 converter resolves the input column to a positional id and applies the
/// workload accuracy target — so this only picks the `AggFunc` + input column.
fn lower_agg_item(expr: &Expr) -> Result<AggItem, LoweringError> {
    match expr {
        Expr::Alias(a) => lower_agg_item(&a.expr),
        Expr::AggregateFunction(agg_fn) => {
            let name = agg_fn.func.name().to_lowercase();
            // L3 has no DISTINCT modifier for the value reducers; only
            // COUNT(DISTINCT) maps (to Cardinality). Reject DISTINCT elsewhere
            // rather than silently lowering `SUM(DISTINCT x)` as `SUM(x)`.
            if agg_fn.distinct && name != "count" {
                return Err(LoweringError::UnsupportedAggregate(format!(
                    "DISTINCT {name}"
                )));
            }
            // Value reducers (`reducer_col`) require a real column — `SUM(a*b)`
            // is rejected, not silently reduced over a probe column.
            let (func, col) = match name.as_str() {
                "count" if agg_fn.distinct => (AggFunc::CountDistinct, agg_col_ref(&agg_fn.args)),
                "count" => (AggFunc::Count, ColumnRef::Wildcard),
                "sum" => (AggFunc::Sum, reducer_col(&name, &agg_fn.args)?),
                "min" => (AggFunc::Min, reducer_col(&name, &agg_fn.args)?),
                "max" => (AggFunc::Max, reducer_col(&name, &agg_fn.args)?),
                "avg" | "mean" => (AggFunc::Avg, reducer_col(&name, &agg_fn.args)?),
                "stddev" | "stddev_samp" => (
                    AggFunc::StdDev { population: false },
                    reducer_col(&name, &agg_fn.args)?,
                ),
                "stddev_pop" => (
                    AggFunc::StdDev { population: true },
                    reducer_col(&name, &agg_fn.args)?,
                ),
                "var" | "variance" | "var_samp" => (
                    AggFunc::Variance { population: false },
                    reducer_col(&name, &agg_fn.args)?,
                ),
                "var_pop" => (
                    AggFunc::Variance { population: true },
                    reducer_col(&name, &agg_fn.args)?,
                ),
                "approx_percentile_cont" | "percentile_cont" => (
                    AggFunc::Quantile(extract_percentile_q(&agg_fn.args)?),
                    agg_col_ref(&agg_fn.args),
                ),
                "approx_distinct" => (AggFunc::CountDistinct, agg_col_ref(&agg_fn.args)),
                _ => return Err(LoweringError::UnsupportedAggregate(name)),
            };
            Ok(AggItem {
                alias: Some(name),
                func,
                col,
            })
        }
        _ => Err(LoweringError::UnsupportedAggregate(format!("{expr:?}"))),
    }
}

/// The first aggregate argument's column name (bare / aliased / cast column),
/// or `None` for `*` / a non-column expression.
fn agg_col_name(args: &[Expr]) -> Option<String> {
    fn col_name(e: &Expr) -> Option<String> {
        match e {
            Expr::Column(c) => Some(c.name.clone()),
            Expr::Alias(a) => col_name(&a.expr),
            Expr::Cast(c) => col_name(&c.expr),
            _ => None,
        }
    }
    args.first().and_then(col_name)
}

/// The aggregated input column. `COUNT(*)` and non-column arguments yield
/// `Wildcard`; a bare/aliased/cast column yields its name.
fn agg_col_ref(args: &[Expr]) -> ColumnRef {
    match agg_col_name(args) {
        Some(name) => ColumnRef::Named(name),
        None => ColumnRef::Wildcard,
    }
}

/// The single input column of a value reducer (`SUM`/`MIN`/`MAX`/`AVG`/stddev/
/// variance). Errors if the argument is not a column: L3 reduces a column, not
/// an arbitrary expression (`SUM(a*b)`), so silently picking a probe column
/// would compute the wrong result.
fn reducer_col(name: &str, args: &[Expr]) -> Result<ColumnRef, LoweringError> {
    agg_col_name(args).map(ColumnRef::Named).ok_or_else(|| {
        LoweringError::UnsupportedAggregate(format!("{name} over a non-column expression"))
    })
}

fn expr_to_group_ref(expr: &Expr) -> Result<ColumnRef, LoweringError> {
    match expr {
        // Preserve the relation qualifier so a GROUP BY / PARTITION BY key over a
        // join (`b.k` vs `a.k`) resolves to the correct side — the same rule the
        // scalar predicate path uses (`df_expr_to_l2`).
        Expr::Column(col) => Ok(match &col.relation {
            Some(rel) => ColumnRef::Qualified {
                table: rel.to_string(),
                name: col.name.clone(),
            },
            None => ColumnRef::Named(col.name.clone()),
        }),
        Expr::Alias(a) => expr_to_group_ref(&a.expr),
        other => Err(LoweringError::UnsupportedFeature(format!(
            "non-column GROUP BY expression: {other}"
        ))),
    }
}

fn extract_percentile_q(args: &[Expr]) -> Result<f64, LoweringError> {
    let q = match args.get(1) {
        Some(Expr::Literal(ScalarValue::Float64(Some(q)))) => *q,
        Some(Expr::Literal(ScalarValue::Float32(Some(q)))) => *q as f64,
        _ => {
            return Err(LoweringError::InvalidExpression(
                "percentile value must be a float literal (2nd arg)".into(),
            ))
        }
    };
    if q.is_finite() && (0.0..=1.0).contains(&q) {
        Ok(q)
    } else {
        Err(LoweringError::InvalidExpression(format!(
            "percentile must be in [0, 1], got {q}"
        )))
    }
}

/// True iff `sort` ranks **descending by a single `COUNT` aggregate** of `agg`
/// — the only shape the heavy-hitter (frequency) `TopK` sketch is correct for.
///
/// Requires `agg` to have exactly one aggregate (a plain, non-`DISTINCT`
/// `COUNT`) and the sole sort key to reference *that* output column (not a
/// group key, a `SUM`/`AVG`/… output, or a multi-aggregate select). Anything
/// else stays a generic `Sort` + `Limit` over the real `Aggregate`, mirroring
/// the PromQL gate (`topk` is heavy-hitter only over `count_over_time`).
fn heavy_hitter_topk(sort: &logical_expr::Sort, agg: &logical_expr::Aggregate) -> bool {
    let [key] = sort.expr.as_slice() else {
        return false;
    };
    if key.asc {
        return false;
    }
    if agg.aggr_expr.len() != 1 || !is_count_aggregate(&agg.aggr_expr[0]) {
        return false;
    }
    // The DESC key must rank by the count's output column, not a group key. The
    // aggregate schema is `[group fields …, aggregate fields …]`, so the single
    // count output sits at index `group_expr.len()`.
    let count_name = agg
        .schema
        .fields()
        .get(agg.group_expr.len())
        .map(|f| f.name().clone());
    column_name(&key.expr) == count_name
}

/// The referenced column name of a bare/aliased column expression, else `None`.
fn column_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Column(c) => Some(c.name.clone()),
        Expr::Alias(a) => column_name(&a.expr),
        _ => None,
    }
}

/// Whether `expr` is a plain (non-`DISTINCT`) `COUNT` aggregate.
fn is_count_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Alias(a) => is_count_aggregate(&a.expr),
        Expr::AggregateFunction(f) => f.func.name().eq_ignore_ascii_case("count") && !f.distinct,
        _ => false,
    }
}

// ── LogicalPlan navigation helpers ──────────────────────────────────────────────

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

/// Strip Projection + SubqueryAlias for TopK pattern-matching only.
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

/// Map a DataFusion window-function definition to the L3 [`WindowFuncKind`].
/// `NthValue` is returned with `None`; `lower_window` fills in `n` from args.
fn lower_window_func_kind(fun: &WindowFunctionDefinition) -> Result<WindowFuncKind, LoweringError> {
    let unsupported = |what: &str, name: &str| {
        LoweringError::UnsupportedFeature(format!("window {what}: {name}"))
    };
    match fun {
        WindowFunctionDefinition::WindowUDF(udf) => match udf.name().to_lowercase().as_str() {
            "row_number" => Ok(WindowFuncKind::RowNumber),
            "rank" => Ok(WindowFuncKind::Rank),
            "dense_rank" => Ok(WindowFuncKind::DenseRank),
            "lag" => Ok(WindowFuncKind::Lag),
            "lead" => Ok(WindowFuncKind::Lead),
            "first_value" => Ok(WindowFuncKind::FirstValue),
            "last_value" => Ok(WindowFuncKind::LastValue),
            "nth_value" => Ok(WindowFuncKind::NthValue(None)),
            other => Err(unsupported("function", other)),
        },
        WindowFunctionDefinition::AggregateUDF(udf) => match udf.name().to_lowercase().as_str() {
            "sum" => Ok(WindowFuncKind::Sum),
            "avg" | "mean" => Ok(WindowFuncKind::Avg),
            "count" => Ok(WindowFuncKind::Count),
            "min" => Ok(WindowFuncKind::Min),
            "max" => Ok(WindowFuncKind::Max),
            other => Err(unsupported("aggregate", other)),
        },
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
