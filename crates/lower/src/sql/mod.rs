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
use datafusion::logical_expr::{self, Distinct, Expr, LogicalPlan};
use datafusion::prelude::SessionContext;

use asap_control_core::intent_algebra::relational::{
    AggFunc, AggItem, QueryExpr as L2, SourceSpec,
};
use asap_control_core::intent_algebra::{ColumnRef, ProjectItem, SetOpKind, SortKey};

use crate::error::LoweringError;

mod expr;
mod types;

pub use types::SqlCatalog;

use self::expr::df_expr_to_l3;
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
                pred: df_expr_to_l3(&filter.predicate)?,
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
            LogicalPlan::Window(_) => Err(LoweringError::UnsupportedFeature(
                "SQL window functions (no L3 analytic-window node yet)".into(),
            )),
            LogicalPlan::Join(_) => Err(LoweringError::UnsupportedFeature("JOIN".into())),
            LogicalPlan::Subquery(_) => Err(LoweringError::UnsupportedFeature("subquery".into())),
            LogicalPlan::SubqueryAlias(alias) => {
                // A bare table alias is transparent; a derived table (inline view)
                // is unsupported in v1.
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

    /// Table leaf — carries the catalog's resolved schema so the L2→L3 Binder
    /// has positional column identity. Projection pushdown is left to the
    /// enclosing `Project` (DataFusion's unoptimized plan sets no projection).
    fn lower_table_scan(&self, scan: &logical_expr::TableScan) -> Result<L2, LoweringError> {
        let table_name = scan.table_name.to_string();
        let schema = self
            .catalog
            .tables
            .get(&table_name)
            .ok_or_else(|| LoweringError::TableNotFound(table_name.clone()))?;
        Ok(L2::Source(SourceSpec::with_schema(
            table_name,
            schema.clone(),
        )))
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
                Expr::Alias(a) => df_expr_to_l3(&a.expr).map(|expr| ProjectItem {
                    expr,
                    alias: Some(a.name.clone()),
                }),
                _ => df_expr_to_l3(e).map(|expr| ProjectItem { expr, alias: None }),
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(L2::Project { cols, input })
    }

    fn lower_aggregate(&self, agg: &logical_expr::Aggregate) -> Result<L2, LoweringError> {
        let input = Box::new(self.lower_plan(&agg.input)?);
        let keys = agg
            .group_expr
            .iter()
            .map(expr_to_group_name)
            .collect::<Result<Vec<_>, _>>()?;
        let aggs = agg
            .aggr_expr
            .iter()
            .map(lower_agg_item)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(L2::Aggregate {
            keys,
            aggs,
            having: None,
            input,
        })
    }

    fn lower_sort(&self, sort: &logical_expr::Sort) -> Result<L2, LoweringError> {
        // TopK: Sort with a folded LIMIT, all keys descending, over an Aggregate.
        if let Some(k) = sort.fetch {
            if sort.expr.iter().all(|s| !s.asc) {
                if let Some(agg) = find_aggregate(strip_projections_and_aliases(&sort.input)) {
                    return self.lower_as_topk(agg, k as u64);
                }
            }
        }
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
        Ok(L2::Sort {
            keys,
            input: Box::new(self.lower_plan(&sort.input)?),
        })
    }

    fn lower_limit(&self, limit: &logical_expr::Limit) -> Result<L2, LoweringError> {
        // TopK: Limit over Sort over Aggregate, all sort keys DESC, no OFFSET.
        if let Some(k) = eval_fetch(&limit.fetch) {
            if eval_fetch(&limit.skip).unwrap_or(0) == 0 {
                if let LogicalPlan::Sort(sort) = strip_aliases(&limit.input) {
                    if sort.expr.iter().all(|s| !s.asc) {
                        if let Some(agg) =
                            find_aggregate(strip_projections_and_aliases(&sort.input))
                        {
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
            .map(expr_to_group_name)
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
            let (func, col) = match name.as_str() {
                "count" if agg_fn.distinct => (AggFunc::CountDistinct, agg_col_ref(&agg_fn.args)),
                "count" => (AggFunc::Count, ColumnRef::Wildcard),
                "sum" => (AggFunc::Sum, agg_col_ref(&agg_fn.args)),
                "min" => (AggFunc::Min, agg_col_ref(&agg_fn.args)),
                "max" => (AggFunc::Max, agg_col_ref(&agg_fn.args)),
                "avg" | "mean" => (AggFunc::Avg, agg_col_ref(&agg_fn.args)),
                "stddev" | "stddev_samp" => (
                    AggFunc::StdDev { population: false },
                    agg_col_ref(&agg_fn.args),
                ),
                "stddev_pop" => (
                    AggFunc::StdDev { population: true },
                    agg_col_ref(&agg_fn.args),
                ),
                "var" | "variance" | "var_samp" => (
                    AggFunc::Variance { population: false },
                    agg_col_ref(&agg_fn.args),
                ),
                "var_pop" => (
                    AggFunc::Variance { population: true },
                    agg_col_ref(&agg_fn.args),
                ),
                "approx_percentile_cont" | "percentile_cont" => (
                    AggFunc::Quantile(extract_percentile_q(&agg_fn.args)?),
                    agg_col_ref(&agg_fn.args),
                ),
                "approx_distinct" => (AggFunc::CountDistinct, agg_col_ref(&agg_fn.args)),
                _ => return Err(LoweringError::UnsupportedAggregate(name)),
            };
            Ok(AggItem {
                alias: name,
                func,
                col,
                distinct: agg_fn.distinct,
            })
        }
        _ => Err(LoweringError::UnsupportedAggregate(format!("{expr:?}"))),
    }
}

/// The aggregated input column. `COUNT(*)` and non-column arguments yield
/// `Wildcard`; a bare/aliased/cast column yields its name.
fn agg_col_ref(args: &[Expr]) -> ColumnRef {
    fn col_name(e: &Expr) -> Option<String> {
        match e {
            Expr::Column(c) => Some(c.name.clone()),
            Expr::Alias(a) => col_name(&a.expr),
            Expr::Cast(c) => col_name(&c.expr),
            _ => None,
        }
    }
    match args.first().and_then(col_name) {
        Some(name) => ColumnRef::Named(name),
        None => ColumnRef::Wildcard,
    }
}

fn expr_to_group_name(expr: &Expr) -> Result<String, LoweringError> {
    match expr {
        Expr::Column(col) => Ok(col.name.clone()),
        Expr::Alias(a) => expr_to_group_name(&a.expr),
        other => Err(LoweringError::UnsupportedFeature(format!(
            "non-column GROUP BY expression: {other}"
        ))),
    }
}

fn extract_percentile_q(args: &[Expr]) -> Result<f64, LoweringError> {
    match args.get(1) {
        Some(Expr::Literal(ScalarValue::Float64(Some(q)))) => Ok(*q),
        Some(Expr::Literal(ScalarValue::Float32(Some(q)))) => Ok(*q as f64),
        _ => Err(LoweringError::InvalidExpression(
            "percentile value must be a float literal (2nd arg)".into(),
        )),
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
